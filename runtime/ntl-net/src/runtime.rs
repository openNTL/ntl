//! The node runtime: transport, peer sessions, and the learning loop.
//!
//! This is the async half of an NTL node. [`ntl_core::Node`] decides *what*
//! should happen to a signal; this drives the I/O that makes it happen, and
//! the periodic work the learning model needs.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ntl_core::crypto::{ClassicalModule, PublicKey};
use ntl_core::delivery::Receipt;
use ntl_core::signal::{NodeId, Signal, SignalBuilder, SignalType};
use ntl_core::{Node, NodeConfig, NodeStore};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, RwLock, mpsc};

use crate::frame::{self, FrameError};
use crate::identity::Identity;

/// How often the runtime resolves timed-out decisions and checkpoints.
const MAINTENANCE_INTERVAL_SECS: u64 = 5;

/// How often the activation queue is polled for starving signals.
///
/// Must be well under the smallest `max_queue_latency_ms` any node class
/// configures, or the guard it implements is toothless.
const ACTIVATION_POLL_INTERVAL_MS: u64 = 50;

/// Outbound capacity per peer.
///
/// Bounded on purpose: an unbounded channel converts a slow peer into
/// unbounded memory growth on this node.
const PEER_QUEUE_DEPTH: usize = 256;

/// Events a runtime surfaces to its host.
#[derive(Debug, Clone)]
pub enum Event {
    /// A signal was handled locally.
    Handled {
        /// The signal.
        signal: Box<Signal>,
    },
    /// A signal was forwarded onward.
    Forwarded {
        /// Which signal.
        signal_id: ntl_core::SignalId,
        /// How many peers it went to.
        peers: usize,
        /// Whether any choice was exploratory.
        explored: bool,
    },
    /// A signal was released by the activation latency guard rather than by
    /// crossing the threshold.
    ///
    /// Carries the body where the runtime still holds it, so `ntl listen` can
    /// show a released signal as fully as a fired one.
    Released {
        /// Which signal.
        signal_id: ntl_core::SignalId,
        /// The body, if still cached.
        signal: Option<Box<Signal>>,
    },
    /// A signal was refused.
    Refused {
        /// Which signal.
        signal_id: ntl_core::SignalId,
        /// Why.
        reason: ntl_core::RejectReason,
    },
    /// A receipt resolved one of our decisions.
    ReceiptApplied {
        /// The peer that sent it.
        peer: NodeId,
        /// Whether it reported success.
        delivered: bool,
        /// The weight change it caused.
        weight_delta: f32,
    },
    /// A peer connected.
    PeerConnected {
        /// Their identity, once known.
        peer: NodeId,
    },
    /// A peer disconnected.
    PeerDisconnected {
        /// Their identity.
        peer: NodeId,
    },
    /// A signal failed signature verification.
    ///
    /// Either an attack or a serious implementation defect; both warrant
    /// operator attention.
    SignatureFailed {
        /// The peer that presented it.
        peer: NodeId,
        /// Whether this failure crossed the threshold and pruned the synapse.
        pruned: bool,
    },
    /// An arriving signal's mutable headers exceeded this node's bounds and
    /// were clamped.
    ///
    /// Worth surfacing rather than fixing silently: `weight` and `ttl` sit
    /// outside the origin signature, so a clamp means the peer that handed
    /// this over inflated them — which is a peer trying to win traffic or
    /// extend a signal's life beyond what its origin asked for.
    HeadersClamped {
        /// The peer that handed it over.
        peer: NodeId,
        /// Which signal.
        signal_id: ntl_core::SignalId,
        /// Whether the weight was clamped.
        weight: bool,
        /// Whether the TTL was clamped.
        ttl: bool,
    },
    /// A signal was dropped because no public key could be resolved for its
    /// claimed origin, so its signature could not be checked at all.
    ///
    /// Distinct from [`Self::SignatureFailed`]: that is a signature that did
    /// not verify, this is one that could not be verified. The peer that
    /// relayed it is not necessarily at fault, so no synapse is penalised —
    /// but an operator seeing a steady stream of these is looking at a node
    /// whose neighbours are relaying traffic it has no way to authenticate.
    OriginKeyUnknown {
        /// The peer that relayed it.
        peer: NodeId,
        /// The origin it claimed.
        origin: NodeId,
    },
    /// A signal verified but was structurally invalid.
    Malformed {
        /// The peer that presented it.
        peer: NodeId,
        /// Why it was rejected.
        reason: String,
    },
}

/// A connected peer.
struct Session {
    outbound: mpsc::Sender<Signal>,
    /// Retained so a relayed signal originating from this peer can be
    /// verified even when it arrives over a different session.
    public_key: PublicKey,
}

impl Session {
    /// The peer's verification key.
    fn public_key(&self) -> &PublicKey {
        &self.public_key
    }
}

/// Configuration for a runtime.
pub struct RuntimeConfig {
    /// Address to listen on.
    pub bind: SocketAddr,
    /// Peers to dial at startup.
    pub bootstrap: Vec<SocketAddr>,
    /// Node configuration.
    pub node: NodeConfig,
}

/// A running NTL node.
/// Bodies of signals currently waiting in the activation queue.
///
/// The activation gate holds only what it needs to schedule
/// ([`ntl_core::activation::QueuedSignal`]); the body lives here so a signal
/// released by the latency guard can still be shown and handled in full.
/// Bounded by the queue depth, so a flood cannot grow it without bound.
type PendingBodies = Arc<Mutex<HashMap<ntl_core::SignalId, Signal>>>;

pub struct Runtime {
    node: Arc<Node>,
    identity: Arc<Identity>,
    sessions: Arc<RwLock<HashMap<NodeId, Session>>>,
    pending: PendingBodies,
    events: mpsc::Sender<Event>,
    bind: SocketAddr,
}

impl Runtime {
    /// Build a runtime over a store and configuration.
    ///
    /// # Errors
    /// Returns an error if identity cannot be established or the node cannot
    /// be built.
    pub fn new(
        store: Arc<dyn NodeStore>,
        config: RuntimeConfig,
    ) -> Result<(Self, mpsc::Receiver<Event>), RuntimeError> {
        store
            .migrate()
            .map_err(|e| RuntimeError::Store(e.to_string()))?;

        let identity = Identity::load_or_create(store.as_ref())
            .map_err(|e| RuntimeError::Identity(e.to_string()))?;

        let node = Node::builder()
            .with_config(config.node)
            .with_identity(identity.node_id.clone())
            .with_store(store)
            // Not the default `SplitMix64`. That generator supplies signal
            // identifiers as well as exploration draws, it is seeded from the
            // node id — which is public, being in every signal's `origin` —
            // and its output function is invertible, so one observed
            // identifier recovers the state and predicts every later one.
            // `rng.rs` says outright that it must not be used for identifiers
            // that need unpredictability. See `csprng` for the attack that
            // buys.
            .with_rng(Box::new(crate::csprng::OsBackedRng::from_os()))
            .build()
            .map_err(|e| RuntimeError::Node(e.to_string()))?;

        let (events, receiver) = mpsc::channel(1_024);
        Ok((
            Self {
                node: Arc::new(node),
                identity: Arc::new(identity),
                sessions: Arc::new(RwLock::new(HashMap::new())),
                pending: Arc::new(Mutex::new(HashMap::new())),
                events,
                bind: config.bind,
            },
            receiver,
        ))
    }

    /// The underlying node.
    #[must_use]
    pub fn node(&self) -> &Arc<Node> {
        &self.node
    }

    /// This node's identity.
    #[must_use]
    pub fn identity(&self) -> &Arc<Identity> {
        &self.identity
    }

    /// Peers currently connected.
    pub async fn connected_peers(&self) -> Vec<NodeId> {
        self.sessions.read().await.keys().cloned().collect()
    }

    /// Wait until at least `count` peers have completed their handshake.
    ///
    /// Returns the number connected when it returned, which may be fewer than
    /// `count` if the timeout elapsed first.
    ///
    /// A caller that dials and then immediately routes will find an empty
    /// topology: `dial` returns once the TCP connection is open, but a peer is
    /// not usable until the signed Discovery exchange has completed and its
    /// synapse is in the store. Sleeping a fixed interval instead of waiting
    /// for the event is a race — it passed locally and failed on a slower CI
    /// runner, where 200ms was not enough.
    pub async fn wait_for_peers(&self, count: usize, timeout: Duration) -> usize {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let connected = self.sessions.read().await.len();
            if connected >= count {
                return connected;
            }
            if tokio::time::Instant::now() >= deadline {
                return connected;
            }
            // Polling rather than subscribing: the event channel is consumed by
            // the caller, so taking events here would steal them.
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Start listening, and return the bound address.
    ///
    /// Returning the real address matters when binding to port 0, which is how
    /// tests avoid fighting over ports.
    ///
    /// # Errors
    /// Returns an error if the address cannot be bound.
    pub async fn listen(&self) -> Result<(SocketAddr, tokio::task::JoinHandle<()>), RuntimeError> {
        let listener = TcpListener::bind(self.bind)
            .await
            .map_err(|e| RuntimeError::Bind(format!("{}: {e}", self.bind)))?;
        let local = listener
            .local_addr()
            .map_err(|e| RuntimeError::Bind(e.to_string()))?;

        let node = self.node.clone();
        let identity = self.identity.clone();
        let sessions = self.sessions.clone();
        let pending = self.pending.clone();
        let events = self.events.clone();

        let handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, addr)) => {
                        tracing::debug!(%addr, "inbound connection");
                        spawn_session(
                            stream,
                            node.clone(),
                            identity.clone(),
                            sessions.clone(),
                            pending.clone(),
                            events.clone(),
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "accept failed");
                        // A failed accept is usually transient (fd pressure).
                        // Yield rather than spinning on it.
                        tokio::task::yield_now().await;
                    }
                }
            }
        });
        Ok((local, handle))
    }

    /// Dial a peer and form a synapse.
    ///
    /// # Errors
    /// Returns an error if the connection cannot be established.
    pub async fn dial(&self, addr: SocketAddr) -> Result<(), RuntimeError> {
        let stream = TcpStream::connect(addr)
            .await
            .map_err(|e| RuntimeError::Dial(format!("{addr}: {e}")))?;
        spawn_session(
            stream,
            self.node.clone(),
            self.identity.clone(),
            self.sessions.clone(),
            self.pending.clone(),
            self.events.clone(),
        );
        Ok(())
    }

    /// Emit a signal into the network.
    ///
    /// The signal is signed, journalled, and queued to every selected peer.
    /// Returns the signal as emitted.
    ///
    /// # Errors
    /// Returns an error if signing or routing fails.
    pub async fn emit(&self, builder: SignalBuilder) -> Result<Signal, RuntimeError> {
        let mut signal = self
            .node
            .emit(builder)
            .map_err(|e| RuntimeError::Node(e.to_string()))?;
        ntl_core::crypto::sign_signal(&ClassicalModule, &mut signal, &self.identity.private)
            .map_err(|e| RuntimeError::Crypto(e.to_string()))?;

        self.route(&signal).await?;
        Ok(signal)
    }

    /// Route an already-signed signal to selected peers.
    async fn route(&self, signal: &Signal) -> Result<(), RuntimeError> {
        // A locally emitted signal has no arrival synapse, so plan_forwarding
        // considers the whole topology.
        let disposition = self
            .node
            .receive_local(signal)
            .map_err(|e| RuntimeError::Node(e.to_string()))?;

        let explored = disposition.forward_to.iter().any(|f| f.explored);
        let mut sent = 0;
        let sessions = self.sessions.read().await;
        for forward in &disposition.forward_to {
            if let Some(session) = sessions.get(&forward.peer) {
                let mut outbound = signal.clone();
                outbound.hop(self.identity.node_id.clone());
                outbound.attenuate_for_hop(
                    self.node.config().propagation.attenuation_factor,
                    self.node.config().propagation.min_propagation_weight,
                );
                // try_send, not send: a full queue means a peer we cannot keep
                // up with, and blocking here would let one slow peer stall the
                // whole node.
                if session.outbound.try_send(outbound).is_ok() {
                    sent += 1;
                } else {
                    tracing::warn!(peer = %forward.peer, "outbound queue full; dropping");
                }
            }
        }
        drop(sessions);

        let _ = self
            .events
            .send(Event::Forwarded {
                signal_id: signal.id,
                peers: sent,
                explored,
            })
            .await;
        Ok(())
    }

    /// Run periodic maintenance: resolve timeouts, checkpoint, purge.
    ///
    /// The timeout sweep is not optional housekeeping. It is what converts
    /// silence into a training signal: without it, a path that never delivers
    /// stays `pending` forever and looks identical to one never tried.
    #[must_use]
    pub fn spawn_maintenance(&self) -> tokio::task::JoinHandle<()> {
        let node = self.node.clone();
        let identity = self.identity.clone();
        let sessions = self.sessions.clone();
        let pending = self.pending.clone();
        let events = self.events.clone();

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(
                ACTIVATION_POLL_INTERVAL_MS,
            ));
            let mut ticks: u64 = 0;

            loop {
                ticker.tick().await;
                ticks += 1;

                // Release anything the activation queue has held too long.
                // Without this a below-threshold signal waits for the next
                // arrival that may never come, and its sender penalises a
                // working path.
                match node.poll_activation() {
                    Ok(released) => {
                        for queued in released {
                            // The same path the arrival case takes.
                            // Previously this emitted an event and a
                            // *positive* receipt without forwarding, so a
                            // relay node reported delivery for a signal it
                            // then dropped.
                            let body = pending.lock().await.remove(&queued.id);
                            release_signal(
                                &node,
                                &identity,
                                &sessions,
                                &events,
                                queued.id,
                                body,
                                None,
                                Release::Guard,
                            )
                            .await;
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "activation poll failed"),
                }

                // The heavier work runs less often.
                let per_sweep = (MAINTENANCE_INTERVAL_SECS * 1_000) / ACTIVATION_POLL_INTERVAL_MS;
                if ticks % per_sweep.max(1) != 0 {
                    continue;
                }
                match node.sweep_timeouts(256) {
                    Ok(n) if n > 0 => tracing::debug!(resolved = n, "timed out decisions"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "timeout sweep failed"),
                }
                if let Err(e) = node.checkpoint() {
                    tracing::warn!(error = %e, "checkpoint failed");
                }
            }
        })
    }
}

/// Drive one peer connection.
fn spawn_session(
    stream: TcpStream,
    node: Arc<Node>,
    identity: Arc<Identity>,
    sessions: Arc<RwLock<HashMap<NodeId, Session>>>,
    pending: PendingBodies,
    events: mpsc::Sender<Event>,
) {
    tokio::spawn(async move {
        if let Err(e) = run_session(stream, node, identity, sessions, pending, events).await {
            tracing::debug!(error = %e, "session ended");
        }
    });
}

async fn run_session(
    stream: TcpStream,
    node: Arc<Node>,
    identity: Arc<Identity>,
    sessions: Arc<RwLock<HashMap<NodeId, Session>>>,
    pending: PendingBodies,
    events: mpsc::Sender<Event>,
) -> Result<(), RuntimeError> {
    let _ = stream.set_nodelay(true);
    let (read_half, write_half) = stream.into_split();
    let reader = Arc::new(Mutex::new(read_half));
    let writer = Arc::new(Mutex::new(write_half));

    // Handshake: exchange Discovery signals carrying public keys, so each side
    // can verify the other's signatures afterwards.
    let peer = handshake(&reader, &writer, &node, &identity).await?;

    let (tx, mut rx) = mpsc::channel::<Signal>(PEER_QUEUE_DEPTH);
    {
        // Refuse rather than replace. `insert` returned the old `Sender` and
        // dropped it, which tore down the existing session's writer pump — so a
        // second connection claiming an identity silently displaced the first,
        // and every signal routed to that peer went to the newcomer instead.
        // With authentication now proven per connection this is no longer an
        // impersonation route, but "last connection wins" is still the wrong
        // default: a live session is working, and a reconnect that races a
        // half-closed socket should not cost the traffic in flight.
        let mut guard = sessions.write().await;
        if let Some(existing) = guard.get(&peer.node_id) {
            if !existing.outbound.is_closed() {
                return Err(RuntimeError::Handshake(format!(
                    "{} already has a live session; refusing the duplicate",
                    peer.node_id
                )));
            }
        }
        guard.insert(
            peer.node_id.clone(),
            Session {
                outbound: tx,
                public_key: peer.public_key.clone(),
            },
        );
    }
    let _ = events
        .send(Event::PeerConnected {
            peer: peer.node_id.clone(),
        })
        .await;

    // Writer pump.
    let write_task = {
        let writer = writer.clone();
        tokio::spawn(async move {
            while let Some(signal) = rx.recv().await {
                let mut guard = writer.lock().await;
                if frame::write_signal(&mut *guard, &signal).await.is_err() {
                    break;
                }
            }
        })
    };

    // Reader loop.
    let result = read_loop(
        &reader, &node, &identity, &sessions, &pending, &events, &peer,
    )
    .await;

    sessions.write().await.remove(&peer.node_id);
    write_task.abort();
    let _ = events
        .send(Event::PeerDisconnected {
            peer: peer.node_id.clone(),
        })
        .await;
    result
}

/// A peer's verified identity.
#[derive(Clone)]
struct PeerIdentity {
    node_id: NodeId,
    public_key: PublicKey,
}

/// Bytes of challenge each side sends. 256 bits, from the OS CSPRNG.
const CHALLENGE_LEN: usize = 32;

/// How long the whole exchange may take.
///
/// [synapse-lifecycle](https://openntl.org/spec/synapse-lifecycle) requires the
/// handshake to complete within 30 seconds or be abandoned. Without this a peer
/// that connects and then says nothing holds a task and a socket indefinitely.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Read one framed signal, or time out.
async fn read_handshake_frame(
    reader: &Arc<Mutex<tokio::net::tcp::OwnedReadHalf>>,
    what: &str,
) -> Result<Signal, RuntimeError> {
    let read = async {
        let mut guard = reader.lock().await;
        frame::read_signal(&mut *guard)
            .await
            .map_err(RuntimeError::Frame)
    };
    tokio::time::timeout(HANDSHAKE_TIMEOUT, read)
        .await
        .map_err(|_| RuntimeError::Handshake(format!("timed out waiting for the peer's {what}")))?
}

/// Sign and send one framed signal.
async fn write_handshake_frame(
    writer: &Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>,
    node: &Arc<Node>,
    identity: &Arc<Identity>,
    payload: serde_json::Value,
) -> Result<(), RuntimeError> {
    let mut signal = node
        .emit(Signal::discovery().with_payload(payload))
        .map_err(|e| RuntimeError::Node(e.to_string()))?;
    ntl_core::crypto::sign_signal(&ClassicalModule, &mut signal, &identity.private)
        .map_err(|e| RuntimeError::Crypto(e.to_string()))?;
    let mut guard = writer.lock().await;
    frame::write_signal(&mut *guard, &signal)
        .await
        .map_err(RuntimeError::Frame)
}

/// Read a byte string out of a handshake payload.
fn payload_bytes(signal: &Signal, field: &str) -> Result<Vec<u8>, RuntimeError> {
    signal
        .payload
        .get(field)
        .and_then(|v| serde_json::from_value::<Vec<u8>>(v.clone()).ok())
        .ok_or_else(|| RuntimeError::Handshake(format!("handshake has no {field}")))
}

/// Check a handshake frame is a signed Discovery from `expected`.
fn verify_handshake_frame(
    signal: &Signal,
    key: &PublicKey,
    expected: &NodeId,
    what: &str,
) -> Result<(), RuntimeError> {
    if signal.signal_type != SignalType::Discovery {
        return Err(RuntimeError::Handshake(format!(
            "expected a Discovery signal for the {what}, got {:?}",
            signal.signal_type
        )));
    }
    if signal.origin != *expected {
        return Err(RuntimeError::Handshake(format!(
            "the {what} came from a different identity than the hello"
        )));
    }
    if !ntl_core::crypto::verify_signal(&ClassicalModule, signal, key)
        .map_err(|e| RuntimeError::Crypto(e.to_string()))?
    {
        return Err(RuntimeError::Handshake(format!(
            "the {what} signature did not verify"
        )));
    }
    Ok(())
}

/// Authenticate a peer over this connection.
///
/// Three messages, mutually: both sides send a signed hello carrying their
/// public key and a fresh random challenge, then both send a signed proof whose
/// payload echoes the *peer's* challenge. This matches the SYN / SYN-ACK / ACK
/// exchange [synapse-lifecycle](https://openntl.org/spec/synapse-lifecycle)
/// specifies, and the proof is what makes it an authentication rather than a
/// bearer token.
///
/// The earlier version verified only the hello, which was self-contained,
/// non-expiring and replayable — so it proved that *someone* had once held the
/// private key, not that the party on this socket holds it now. Worse, a node
/// writes its hello before reading the peer's, so anyone able to open a TCP
/// connection could collect a node's valid signed hello and replay it verbatim
/// elsewhere. Because a session is keyed on the claimed `NodeId` and
/// `upsert_synapse` returns the existing record, a replayer would have
/// displaced the genuine peer's session and inherited its learned weight, and
/// could then have had signature-failure penalties charged to that peer's
/// synapse — which threat-model §4 promises is impossible.
///
/// Harvesting a hello is now harmless: it carries a challenge the harvester
/// cannot make anyone answer, and answering a fresh one needs the private key.
async fn handshake(
    reader: &Arc<Mutex<tokio::net::tcp::OwnedReadHalf>>,
    writer: &Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>,
    node: &Arc<Node>,
    identity: &Arc<Identity>,
) -> Result<PeerIdentity, RuntimeError> {
    // Our challenge. From the OS CSPRNG, not the node's sampling generator:
    // a predictable challenge is a replayable one.
    let mut challenge = [0u8; CHALLENGE_LEN];
    {
        use rand::RngCore as _;
        rand::rngs::OsRng.fill_bytes(&mut challenge);
    }

    // SYN. Both sides send without waiting, so neither blocks on the other.
    write_handshake_frame(
        writer,
        node,
        identity,
        serde_json::json!({
            "public_key": identity.public.0,
            "module": ClassicalModule::ID,
            "challenge": challenge,
        }),
    )
    .await?;

    let their_hello = read_handshake_frame(reader, "hello").await?;

    let public_key = PublicKey(payload_bytes(&their_hello, "public_key")?);

    // The claimed identity must match the key that signed the handshake.
    // Without this check a peer could claim any identity it liked, and
    // per-identity influence caps would be meaningless.
    let derived = ntl_core::crypto::node_id_from_public_key(&public_key);
    if derived != their_hello.origin {
        return Err(RuntimeError::Handshake(
            "claimed identity does not match the handshake public key".to_string(),
        ));
    }
    verify_handshake_frame(&their_hello, &public_key, &derived, "hello")?;

    // A node must not form a synapse with itself. Reflecting a node's own
    // hello back at it otherwise passed every check — the identity binding
    // holds and the signature is genuine — and left the node with a session
    // and an active synapse under its own identity, routing its own traffic to
    // whoever held the socket.
    if derived == identity.node_id {
        return Err(RuntimeError::Handshake(
            "the peer presented this node's own identity".to_string(),
        ));
    }

    let their_challenge = payload_bytes(&their_hello, "challenge")?;
    if their_challenge.len() != CHALLENGE_LEN {
        return Err(RuntimeError::Handshake(format!(
            "challenge is {} bytes, expected {CHALLENGE_LEN}",
            their_challenge.len()
        )));
    }

    // ACK. Signing a payload that contains the peer's fresh random challenge
    // proves possession of the private key *now*, over this connection.
    write_handshake_frame(
        writer,
        node,
        identity,
        serde_json::json!({ "proof_for": their_challenge }),
    )
    .await?;

    let their_proof = read_handshake_frame(reader, "proof").await?;
    verify_handshake_frame(&their_proof, &public_key, &derived, "proof")?;

    let echoed = payload_bytes(&their_proof, "proof_for")?;
    // Constant time is not required — a challenge is public once sent, and the
    // secret being proven is the signing key, not the challenge — but the
    // comparison must be exact.
    if echoed != challenge {
        return Err(RuntimeError::Handshake(
            "the peer's proof did not echo our challenge, so it did not \
             demonstrate possession of the key it claimed"
                .to_string(),
        ));
    }

    node.upsert_synapse(&derived)
        .map_err(|e| RuntimeError::Node(e.to_string()))?;

    Ok(PeerIdentity {
        node_id: derived,
        public_key,
    })
}

async fn read_loop(
    reader: &Arc<Mutex<tokio::net::tcp::OwnedReadHalf>>,
    node: &Arc<Node>,
    identity: &Arc<Identity>,
    sessions: &Arc<RwLock<HashMap<NodeId, Session>>>,
    pending: &PendingBodies,
    events: &mpsc::Sender<Event>,
    peer: &PeerIdentity,
) -> Result<(), RuntimeError> {
    loop {
        let mut signal = {
            let mut guard = reader.lock().await;
            match frame::read_signal(&mut *guard).await {
                Ok(s) => s,
                Err(FrameError::Closed) => return Ok(()),
                Err(e) => return Err(RuntimeError::Frame(e)),
            }
        };

        // Verification comes after framing and before any routing work.
        //
        // This is *not* the order Propagation Rule 5 states. Rule 5 asks for
        // the cheap checks — size, TTL, dedup — to run first, so that an
        // attacker cannot force signature work with malformed traffic. Size is
        // enforced in `frame::read_signal`, before any allocation. TTL and
        // dedup are not, and deliberately:
        //
        //   - Dedup has a side effect. `check_and_set_seen` *claims* the id,
        //     so running it before verification would let unauthenticated
        //     traffic occupy the dedup cache and suppress the genuine signal
        //     that follows. That is a cheaper attack than the one Rule 5's
        //     ordering defends against.
        //   - TTL exhaustion owes an acknowledged sender a receipt. Emitting
        //     one for a signal we have not verified means an attacker chooses
        //     when this node emits traffic and to whom.
        //
        // An Ed25519 verification is tens of microseconds against a frame that
        // has already been read off the socket, so the work Rule 5 is trying to
        // avoid is small next to either of those. Raised on the PR as a spec
        // amendment rather than settled here.
        //
        // The origin's key is known for a direct peer, and also for a relayed
        // signal whose origin we happen to have a session with.
        let origin_key = if signal.origin == peer.node_id {
            Some(peer.public_key.clone())
        } else {
            sessions
                .read()
                .await
                .get(&signal.origin)
                .map(|s| s.public_key().clone())
        };

        // An origin we cannot resolve a key for is a drop, not a pass. Letting
        // it through would mean any connected peer could inject traffic under
        // any identity it chose, which is precisely what the handshake's
        // identity binding and the per-identity influence caps exist to
        // prevent — and the incentive gradient would point the wrong way,
        // since forging from a known origin costs a synapse penalty while
        // forging from an unknown one would cost nothing.
        //
        // The price is that verified delivery is one hop today: a signal
        // relayed from a node we have never met has no resolvable key. Fixing
        // that needs key distribution, which is a wire-format question rather
        // than a bug — see threat-model §9.
        let Some(key) = origin_key else {
            let _ = events
                .send(Event::OriginKeyUnknown {
                    peer: peer.node_id.clone(),
                    origin: signal.origin.clone(),
                })
                .await;
            continue;
        };

        let ok = ntl_core::crypto::verify_signal(&ClassicalModule, &signal, &key).unwrap_or(false);
        if !ok {
            let mut pruned = false;
            if let Ok(Some(record)) = node.store().synapse_for_peer(&peer.node_id) {
                if let Ok(Some(outcome)) = node.penalize_signature_failure(&record.id) {
                    pruned = outcome.pruned;
                }
            }
            let _ = events
                .send(Event::SignatureFailed {
                    peer: peer.node_id.clone(),
                    pruned,
                })
                .await;
            if pruned {
                // threat-model §4: the synapse is gone and re-formation is in
                // cooldown, so there is nothing left to carry traffic over
                // this connection. Closing is the honest consequence.
                return Err(RuntimeError::Handshake(format!(
                    "{} exceeded the signature-failure threshold; synapse pruned",
                    peer.node_id
                )));
            }
            continue;
        }

        // Structural validation, but deliberately *not* `Signal::validate()`
        // wholesale. That also rejects `ttl == 0`, and TTL exhaustion is a
        // routing outcome the sender is owed a receipt for
        // (delivery-semantics §2.2) — dropping it here turned a reportable
        // refusal into silence. TTL therefore flows on to `receive`, which
        // refuses it through `check_propagable` and produces the receipt.
        //
        // What is left is the check `receive` genuinely cannot make: a weight
        // outside [0, 1] is a protocol violation rather than a routing
        // decision, and `check_propagable` only tests the lower bound.
        if !signal.weight.is_finite() {
            // No sane value to clamp NaN toward, and it is malformed rather
            // than inflated.
            let _ = events
                .send(Event::Malformed {
                    peer: peer.node_id.clone(),
                    reason: format!("weight {} is not a finite number", signal.weight),
                })
                .await;
            continue;
        }

        // threat-model §6: `weight` and `ttl` are outside the origin signature
        // because propagation mutates them, so an on-path node can inflate
        // either. This node enforces its own bounds regardless of what
        // arrived — clamping rather than dropping, since the inflation is the
        // relay's doing and dropping would let any on-path node destroy
        // traffic by overwriting a field it is free to overwrite.
        let clamp =
            ntl_core::propagation::clamp_inbound_headers(&mut signal, &node.config().propagation);
        if clamp.any() {
            let _ = events
                .send(Event::HeadersClamped {
                    peer: peer.node_id.clone(),
                    signal_id: signal.id,
                    weight: clamp.weight_clamped,
                    ttl: clamp.ttl_clamped,
                })
                .await;
        }

        // A receipt resolves one of our decisions rather than being routed on.
        if signal.signal_type == SignalType::Receipt {
            if let Ok(receipt) = serde_json::from_value::<Receipt>(signal.payload.clone()) {
                match node.apply_receipt(&receipt, &peer.node_id) {
                    Ok(Some(update)) => {
                        let _ = events
                            .send(Event::ReceiptApplied {
                                peer: peer.node_id.clone(),
                                delivered: receipt.is_delivered(),
                                weight_delta: update.applied_delta,
                            })
                            .await;
                    }
                    // An unmatched receipt is discarded, not an error: forged
                    // receipts would otherwise be the cheapest attack on the
                    // routing model.
                    Ok(None) => tracing::debug!("receipt matched no pending decision"),
                    Err(e) => tracing::warn!(error = %e, "receipt application failed"),
                }
            }
            continue;
        }

        let arrival = node
            .store()
            .synapse_for_peer(&peer.node_id)
            .ok()
            .flatten()
            .map(|r| r.id);

        let disposition = match node.receive(&signal, arrival.as_ref()) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, "receive failed");
                continue;
            }
        };

        // A signal displaced from the queue to make room for this one owes its
        // own sender a receipt, and that is a different peer.
        if let Some(ev) = &disposition.evicted {
            let body = pending.lock().await.remove(&ev.signal.id);
            let _ = events
                .send(Event::Refused {
                    signal_id: ev.signal.id,
                    reason: ev.reason,
                })
                .await;
            if ev.needs_receipt() {
                #[allow(clippy::cast_possible_truncation)]
                let hops = body.as_ref().map_or(0, |s| s.trace.len() as u16);
                let receipt = Receipt::rejected(ev.signal.id, ev.reason, hops);
                send_receipt_to(node, identity, sessions, &receipt, &ev.signal.origin).await;
            }
        }

        // A refusal owes the sender a receipt when the class demands one.
        if let Some(receipt) = &disposition.receipt {
            send_receipt(node, identity, sessions, receipt, &signal.origin, peer).await;
        }
        if let Some(reason) = disposition.rejected {
            let _ = events
                .send(Event::Refused {
                    signal_id: signal.id,
                    reason,
                })
                .await;
            // Deliberately no `continue`: refusing *this* signal says nothing
            // about the rest of the batch the gate may have drained on its way
            // in, and skipping the loop below discarded that batch entirely.
        }

        // Everything the gate released, not just this arrival. `fire` drains a
        // batch, so a heavy arrival can release several queued signals at
        // once; handling only the arrival dropped the rest silently — no
        // local handling, no forward, and no receipt for their senders.
        for released in &disposition.handle_locally {
            // The arrival's body is in hand. Anything else came out of the
            // queue, so its body is in the cache.
            let (body, plan) = if released.id == signal.id {
                (Some(signal.clone()), Some(disposition.forward_to.clone()))
            } else {
                (pending.lock().await.remove(&released.id), None)
            };
            release_signal(
                node,
                identity,
                sessions,
                events,
                released.id,
                body,
                plan,
                Release::Threshold,
            )
            .await;
        }

        // Retain the body only while the gate is actually holding the signal.
        // Caching before `receive` leaked an entry for every duplicate: a
        // dedup hit and a successful enqueue both look like an empty
        // disposition from outside, so the cache filled with bodies nothing
        // would ever release and then began evicting live ones.
        if disposition.queued {
            remember_body(pending, node, &signal).await;
        }
    }
}

/// Cache a signal body while the activation gate is holding it.
///
/// The cache mirrors the gate's queue: an entry is added when a signal is
/// enqueued and removed on every way back out — fired, released by the guard,
/// displaced by overflow, or refused. So its size is bounded by the queue
/// depth without needing a policy of its own, and it must not have one:
/// evicting on any criterion the gate does not share drops the body of a
/// signal still queued, which then releases with nothing to forward.
///
/// The cap below is a leak backstop, not a mechanism. Reaching it means the
/// mirror has drifted, which is a bug in this file rather than a condition to
/// handle quietly — hence the warning.
async fn remember_body(pending: &PendingBodies, node: &Arc<Node>, signal: &Signal) {
    let queue_depth = node.config().activation.max_queue_depth.max(1);
    let mut guard = pending.lock().await;
    if guard.len() >= queue_depth.saturating_mul(2) {
        // SignalId is a ULID, so the smallest is the oldest.
        if let Some(oldest) = guard.keys().min().copied() {
            guard.remove(&oldest);
            tracing::warn!(
                cached = guard.len() + 1,
                queue_depth,
                "signal body cache exceeded twice the queue depth; evicting. \
                 This means a queued signal's body was not released — a bug, \
                 not backpressure."
            );
        }
    }
    guard.insert(signal.id, signal.clone());
}

/// Why the activation gate let a signal through.
///
/// Only affects which event is reported, but the distinction matters to an
/// operator: a stream of guard releases means the node is saturated and
/// signals are being let through to avoid starving them, rather than because
/// they earned their way past the threshold.
#[derive(Debug, Clone, Copy)]
enum Release {
    /// The accumulated potential crossed the threshold.
    Threshold,
    /// The queue latency guard released it before it could starve.
    Guard,
}

/// Handle one signal the activation gate has released.
///
/// The single path for both releases: the batch drained on arrival, and the
/// latency guard's later drain. `plan` is the forwarding decision
/// [`Node::receive`] already journalled for the arriving signal; `None` means
/// this signal was released without one and needs a fresh plan.
#[allow(clippy::too_many_arguments)]
async fn release_signal(
    node: &Arc<Node>,
    identity: &Arc<Identity>,
    sessions: &Arc<RwLock<HashMap<NodeId, Session>>>,
    events: &mpsc::Sender<Event>,
    signal_id: ntl_core::SignalId,
    body: Option<Signal>,
    plan: Option<Vec<ntl_core::node::Forward>>,
    how: Release,
) {
    let Some(body) = body else {
        // No body: the gate is holding an id this process never saw a body
        // for, which is what a restart looks like — the activation snapshot is
        // persisted, the body cache is not. Report the release so an operator
        // sees it rather than losing the signal without trace, but there is
        // nothing left to forward.
        let _ = events
            .send(Event::Released {
                signal_id,
                signal: None,
            })
            .await;
        return;
    };

    // Exactly one event per released signal. Emitting both `Released` and
    // `Handled` printed the same signal twice in `ntl listen`.
    let _ = events
        .send(match how {
            Release::Threshold => Event::Handled {
                signal: Box::new(body.clone()),
            },
            Release::Guard => Event::Released {
                signal_id,
                signal: Some(Box::new(body.clone())),
            },
        })
        .await;

    let arrival = body
        .trace
        .last()
        .and_then(|hop| node.store().synapse_for_peer(hop).ok().flatten())
        .map(|r| r.id);

    let forwards = match plan {
        Some(p) => p,
        None => match node.plan_release(&body, arrival.as_ref()) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "planning a released signal failed");
                Vec::new()
            }
        },
    };

    let delivered = forward_signal(node, identity, sessions, events, &body, &forwards).await;

    // The receipt reports what this node did with the signal, which is
    // hop-local by design — end-to-end receipts are out of scope, see
    // threat-model. Handling it locally is delivery; failing to reach any
    // chosen peer is not.
    if body.requires_receipt() {
        #[allow(clippy::cast_possible_truncation)]
        let hops = body.trace.len() as u16;
        let receipt = if forwards.is_empty() || delivered {
            Receipt::delivered(signal_id, hops)
        } else {
            Receipt::rejected(signal_id, ntl_core::RejectReason::TransportFailure, hops)
        };
        send_receipt_to(node, identity, sessions, &receipt, &body.origin).await;
    }
}

/// Transmit a signal over the chosen synapses.
///
/// Returns whether at least one peer actually received it. A journalled
/// decision whose peer has no live session is resolved as a transport failure
/// straight away: leaving it pending would let the timeout sweep attribute the
/// disconnection to the *path* several seconds later, and the two are
/// different things to learn from.
async fn forward_signal(
    node: &Arc<Node>,
    identity: &Arc<Identity>,
    sessions: &Arc<RwLock<HashMap<NodeId, Session>>>,
    events: &mpsc::Sender<Event>,
    signal: &Signal,
    forwards: &[ntl_core::node::Forward],
) -> bool {
    if forwards.is_empty() {
        return false;
    }

    let explored = forwards.iter().any(|f| f.explored);
    let mut sent = 0;
    let mut failed = Vec::new();
    {
        let guard = sessions.read().await;
        for forward in forwards {
            let live = guard.get(&forward.peer).filter(|session| {
                let mut outbound = signal.clone();
                outbound.hop(identity.node_id.clone());
                outbound.attenuate_for_hop(
                    node.config().propagation.attenuation_factor,
                    node.config().propagation.min_propagation_weight,
                );
                session.outbound.try_send(outbound).is_ok()
            });
            if live.is_some() {
                sent += 1;
            } else {
                failed.push(forward.journal_id);
            }
        }
    }

    for journal_id in failed {
        if let Err(e) = node.fail_forward(journal_id) {
            tracing::warn!(error = %e, "recording a transport failure failed");
        }
    }

    let _ = events
        .send(Event::Forwarded {
            signal_id: signal.id,
            peers: sent,
            explored,
        })
        .await;

    sent > 0
}

/// Send a receipt toward an origin we hold no arriving peer for.
///
/// Returns whether it was actually handed to a session.
///
/// Only a *direct* session with the origin will do. This used to fall back to
/// `sessions.values().next()` — an arbitrary peer — on the theory that a
/// receipt routes `Targeted` and can be relayed. Nothing relays it: the read
/// loop applies a receipt to a local decision and `continue`s, so a receipt
/// reaching a node that has no matching decision is discarded. Sending to an
/// arbitrary peer therefore looked like delivery and was not, which is worse
/// than not sending: the caller believed the sender had been told.
///
/// Receipts are consequently one hop, the same bound and for the same reason
/// as signature verification — see threat-model §9.
async fn send_receipt_to(
    node: &Arc<Node>,
    identity: &Arc<Identity>,
    sessions: &Arc<RwLock<HashMap<NodeId, Session>>>,
    receipt: &Receipt,
    origin: &NodeId,
) -> bool {
    let Ok(mut signal) = node.emit(Signal::receipt(receipt, origin.clone())) else {
        return false;
    };
    if ntl_core::crypto::sign_signal(&ClassicalModule, &mut signal, &identity.private).is_err() {
        return false;
    }
    let guard = sessions.read().await;
    if let Some(session) = guard.get(origin) {
        return session.outbound.try_send(signal).is_ok();
    }
    tracing::debug!(%origin, "no direct session with the origin; receipt not sent");
    false
}

/// Send a receipt back toward a signal's origin.
async fn send_receipt(
    node: &Arc<Node>,
    identity: &Arc<Identity>,
    sessions: &Arc<RwLock<HashMap<NodeId, Session>>>,
    receipt: &Receipt,
    origin: &NodeId,
    via: &PeerIdentity,
) {
    let Ok(mut signal) = node.emit(Signal::receipt(receipt, origin.clone())) else {
        return;
    };
    if ntl_core::crypto::sign_signal(&ClassicalModule, &mut signal, &identity.private).is_err() {
        return;
    }

    let guard = sessions.read().await;
    // Prefer a direct session with the origin; otherwise return it the way the
    // signal came, which is the reverse of the recorded path.
    let target = guard.get(origin).or_else(|| guard.get(&via.node_id));
    if let Some(session) = target {
        let _ = session.outbound.try_send(signal);
    }
}

/// Runtime errors.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// Storage failed.
    #[error("storage error: {0}")]
    Store(String),
    /// Identity could not be established.
    #[error("identity error: {0}")]
    Identity(String),
    /// The core node rejected an operation.
    #[error("node error: {0}")]
    Node(String),
    /// A cryptographic operation failed.
    #[error("crypto error: {0}")]
    Crypto(String),
    /// The listen address could not be bound.
    #[error("cannot bind: {0}")]
    Bind(String),
    /// A peer could not be dialed.
    #[error("cannot dial: {0}")]
    Dial(String),
    /// The handshake failed.
    #[error("handshake failed: {0}")]
    Handshake(String),
    /// Framing failed.
    #[error(transparent)]
    Frame(#[from] FrameError),
}

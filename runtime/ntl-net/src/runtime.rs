//! The node runtime: transport, peer sessions, and the learning loop.
//!
//! This is the async half of an NTL node. [`ntl_core::Node`] decides *what*
//! should happen to a signal; this drives the I/O that makes it happen, and
//! the periodic work the learning model needs.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

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
                            let body = pending.lock().await.remove(&queued.id);
                            #[allow(clippy::cast_possible_truncation)]
                            let hops = body.as_ref().map_or(0, |s| s.trace.len() as u16);
                            let _ = events
                                .send(Event::Released {
                                    signal_id: queued.id,
                                    signal: body.map(Box::new),
                                })
                                .await;
                            if queued.delivery.requires_receipt() {
                                let receipt = Receipt::delivered(queued.id, hops);
                                send_receipt_to(
                                    &node,
                                    &identity,
                                    &sessions,
                                    &receipt,
                                    &queued.origin,
                                )
                                .await;
                            }
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
    sessions.write().await.insert(
        peer.node_id.clone(),
        Session {
            outbound: tx,
            public_key: peer.public_key.clone(),
        },
    );
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

async fn handshake(
    reader: &Arc<Mutex<tokio::net::tcp::OwnedReadHalf>>,
    writer: &Arc<Mutex<tokio::net::tcp::OwnedWriteHalf>>,
    node: &Arc<Node>,
    identity: &Arc<Identity>,
) -> Result<PeerIdentity, RuntimeError> {
    // Announce ourselves.
    let mut hello = node
        .emit(Signal::discovery().with_payload(serde_json::json!({
            "public_key": identity.public.0,
            "module": ClassicalModule::ID,
        })))
        .map_err(|e| RuntimeError::Node(e.to_string()))?;
    ntl_core::crypto::sign_signal(&ClassicalModule, &mut hello, &identity.private)
        .map_err(|e| RuntimeError::Crypto(e.to_string()))?;

    {
        let mut guard = writer.lock().await;
        frame::write_signal(&mut *guard, &hello)
            .await
            .map_err(RuntimeError::Frame)?;
    }

    // Await theirs.
    let their_hello = {
        let mut guard = reader.lock().await;
        frame::read_signal(&mut *guard)
            .await
            .map_err(RuntimeError::Frame)?
    };

    if their_hello.signal_type != SignalType::Discovery {
        return Err(RuntimeError::Handshake(format!(
            "expected a Discovery signal, got {:?}",
            their_hello.signal_type
        )));
    }

    let key_bytes: Vec<u8> = their_hello
        .payload
        .get("public_key")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .ok_or_else(|| RuntimeError::Handshake("no public key in handshake".to_string()))?;
    let public_key = PublicKey(key_bytes);

    // The claimed identity must match the key that signed the handshake.
    // Without this check a peer could claim any identity it liked, and
    // per-identity influence caps would be meaningless.
    let derived = ntl_core::crypto::node_id_from_public_key(&public_key);
    if derived != their_hello.origin {
        return Err(RuntimeError::Handshake(
            "claimed identity does not match the handshake public key".to_string(),
        ));
    }
    if !ntl_core::crypto::verify_signal(&ClassicalModule, &their_hello, &public_key)
        .map_err(|e| RuntimeError::Crypto(e.to_string()))?
    {
        return Err(RuntimeError::Handshake(
            "handshake signature did not verify".to_string(),
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
        let signal = {
            let mut guard = reader.lock().await;
            match frame::read_signal(&mut *guard).await {
                Ok(s) => s,
                Err(FrameError::Closed) => return Ok(()),
                Err(e) => return Err(RuntimeError::Frame(e)),
            }
        };

        // Verification comes after framing but before any routing work. The
        // origin's key is known for a direct peer, and also for a relayed
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
        if let Some(key) = origin_key {
            let ok =
                ntl_core::crypto::verify_signal(&ClassicalModule, &signal, &key).unwrap_or(false);
            if !ok {
                if let Ok(Some(record)) = node.store().synapse_for_peer(&peer.node_id) {
                    let _ = node.penalize_signature_failure(&record.id);
                }
                let _ = events
                    .send(Event::SignatureFailed {
                        peer: peer.node_id.clone(),
                    })
                    .await;
                continue;
            }
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
            continue;
        }

        // Queued rather than fired: keep the body so the latency guard can
        // release it in full later.
        let fired_this_signal = disposition.handle_locally.iter().any(|h| h.id == signal.id);
        if !fired_this_signal && !disposition.was_rejected() {
            let mut guard = pending.lock().await;
            if guard.len() < node.config().activation.max_queue_depth {
                guard.insert(signal.id, signal.clone());
            }
        }

        // Anything the gate released is handled here, and acknowledged
        // signals get a positive receipt.
        for handled in &disposition.handle_locally {
            if handled.id == signal.id {
                let _ = events
                    .send(Event::Handled {
                        signal: Box::new(signal.clone()),
                    })
                    .await;
                if signal.requires_receipt() {
                    #[allow(clippy::cast_possible_truncation)]
                    let hops = signal.trace.len() as u16;
                    let receipt = Receipt::delivered(signal.id, hops);
                    send_receipt(node, identity, sessions, &receipt, &signal.origin, peer).await;
                }
            }
        }

        // Forward onward.
        if !disposition.forward_to.is_empty() {
            let explored = disposition.forward_to.iter().any(|f| f.explored);
            let mut sent = 0;
            let guard = sessions.read().await;
            for forward in &disposition.forward_to {
                if let Some(session) = guard.get(&forward.peer) {
                    let mut outbound = signal.clone();
                    outbound.hop(identity.node_id.clone());
                    outbound.attenuate_for_hop(
                        node.config().propagation.attenuation_factor,
                        node.config().propagation.min_propagation_weight,
                    );
                    if session.outbound.try_send(outbound).is_ok() {
                        sent += 1;
                    }
                }
            }
            drop(guard);
            let _ = events
                .send(Event::Forwarded {
                    signal_id: signal.id,
                    peers: sent,
                    explored,
                })
                .await;
        }
    }
}

/// Send a receipt toward an origin, with no particular peer to fall back on.
async fn send_receipt_to(
    node: &Arc<Node>,
    identity: &Arc<Identity>,
    sessions: &Arc<RwLock<HashMap<NodeId, Session>>>,
    receipt: &Receipt,
    origin: &NodeId,
) {
    let Ok(mut signal) = node.emit(Signal::receipt(receipt, origin.clone())) else {
        return;
    };
    if ntl_core::crypto::sign_signal(&ClassicalModule, &mut signal, &identity.private).is_err() {
        return;
    }
    let guard = sessions.read().await;
    // Prefer the origin directly; otherwise any peer, since the receipt
    // routes Targeted and can be relayed.
    if let Some(session) = guard.get(origin).or_else(|| guard.values().next()) {
        let _ = session.outbound.try_send(signal);
    }
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

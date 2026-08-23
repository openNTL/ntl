//! Inbound signal verification at the transport boundary.
//!
//! Propagation Rule 5 says a node MUST verify a signal's signature before
//! processing or propagating it. These tests drive a real `Runtime` over a
//! real socket from a hand-rolled peer, because the interesting cases are all
//! ones a well-behaved peer never produces: a forged signature, and a claimed
//! origin whose key this node has no way to obtain.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ntl_core::crypto::{ClassicalModule, PrivateKey, PublicKey, node_id_from_public_key};
use ntl_core::signal::{NodeId, Signal, SignalType};
use ntl_core::store::MemoryStore;
use ntl_core::{NodeConfig, NodeStore};
use ntl_net::runtime::{Event, Runtime, RuntimeConfig};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// A keypair standing in for a peer, derived from a fixed seed so failures
/// reproduce.
fn keys(seed: u8) -> (PublicKey, PrivateKey, NodeId) {
    let (public, private) =
        ClassicalModule::keypair_from_seed(&[seed; 32]).expect("keypair from seed");
    let node_id = node_id_from_public_key(&public);
    (public, private, node_id)
}

/// Start a listening runtime and return its address plus its event stream.
async fn start_node() -> (Arc<Runtime>, SocketAddr, mpsc::Receiver<Event>) {
    let store: Arc<dyn NodeStore> = Arc::new(MemoryStore::new());
    let (runtime, events) = Runtime::new(
        store,
        RuntimeConfig {
            bind: "127.0.0.1:0".parse().expect("bind address"),
            bootstrap: Vec::new(),
            node: NodeConfig::default(),
        },
    )
    .expect("runtime");
    let runtime = Arc::new(runtime);
    let (addr, _accept) = runtime.listen().await.expect("listen");
    // The activation latency guard lives in the maintenance loop, so without
    // it a signal that does not immediately cross the threshold is never
    // released and the accepted case looks identical to a dropped one.
    // Dropping the handle does not abort the task.
    let _maintenance = runtime.spawn_maintenance();
    (runtime, addr, events)
}

/// A raw peer: a socket plus the identity it authenticated as.
struct RawPeer {
    stream: TcpStream,
    private: PrivateKey,
    node_id: NodeId,
}

impl RawPeer {
    /// Connect and complete the three-message Discovery handshake honestly.
    ///
    /// SYN / SYN-ACK / ACK: both sides send a signed hello carrying a public
    /// key and a fresh challenge, then both send a signed proof echoing the
    /// peer's challenge. The proof is what makes the exchange an
    /// authentication rather than a replayable bearer token, so a helper that
    /// skipped it would not be exercising the real path.
    async fn connect(addr: SocketAddr, seed: u8) -> Self {
        let (public, private, node_id) = keys(seed);
        let mut stream = TcpStream::connect(addr).await.expect("connect");

        // Both sides send their hello without waiting, so read order is free.
        let mut challenge = [0u8; 32];
        for (i, b) in challenge.iter_mut().enumerate() {
            *b = seed ^ u8::try_from(i).unwrap_or(0);
        }

        let mut hello = Signal::discovery()
            .with_payload(serde_json::json!({
                "public_key": public.0,
                "module": ClassicalModule::ID,
                "challenge": challenge,
            }))
            .build_unsigned(node_id.clone());
        ntl_core::crypto::sign_signal(&ClassicalModule, &mut hello, &private).expect("sign hello");
        ntl_net::frame::write_signal(&mut stream, &hello)
            .await
            .expect("write hello");

        let their_hello = ntl_net::frame::read_signal(&mut stream)
            .await
            .expect("their hello");
        assert_eq!(their_hello.signal_type, SignalType::Discovery);
        let their_challenge: Vec<u8> = serde_json::from_value(
            their_hello
                .payload
                .get("challenge")
                .expect("the node must issue a challenge")
                .clone(),
        )
        .expect("challenge bytes");
        assert_eq!(their_challenge.len(), 32);

        let mut proof = Signal::discovery()
            .with_payload(serde_json::json!({ "proof_for": their_challenge }))
            .build_unsigned(node_id.clone());
        ntl_core::crypto::sign_signal(&ClassicalModule, &mut proof, &private).expect("sign proof");
        ntl_net::frame::write_signal(&mut stream, &proof)
            .await
            .expect("write proof");

        let their_proof = ntl_net::frame::read_signal(&mut stream)
            .await
            .expect("their proof");
        let echoed: Vec<u8> = serde_json::from_value(
            their_proof
                .payload
                .get("proof_for")
                .expect("the node must answer our challenge")
                .clone(),
        )
        .expect("proof bytes");
        assert_eq!(
            echoed,
            challenge.to_vec(),
            "the node must echo our challenge, not some other value"
        );

        Self {
            stream,
            private,
            node_id,
        }
    }

    /// Send a signal exactly as given, with no further validation.
    async fn send(&mut self, signal: &Signal) {
        ntl_net::frame::write_signal(&mut self.stream, signal)
            .await
            .expect("write signal");
    }
}

/// Collect events for a bounded window.
///
/// A timeout rather than a fixed count: the assertions below are as much about
/// what the node does *not* emit as what it does, and waiting for a specific
/// count would hang on exactly the failure being tested.
async fn drain(events: &mut mpsc::Receiver<Event>, window: Duration) -> Vec<Event> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + window;
    while let Ok(Some(event)) = tokio::time::timeout_at(deadline, events.recv()).await {
        out.push(event);
    }
    out
}

#[tokio::test]
async fn a_signal_from_an_unknown_origin_is_dropped_rather_than_processed() {
    let (_runtime, addr, mut events) = start_node().await;
    let mut peer = RawPeer::connect(addr, 1).await;

    // A third identity the node has never met, and never will: no session, no
    // handshake, no stored key. This is the shape of a relayed signal, and it
    // is also the shape of a forgery — the node cannot tell them apart, which
    // is the whole point.
    let (_ghost_public, ghost_private, ghost_id) = keys(9);

    let mut relayed = Signal::data("relayed")
        .with_weight(0.6)
        .build_unsigned(ghost_id.clone());
    // Signed correctly by the ghost. Even a *valid* signature is unusable
    // without the key, so the node must drop it: accepting it would mean
    // accepting the forged case too.
    ntl_core::crypto::sign_signal(&ClassicalModule, &mut relayed, &ghost_private).expect("sign");
    peer.send(&relayed).await;

    let seen = drain(&mut events, Duration::from_millis(400)).await;

    assert!(
        seen.iter().any(|e| matches!(
            e,
            Event::OriginKeyUnknown { origin, .. } if *origin == ghost_id
        )),
        "expected the drop to be reported, saw {seen:?}"
    );
    assert!(
        !seen.iter().any(|e| matches!(e, Event::Handled { .. })),
        "an unverifiable signal must not be handled locally, saw {seen:?}"
    );
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, Event::Forwarded { peers, .. } if *peers > 0)),
        "an unverifiable signal must not be forwarded, saw {seen:?}"
    );
}

#[tokio::test]
async fn a_forged_signature_from_a_known_peer_is_dropped_and_penalised() {
    let (runtime, addr, mut events) = start_node().await;
    let mut peer = RawPeer::connect(addr, 2).await;

    // Wait for the handshake to land, so the synapse exists and the penalty
    // has something to apply to.
    assert_eq!(runtime.wait_for_peers(1, Duration::from_secs(5)).await, 1);
    let before = runtime
        .node()
        .store()
        .synapse_for_peer(&peer.node_id)
        .expect("store")
        .expect("synapse")
        .weight;

    let mut forged = Signal::data("forged")
        .with_weight(0.6)
        .build_unsigned(peer.node_id.clone());
    ntl_core::crypto::sign_signal(&ClassicalModule, &mut forged, &peer.private).expect("sign");
    // Corrupt the payload after signing: the signature now belongs to a
    // different body.
    forged.payload = serde_json::json!({ "tampered": true });
    peer.send(&forged).await;

    let seen = drain(&mut events, Duration::from_millis(400)).await;

    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::SignatureFailed { .. })),
        "expected a signature failure, saw {seen:?}"
    );
    assert!(
        !seen.iter().any(|e| matches!(e, Event::Handled { .. })),
        "a forged signal must not be handled locally, saw {seen:?}"
    );

    let after = runtime
        .node()
        .store()
        .synapse_for_peer(&peer.node_id)
        .expect("store")
        .expect("synapse")
        .weight;
    assert!(
        after < before,
        "a signature failure must cost the synapse weight: {before} -> {after}"
    );
}

#[tokio::test]
async fn a_signal_from_the_connected_peer_is_accepted() {
    // The control case. Without it the two tests above would also pass on a
    // node that dropped everything.
    let (runtime, addr, mut events) = start_node().await;
    let mut peer = RawPeer::connect(addr, 3).await;
    assert_eq!(runtime.wait_for_peers(1, Duration::from_secs(5)).await, 1);

    let mut good = Signal::data("hello")
        .with_weight(0.9)
        .build_unsigned(peer.node_id.clone());
    ntl_core::crypto::sign_signal(&ClassicalModule, &mut good, &peer.private).expect("sign");
    peer.send(&good).await;

    let seen = drain(&mut events, Duration::from_millis(1_500)).await;
    assert!(
        seen.iter().any(|e| matches!(e, Event::Handled { .. }))
            || seen.iter().any(|e| matches!(e, Event::Released { .. })),
        "a correctly signed signal from a connected peer must be handled or \
         released by the latency guard, saw {seen:?}"
    );
    assert!(
        !seen.iter().any(|e| matches!(
            e,
            Event::SignatureFailed { .. } | Event::OriginKeyUnknown { .. }
        )),
        "the control case must not trip any verification failure, saw {seen:?}"
    );
}

#[tokio::test]
async fn an_expired_ttl_is_refused_and_reported_not_silently_dropped() {
    let (runtime, addr, mut events) = start_node().await;
    let mut peer = RawPeer::connect(addr, 4).await;
    assert_eq!(runtime.wait_for_peers(1, Duration::from_secs(5)).await, 1);

    // TTL zero is exhausted on arrival. It must reach the routing layer, which
    // refuses it and owes an acknowledged sender a receipt — an earlier version
    // ran `Signal::validate()` at the transport boundary and dropped it as
    // "malformed", turning a reportable refusal into silence.
    let mut expired = Signal::data("expired")
        .with_weight(0.5)
        .with_ttl(0)
        .acknowledged()
        .build_unsigned(peer.node_id.clone());
    ntl_core::crypto::sign_signal(&ClassicalModule, &mut expired, &peer.private).expect("sign");
    peer.send(&expired).await;

    let seen = drain(&mut events, Duration::from_millis(500)).await;
    assert!(
        seen.iter().any(|e| matches!(
            e,
            Event::Refused { reason, .. } if *reason == ntl_core::RejectReason::TtlExhausted
        )),
        "an exhausted TTL must be refused, not silently dropped, saw {seen:?}"
    );
    assert!(
        !seen.iter().any(|e| matches!(e, Event::Malformed { .. })),
        "TTL exhaustion is a routing outcome, not a malformed signal, saw {seen:?}"
    );
    assert!(
        !seen.iter().any(|e| matches!(e, Event::Handled { .. })),
        "an expired signal must not be handled, saw {seen:?}"
    );

    // And the sender is told. The receipt comes back over the same session.
    let receipt = tokio::time::timeout(
        Duration::from_secs(2),
        ntl_net::frame::read_signal(&mut peer.stream),
    )
    .await
    .expect("an acknowledged refusal must produce a receipt")
    .expect("a readable signal");
    assert_eq!(receipt.signal_type, SignalType::Receipt);
}

#[tokio::test]
async fn a_weight_outside_the_valid_range_is_malformed() {
    let (runtime, addr, mut events) = start_node().await;
    let mut peer = RawPeer::connect(addr, 8).await;
    assert_eq!(runtime.wait_for_peers(1, Duration::from_secs(5)).await, 1);

    // Weight is in the signed body, so this is a peer signing something it
    // should never have sent rather than an on-path edit. `check_propagable`
    // only tests the lower bound, so this is the one structural check the
    // routing layer cannot make.
    let mut bad = Signal::data("overweight")
        .with_weight(0.5)
        .build_unsigned(peer.node_id.clone());
    bad.weight = 4.0;
    ntl_core::crypto::sign_signal(&ClassicalModule, &mut bad, &peer.private).expect("sign");
    peer.send(&bad).await;

    let seen = drain(&mut events, Duration::from_millis(500)).await;
    assert!(
        seen.iter().any(|e| matches!(e, Event::Malformed { .. })),
        "expected a malformed-signal drop, saw {seen:?}"
    );
    assert!(
        !seen.iter().any(|e| matches!(e, Event::Handled { .. })),
        "a malformed signal must not be handled, saw {seen:?}"
    );
}

// ---------------------------------------------------------------------------
// The activation/forwarding seam.
//
// The activation gate is a *queue*, so a signal can be admitted on one arrival
// and released later — in a batch triggered by a different arrival, or by the
// latency guard. The runtime was written as if the gate only ever returned the
// arriving signal, which lost everything else it released.
// ---------------------------------------------------------------------------

/// A node whose gate fires a batch, so several queued signals are released at
/// once and the arrival is not the only one.
async fn start_batching_node() -> (Arc<Runtime>, SocketAddr, mpsc::Receiver<Event>) {
    let store: Arc<dyn NodeStore> = Arc::new(MemoryStore::new());
    let mut node = NodeConfig::default();
    // Contribution is signal_weight × synapse_weight, and a fresh synapse
    // sits at 0.1 — so a 0.15 signal contributes 0.015. Two of them cross
    // this threshold on the *second* arrival, which is what makes the gate
    // drain a batch through the arrival path rather than the latency guard.
    node.activation.base_threshold = 0.025;
    node.activation.fire_batch_size = 8;
    // Long enough that the guard cannot be what releases them.
    node.activation.max_queue_latency_ms = 60_000;
    node.activation.refractory_period_ms = 0;
    node.activation.dynamic_threshold = false;

    let (runtime, events) = Runtime::new(
        store,
        RuntimeConfig {
            bind: "127.0.0.1:0".parse().expect("bind address"),
            bootstrap: Vec::new(),
            node,
        },
    )
    .expect("runtime");
    let runtime = Arc::new(runtime);
    let (addr, _accept) = runtime.listen().await.expect("listen");
    let _maintenance = runtime.spawn_maintenance();
    (runtime, addr, events)
}

#[tokio::test]
async fn every_signal_a_batch_releases_is_handled_not_only_the_arrival() {
    let (runtime, addr, mut events) = start_batching_node().await;
    let mut peer = RawPeer::connect(addr, 5).await;
    assert_eq!(runtime.wait_for_peers(1, Duration::from_secs(5)).await, 1);

    // Two signals. The first sits below the threshold; the second pushes the
    // accumulated potential over it, and the gate drains both.
    let mut sent = Vec::new();
    for (i, weight) in [0.15_f32, 0.15].into_iter().enumerate() {
        let mut signal = Signal::data("batch")
            .with_weight(weight)
            .with_payload(serde_json::json!({ "n": i }))
            .build_unsigned(peer.node_id.clone());
        ntl_core::crypto::sign_signal(&ClassicalModule, &mut signal, &peer.private).expect("sign");
        peer.send(&signal).await;
        sent.push(signal.id);
        // Serialise arrivals so the batch composition is deterministic.
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    let seen = drain(&mut events, Duration::from_millis(800)).await;
    let handled: Vec<_> = seen
        .iter()
        .filter_map(|e| match e {
            Event::Handled { signal } => Some(signal.id),
            _ => None,
        })
        .collect();

    // No Released events: the guard is set to 60s, so anything handled here
    // came out of the batch the second arrival fired.
    assert!(
        !seen.iter().any(|e| matches!(e, Event::Released { .. })),
        "the latency guard must not be what released these, saw {seen:?}"
    );
    for id in &sent {
        assert!(
            handled.contains(id),
            "every signal the batch released must be handled, not just the \
             arrival that triggered it. Missing {id}; handled {handled:?}"
        );
    }
}

#[tokio::test]
async fn a_signal_released_by_the_latency_guard_is_still_forwarded() {
    // Contribution is signal_weight × synapse_weight, but the threshold is
    // absolute, so over a fresh synapse (weight 0.1) even a heavy signal never
    // crosses it and only the latency guard releases it. That path emitted an
    // event and a positive receipt without forwarding, so a relay node
    // reported delivery for a signal it then dropped.
    let (relay, relay_addr, mut events) = start_node().await;

    // Two peers, so the relay has somewhere to forward to that is not where
    // the signal came from.
    let mut sender = RawPeer::connect(relay_addr, 6).await;
    let mut onward = RawPeer::connect(relay_addr, 7).await;
    assert_eq!(relay.wait_for_peers(2, Duration::from_secs(5)).await, 2);

    let mut signal = Signal::data("relayed-onward")
        .with_weight(0.9)
        .build_unsigned(sender.node_id.clone());
    ntl_core::crypto::sign_signal(&ClassicalModule, &mut signal, &sender.private).expect("sign");
    sender.send(&signal).await;

    let seen = drain(&mut events, Duration::from_millis(2_000)).await;

    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::Released { signal_id, .. } if *signal_id == signal.id)),
        "the guard should have released it, saw {seen:?}"
    );
    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::Forwarded { signal_id, peers, .. }
                if *signal_id == signal.id && *peers > 0)),
        "a released signal must still be forwarded onward, saw {seen:?}"
    );

    // And the onward peer really received it, rather than the relay merely
    // reporting that it had.
    let received = tokio::time::timeout(
        Duration::from_secs(2),
        ntl_net::frame::read_signal(&mut onward.stream),
    )
    .await
    .expect("the onward peer should receive the forwarded signal")
    .expect("a readable signal");
    assert_eq!(received.id, signal.id);
    assert!(
        received.trace.contains(&relay.identity().node_id),
        "the relay must appear in the trace of what it forwarded"
    );
}

// ---------------------------------------------------------------------------
// Handshake authentication.
//
// The exchange used to be a single signed hello: self-contained, non-expiring
// and replayable. It proved someone had once held the private key, not that
// the party on this socket holds it. And a node writes its hello before
// reading the peer's, so the hello was harvestable by anyone able to open a
// TCP connection.
// ---------------------------------------------------------------------------

/// Read a node's hello without answering it — what an attacker harvests.
async fn harvest_hello(addr: SocketAddr) -> Signal {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let hello = ntl_net::frame::read_signal(&mut stream)
        .await
        .expect("a node offers its hello to anyone who connects");
    assert_eq!(hello.signal_type, SignalType::Discovery);
    hello
}

#[tokio::test]
async fn a_replayed_hello_does_not_authenticate() {
    // Victim and the peer whose identity gets stolen.
    let (victim, victim_addr, mut events) = start_node().await;
    let (_target, target_addr, _target_events) = start_node().await;

    // Step 1: harvest the target's signed hello. No key material needed — the
    // node hands it to any connection.
    let stolen = harvest_hello(target_addr).await;
    let stolen_identity = stolen.origin.clone();

    // Step 2: replay it verbatim at the victim.
    let mut attacker = TcpStream::connect(victim_addr).await.expect("connect");
    let _their_hello = ntl_net::frame::read_signal(&mut attacker)
        .await
        .expect("the victim's hello");
    ntl_net::frame::write_signal(&mut attacker, &stolen)
        .await
        .expect("write the stolen hello");

    // The identity binding holds and the signature is genuine, so everything
    // the old handshake checked passes. What the attacker cannot do is answer
    // the victim's fresh challenge, so no session is ever established.
    let connected = victim.wait_for_peers(1, Duration::from_secs(2)).await;
    assert_eq!(
        connected, 0,
        "a replayed hello must not produce a session — it proves possession of \
         a key at some point in the past, not now"
    );
    assert!(
        !victim.connected_peers().await.contains(&stolen_identity),
        "the attacker must not hold a session under the stolen identity"
    );

    let seen = drain(&mut events, Duration::from_millis(200)).await;
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, Event::PeerConnected { .. })),
        "no peer should have connected, saw {seen:?}"
    );
}

#[tokio::test]
async fn a_node_refuses_its_own_reflected_hello() {
    // The degenerate case, needing no third party: echo a node's own hello
    // back at it. The identity binding holds — it is the node's own key — and
    // the signature is genuinely the node's. The old handshake accepted it and
    // formed an active synapse under the node's own identity, after which the
    // node would route its own traffic to whoever held the socket.
    let (node, addr, mut events) = start_node().await;
    let own_identity = node.identity().node_id.clone();

    let mut attacker = TcpStream::connect(addr).await.expect("connect");

    // Reflect *everything*. The node's hello carries its own challenge, so the
    // proof it then sends answers that challenge — and reflecting the proof
    // back satisfies the echo check perfectly. Nothing but the self-identity
    // check stands between this and a synapse to self, which is why the mirror
    // attack has to be refused by identity rather than by the challenge.
    let its_own_hello = ntl_net::frame::read_signal(&mut attacker)
        .await
        .expect("the node's hello");
    ntl_net::frame::write_signal(&mut attacker, &its_own_hello)
        .await
        .expect("reflect the hello");

    // The node may already have hung up on the identity check; if it has not,
    // reflect its proof as well so the mirror is complete.
    if let Ok(Ok(its_own_proof)) = tokio::time::timeout(
        Duration::from_secs(2),
        ntl_net::frame::read_signal(&mut attacker),
    )
    .await
    {
        let _ = ntl_net::frame::write_signal(&mut attacker, &its_own_proof).await;
    }

    assert_eq!(
        node.wait_for_peers(1, Duration::from_secs(2)).await,
        0,
        "a node must not form a session with itself"
    );
    assert!(
        !node.connected_peers().await.contains(&own_identity),
        "and certainly not under its own identity"
    );
    let seen = drain(&mut events, Duration::from_millis(200)).await;
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, Event::PeerConnected { .. }))
    );

    // The synapse table must be untouched too: a self-synapse would be
    // selected as a routing candidate.
    let synapses = node
        .node()
        .store()
        .list_synapses(&ntl_core::store::SynapseFilter::eligible())
        .expect("store");
    assert!(
        !synapses.iter().any(|s| s.peer == own_identity),
        "no synapse to self"
    );
}

#[tokio::test]
async fn a_proof_that_echoes_the_wrong_challenge_is_refused() {
    // A peer that holds a real key but answers a challenge of its own choosing
    // rather than the one it was issued. This is what a relayed or
    // man-in-the-middle proof looks like.
    let (node, addr, mut events) = start_node().await;
    let (public, private, node_id) = keys(21);

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let mut hello = Signal::discovery()
        .with_payload(serde_json::json!({
            "public_key": public.0,
            "module": ClassicalModule::ID,
            "challenge": vec![7u8; 32],
        }))
        .build_unsigned(node_id.clone());
    ntl_core::crypto::sign_signal(&ClassicalModule, &mut hello, &private).expect("sign");
    ntl_net::frame::write_signal(&mut stream, &hello)
        .await
        .expect("write hello");
    let _their_hello = ntl_net::frame::read_signal(&mut stream)
        .await
        .expect("their hello");

    // Correctly signed, wrong content: it echoes a challenge the node never
    // issued.
    let mut proof = Signal::discovery()
        .with_payload(serde_json::json!({ "proof_for": vec![9u8; 32] }))
        .build_unsigned(node_id.clone());
    ntl_core::crypto::sign_signal(&ClassicalModule, &mut proof, &private).expect("sign");
    ntl_net::frame::write_signal(&mut stream, &proof)
        .await
        .expect("write proof");

    assert_eq!(
        node.wait_for_peers(1, Duration::from_secs(2)).await,
        0,
        "a proof must answer the challenge this node issued"
    );
    let seen = drain(&mut events, Duration::from_millis(200)).await;
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, Event::PeerConnected { .. }))
    );
}

#[tokio::test]
async fn a_second_connection_does_not_displace_a_live_session() {
    // `sessions.insert` returned the old sender and dropped it, tearing down
    // the existing writer pump — so a second connection claiming an identity
    // silently took over every signal routed to that peer.
    let (node, addr, _events) = start_node().await;
    let first = RawPeer::connect(addr, 30).await;
    assert_eq!(node.wait_for_peers(1, Duration::from_secs(5)).await, 1);

    // A second connection with the *same* key. It can answer the challenge —
    // it holds the key — so this is not an impersonation, just a duplicate.
    let second = RawPeer::connect(addr, 30).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(
        node.connected_peers().await.len(),
        1,
        "one identity, one session"
    );

    // And the original is the one still standing: it must still be able to
    // deliver a signal.
    let mut first = first;
    let mut signal = Signal::data("still-here")
        .with_weight(0.9)
        .build_unsigned(first.node_id.clone());
    ntl_core::crypto::sign_signal(&ClassicalModule, &mut signal, &first.private).expect("sign");
    first.send(&signal).await;
    drop(second);
}

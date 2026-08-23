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
    /// Connect and complete the Discovery handshake honestly.
    async fn connect(addr: SocketAddr, seed: u8) -> Self {
        let (public, private, node_id) = keys(seed);
        let mut stream = TcpStream::connect(addr).await.expect("connect");

        // The node speaks first on an inbound connection, so read its hello
        // before sending ours. Order does not actually matter to the protocol,
        // but draining it keeps the socket from filling behind us.
        let their_hello = ntl_net::frame::read_signal(&mut stream)
            .await
            .expect("their hello");
        assert_eq!(their_hello.signal_type, SignalType::Discovery);

        let mut hello = Signal::discovery()
            .with_payload(serde_json::json!({
                "public_key": public.0,
                "module": ClassicalModule::ID,
            }))
            .build_unsigned(node_id.clone());
        ntl_core::crypto::sign_signal(&ClassicalModule, &mut hello, &private).expect("sign hello");
        ntl_net::frame::write_signal(&mut stream, &hello)
            .await
            .expect("write hello");

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
async fn a_structurally_invalid_signal_is_dropped_even_when_correctly_signed() {
    let (runtime, addr, mut events) = start_node().await;
    let mut peer = RawPeer::connect(addr, 4).await;
    assert_eq!(runtime.wait_for_peers(1, Duration::from_secs(5)).await, 1);

    // TTL zero is expired on arrival. It is inside the signed body, so this is
    // a peer signing something it should never have sent rather than an
    // on-path edit — but the node must refuse it either way.
    let mut expired = Signal::data("expired")
        .with_weight(0.5)
        .with_ttl(0)
        .build_unsigned(peer.node_id.clone());
    ntl_core::crypto::sign_signal(&ClassicalModule, &mut expired, &peer.private).expect("sign");
    peer.send(&expired).await;

    let seen = drain(&mut events, Duration::from_millis(400)).await;
    assert!(
        seen.iter().any(|e| matches!(e, Event::Malformed { .. })),
        "expected a malformed-signal drop, saw {seen:?}"
    );
    assert!(
        !seen.iter().any(|e| matches!(e, Event::Handled { .. })),
        "a malformed signal must not be handled, saw {seen:?}"
    );
}

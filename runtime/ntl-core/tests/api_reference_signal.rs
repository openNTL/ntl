//! Every code sample in `docs/api-reference/signal.mdx`, compiled and run.
//!
//! The page quotes this file. It previously documented an API that did not
//! exist: `SignalBuilder::emit` (there is none — you emit through the node),
//! `.await` on a crate with no `async fn` anywhere, a `SignalType::Ack`
//! variant that was renamed `Receipt`, and a field table missing five of the
//! struct's fields. Prose cannot be type-checked, so the samples live here.
//! If the API changes, this file stops compiling.

use std::sync::Arc;

use ntl_core::delivery::{Receipt, RejectReason};
use ntl_core::signal::Encoding;
use ntl_core::store::MemoryStore;
use ntl_core::{DeliveryClass, Node, NodeId, PropagationScope, Signal, SignalId, SignalType};

/// A node to emit through. Synchronous: `ntl-core` holds no async runtime,
/// which is what keeps it building for `wasm32-unknown-unknown`.
fn build_node() -> ntl_core::Result<Node> {
    let store = Arc::new(MemoryStore::new());
    Node::builder().with_store(store).build()
}

/// The six constructors. Each returns a `SignalBuilder`, never a `Signal`.
fn constructors(origin: &NodeId) {
    let data = Signal::data("orders.created").build_unsigned(origin.clone());
    let query = Signal::query("orders.lookup").build_unsigned(origin.clone());
    let event = Signal::event("order.shipped").build_unsigned(origin.clone());
    let command = Signal::command("order.cancel").build_unsigned(origin.clone());
    let discovery = Signal::discovery().build_unsigned(origin.clone());
    let heartbeat = Signal::heartbeat().build_unsigned(origin.clone());

    assert_eq!(data.signal_type, SignalType::Data);
    assert_eq!(query.signal_type, SignalType::Query);
    assert_eq!(event.signal_type, SignalType::Event);
    assert_eq!(command.signal_type, SignalType::Command);
    assert_eq!(discovery.signal_type, SignalType::Discovery);
    assert_eq!(heartbeat.signal_type, SignalType::Heartbeat);

    // `discovery` and `heartbeat` take no topic; they carry a fixed one.
    assert_eq!(discovery.tags, vec!["discovery".to_string()]);
    assert_eq!(heartbeat.tags, vec!["heartbeat".to_string()]);
}

/// What an untouched builder produces.
fn builder_defaults(origin: &NodeId) {
    let signal = Signal::data("orders.created").build_unsigned(origin.clone());

    assert_eq!(signal.weight, 0.5);
    assert_eq!(signal.ttl, 10);
    assert_eq!(signal.delivery, DeliveryClass::BestEffort);
    assert_eq!(signal.encoding, Encoding::Cbor);
    assert!(matches!(
        signal.scope,
        PropagationScope::Weighted {
            min_synapse_weight: 0.0
        }
    ));

    // The topic is not a field: it is pushed onto the front of `tags`.
    assert_eq!(signal.tags, vec!["orders.created".to_string()]);

    // Out-of-range weights are clamped by the builder, not rejected.
    let clamped = Signal::data("orders.created")
        .with_weight(1.5)
        .build_unsigned(origin.clone());
    assert_eq!(clamped.weight, 1.0);
}

/// Every builder method, then emission through the node. The node stamps
/// origin, identifier and timestamp, so there is no `emit` on the builder.
fn full_builder_chain(node: &Node) -> ntl_core::Result<Signal> {
    let signal = node.emit(
        Signal::event("order.shipped")
            .with_payload(serde_json::json!({ "order_id": "A-1" }))
            .with_weight(0.7)
            .with_ttl(6)
            .with_tags(vec!["orders", "fulfilment"])
            .with_scope(PropagationScope::Weighted {
                min_synapse_weight: 0.2,
            })
            .with_delivery(DeliveryClass::Acknowledged),
    )?;

    assert_eq!(signal.weight, 0.7);
    assert_eq!(signal.ttl, 6);
    assert_eq!(
        signal.tags,
        vec![
            "order.shipped".to_string(),
            "orders".to_string(),
            "fulfilment".to_string(),
        ]
    );
    Ok(signal)
}

/// Request-response: the reply carries the request's identifier.
fn correlating_a_reply(node: &Node, request: &Signal) -> ntl_core::Result<Signal> {
    let reply = node.emit(
        Signal::data("orders.lookup.reply")
            .with_payload(serde_json::json!({ "status": "shipped" }))
            .with_correlation(request.id),
    )?;

    assert_eq!(reply.correlation_id, Some(request.id));
    Ok(reply)
}

/// The fields an emitted signal carries.
fn reading_fields(node: &Node, signal: &Signal) {
    assert_eq!(&signal.origin, node.identity());
    assert_eq!(signal.version, 1);
    assert_eq!(signal.encoding, Encoding::Cbor);
    assert!(signal.timestamp > 0);
    assert!(signal.trace.is_empty());

    // `emit` stamps identity, not a signature. Signing happens before
    // transmission, over `signing_bytes()`.
    assert!(signal.signature.is_empty());
}

/// A signal is not valid until it is signed.
fn validation(origin: &NodeId) {
    let unsigned = Signal::data("orders.created").build_unsigned(origin.clone());
    assert!(unsigned.validate().is_err());

    let mut signed = unsigned;
    signed.signature = vec![0u8; 64];
    assert!(signed.validate().is_ok());

    // A run-down TTL is the other rejection an unmodified builder can reach.
    let mut expired = signed;
    expired.ttl = 0;
    assert!(expired.is_expired());
    assert!(expired.validate().is_err());
}

/// The wire type byte. `Custom` has no round trip: byte 15 does not carry the
/// application-defined name, so `from_type_byte` returns `None` for it.
fn signal_types_on_the_wire() {
    for signal_type in [
        SignalType::Data,
        SignalType::Query,
        SignalType::Event,
        SignalType::Command,
        SignalType::Heartbeat,
        SignalType::Discovery,
        SignalType::Receipt,
    ] {
        let byte = signal_type.to_type_byte();
        assert_eq!(SignalType::from_type_byte(byte), Some(signal_type));
    }

    let custom = SignalType::Custom("orders.bespoke".to_string());
    assert_eq!(custom.to_type_byte(), 15);
    assert_eq!(SignalType::from_type_byte(15), None);

    // A receipt is never itself acknowledged, so the protocol cannot recurse.
    assert!(!SignalType::Receipt.can_be_acknowledged());
    assert!(SignalType::Data.can_be_acknowledged());
}

/// The four propagation scopes.
fn propagation_scopes(origin: &NodeId, destination: &NodeId) {
    let scopes = [
        PropagationScope::Flood { max_hops: 3 },
        PropagationScope::Weighted {
            min_synapse_weight: 0.4,
        },
        PropagationScope::Targeted {
            destination: destination.clone(),
        },
        PropagationScope::Gradient {
            signal_type: "orders".to_string(),
        },
    ];

    for scope in scopes {
        let signal = Signal::data("orders.created")
            .with_scope(scope)
            .build_unsigned(origin.clone());
        assert!(signal.encode().is_ok());
    }
}

/// Acknowledged delivery, and the receipt that reports its outcome.
fn delivery_and_receipts(node: &Node) -> ntl_core::Result<()> {
    let command = node.emit(Signal::command("payment.capture").acknowledged())?;
    assert_eq!(command.delivery, DeliveryClass::Acknowledged);
    assert!(command.requires_receipt());

    let receipt = Receipt::rejected(command.id, RejectReason::NoRoute, 3);
    assert!(!receipt.is_delivered());
    assert!(RejectReason::NoRoute.is_transient());
    assert_eq!(RejectReason::NoRoute.as_str(), "no_route");

    // `Signal::receipt` routes the reply back to the acknowledged signal's
    // origin, and refuses to be acknowledged itself.
    let reply = node.emit(Signal::receipt(&receipt, command.origin.clone()).acknowledged())?;
    assert_eq!(reply.signal_type, SignalType::Receipt);
    assert_eq!(reply.correlation_id, Some(command.id));
    assert_eq!(reply.delivery, DeliveryClass::BestEffort);
    assert!(matches!(reply.scope, PropagationScope::Targeted { .. }));
    Ok(())
}

/// What each hop does to a signal.
fn hop_mechanics(origin: &NodeId) {
    let mut signal = Signal::data("orders.created")
        .with_weight(1.0)
        .with_ttl(2)
        .build_unsigned(origin.clone());

    let relay = NodeId(vec![7u8; 32]);
    signal.hop(relay.clone());
    assert_eq!(signal.ttl, 1);
    assert!(signal.has_visited(&relay));
    assert!(!signal.has_visited(origin));

    signal.attenuate(0.9);
    assert!((signal.weight - 0.9).abs() < 1e-6);

    signal.hop(NodeId(vec![8u8; 32]));
    assert_eq!(signal.ttl, 0);
    assert!(signal.is_expired());
}

/// CBOR encoding, and the size bound checked before any decoding work.
fn wire_encoding(signal: &Signal) -> ntl_core::Result<()> {
    let bytes = signal.encode()?;
    assert!(bytes.len() <= Signal::MAX_SIZE);

    let decoded = Signal::decode(&bytes)?;
    assert_eq!(decoded.id, signal.id);
    assert_eq!(decoded.payload, signal.payload);
    assert_eq!(decoded.delivery, signal.delivery);

    // Oversize input is refused before it is parsed.
    let oversize = vec![0u8; Signal::MAX_SIZE + 1];
    assert!(Signal::decode(&oversize).is_err());
    Ok(())
}

/// What the origin signature covers — and, just as importantly, what it does
/// not. `signature`, `trace`, `ttl` and `weight` are excluded, because
/// propagation mutates them and a signature over them could not survive a
/// single hop.
fn signature_coverage(origin: &NodeId) -> ntl_core::Result<()> {
    let signal = Signal::data("orders.created")
        .with_payload(serde_json::json!({ "order_id": "A-1" }))
        .with_weight(0.8)
        .with_ttl(9)
        .build_unsigned(origin.clone());
    let covered = signal.signing_bytes()?;

    // An on-path node can inflate weight and TTL: the bytes still match.
    let mut relayed = signal.clone();
    relayed.ttl = 64;
    relayed.weight = 1.0;
    relayed.trace.push(NodeId(vec![7u8; 32]));
    relayed.signature = vec![1u8; 64];
    assert_eq!(relayed.signing_bytes()?, covered);

    // Rewriting the payload does not.
    let mut tampered = signal.clone();
    tampered.payload = serde_json::json!({ "order_id": "A-2" });
    assert_ne!(tampered.signing_bytes()?, covered);

    // Nor does downgrading — or upgrading — the delivery class.
    let mut reclassified = signal;
    reclassified.delivery = DeliveryClass::Acknowledged;
    assert_ne!(reclassified.signing_bytes()?, covered);
    Ok(())
}

/// Signing a signal: the signature goes over `signing_bytes()`, never over
/// `encode()`.
#[cfg(feature = "classical-crypto")]
fn signing_a_signal() -> ntl_core::Result<()> {
    use ntl_core::crypto::{ClassicalModule, CryptoModule, Signature, node_id_from_public_key};

    let (public, private) = ClassicalModule::keypair_from_seed(&[7u8; 32])?;
    let origin = node_id_from_public_key(&public);

    let module = ClassicalModule;
    let mut signal = Signal::data("orders.created").build_unsigned(origin);
    signal.signature = module.sign(&signal.signing_bytes()?, &private)?.0;
    assert!(signal.validate().is_ok());

    let signature = Signature(signal.signature.clone());
    assert!(module.verify(&signal.signing_bytes()?, &signature, &public)?);

    // Still verifies after a hop, which is what the exclusions are for.
    signal.hop(NodeId(vec![7u8; 32]));
    signal.attenuate(0.9);
    assert!(module.verify(&signal.signing_bytes()?, &signature, &public)?);
    Ok(())
}

/// `SignalId` is a ULID. It travels as 16 raw bytes; the 26-character string
/// is for humans and logs, and would cost 10 extra bytes per signal.
fn identifiers(signal: &Signal) {
    let bytes: [u8; 16] = signal.id.to_bytes();
    assert_eq!(SignalId::from_bytes(bytes), signal.id);
    assert_eq!(signal.id.to_string().len(), 26);
    assert!(signal.id.timestamp_ms() > 0);
}

#[test]
fn every_sample_on_the_signal_page_compiles_and_runs() {
    let node = build_node().expect("a node with a memory store builds");
    let origin = node.identity().clone();
    let peer = NodeId(vec![9u8; 32]);

    constructors(&origin);
    builder_defaults(&origin);

    let signal = full_builder_chain(&node).expect("emitting an event signal");
    correlating_a_reply(&node, &signal).expect("emitting a correlated reply");
    reading_fields(&node, &signal);
    validation(&origin);
    signal_types_on_the_wire();
    propagation_scopes(&origin, &peer);
    delivery_and_receipts(&node).expect("acknowledged delivery and its receipt");
    hop_mechanics(&origin);
    wire_encoding(&signal).expect("CBOR round trip");
    signature_coverage(&origin).expect("signing-byte coverage");
    identifiers(&signal);

    #[cfg(feature = "classical-crypto")]
    signing_a_signal().expect("signing and verifying a signal");
}

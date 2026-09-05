//! Every code sample in `docs/api-reference/adapter.mdx`, compiled and run.
//!
//! The page quotes this file. It previously documented an API that does not
//! exist: `use ntl::` for a crate named `ntl_core`; `async fn start` and
//! `async fn stop` on a trait that has neither, in a crate containing no
//! `async fn` at all; and three built-in adapters — `Web2Adapter`,
//! `Web3Adapter`, `LegacyAdapter` — with builders, `from_config`, and
//! `node.register_adapter(...).await`. None of those types or methods are
//! written: `adapters/web2`, `adapters/web3` and `adapters/legacy` hold one
//! doc comment each, and `Node` has no `register_adapter`.
//!
//! The trait itself is real and normative (`docs/spec/adapter-contract.mdx`),
//! so what the page can honestly show is an implementation of it. That is the
//! adapter below: synchronous, `Send + Sync`, translating JSON payloads in
//! both directions. If the trait changes, this file stops building.

use std::collections::HashMap;
use std::sync::Mutex;

use ntl_core::adapter::{AdapterCapabilities, ExternalPayload, Protocol};
use ntl_core::rng::SplitMix64;
use ntl_core::signal::NodeId;
use ntl_core::time::ManualClock;
use ntl_core::{Adapter, AdapterHealth, Signal, SignalType};

/// A minimal adapter: JSON bytes in, JSON bytes out.
///
/// `Adapter` requires `Send + Sync`, and `ingest` takes `&self`, so the
/// injected randomness lives behind a `Mutex`. Time and randomness are
/// arguments rather than ambient calls — the same constraint that keeps
/// `ntl-core` buildable for `wasm32-unknown-unknown`.
struct JsonEchoAdapter {
    origin: NodeId,
    clock: ManualClock,
    rng: Mutex<SplitMix64>,
}

impl JsonEchoAdapter {
    fn new(origin: NodeId, epoch_ns: u64) -> Self {
        Self {
            origin,
            clock: ManualClock::starting_at(epoch_ns),
            rng: Mutex::new(SplitMix64::seeded(0x5eed)),
        }
    }
}

impl Adapter for JsonEchoAdapter {
    fn ingest(&self, external: ExternalPayload) -> ntl_core::Result<Signal> {
        if external.content_type != "application/json" {
            return Err(ntl_core::Error::Adapter(format!(
                "unsupported content type: {}",
                external.content_type
            )));
        }

        let payload: serde_json::Value = serde_json::from_slice(&external.data)
            .map_err(|e| ntl_core::Error::Adapter(format!("payload is not JSON: {e}")))?;

        let topic = external
            .metadata
            .get("topic")
            .map_or("external", String::as_str);

        // The adapter builds an *unsigned* signal. Signing, identity and the
        // emission timestamp belong to the node: `Node::emit` stamps them.
        let mut rng = self.rng.lock().expect("adapter rng lock");
        Ok(Signal::data(topic)
            .with_payload(payload)
            .with_weight(0.5)
            .build_unsigned_with(self.origin.clone(), &self.clock, &mut *rng))
    }

    fn emit(&self, signal: Signal) -> ntl_core::Result<ExternalPayload> {
        let data = serde_json::to_vec(&signal.payload)
            .map_err(|e| ntl_core::Error::Adapter(format!("payload is not serialisable: {e}")))?;

        let mut metadata = HashMap::new();
        metadata.insert("signal_id".to_string(), signal.id.to_string());
        metadata.insert("tags".to_string(), signal.tags.join(","));

        Ok(ExternalPayload {
            data,
            content_type: "application/json".to_string(),
            metadata,
        })
    }

    fn protocol(&self) -> Protocol {
        Protocol::Custom("json-echo".to_string())
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            can_ingest: true,
            can_emit: true,
            bidirectional: false,
            correlation: false,
            streaming: false,
        }
    }

    fn health(&self) -> AdapterHealth {
        AdapterHealth::Healthy
    }
}

/// The trait is object-safe, so an adapter can be held as `&dyn Adapter`.
/// Nothing in `ntl-core` holds a registry of them — there is no
/// `node.register_adapter`, so whoever owns the transport owns the adapter.
fn describe(adapter: &dyn Adapter) -> String {
    let protocol = match adapter.protocol() {
        Protocol::Http => "http".to_string(),
        Protocol::WebSocket => "websocket".to_string(),
        Protocol::Grpc => "grpc".to_string(),
        Protocol::GraphQl => "graphql".to_string(),
        Protocol::EvmChain => "evm-chain".to_string(),
        Protocol::Did => "did".to_string(),
        Protocol::Custom(name) => name,
    };

    let health = match adapter.health() {
        AdapterHealth::Healthy => "healthy".to_string(),
        AdapterHealth::Degraded { reason } => format!("degraded: {reason}"),
        AdapterHealth::Unhealthy { reason } => format!("unhealthy: {reason}"),
    };

    format!("{protocol} ({health})")
}

#[test]
fn every_sample_on_the_adapter_reference_page_compiles_and_runs() {
    let adapter = JsonEchoAdapter::new(NodeId(vec![7; 32]), 1_700_000_000 * 1_000_000_000);

    // Ingest: external bytes become an unsigned signal.
    let mut metadata = HashMap::new();
    metadata.insert("topic".to_string(), "user-created".to_string());
    let payload = ExternalPayload {
        data: br#"{"user_id":"abc123"}"#.to_vec(),
        content_type: "application/json".to_string(),
        metadata,
    };

    let signal = adapter.ingest(payload).expect("JSON ingests into a signal");
    assert_eq!(signal.signal_type, SignalType::Data);
    assert!(signal.tags.contains(&"user-created".to_string()));
    assert_eq!(signal.payload["user_id"], "abc123");
    assert!(
        signal.signature.is_empty(),
        "an adapter builds unsigned signals; the node signs on emit"
    );

    // Emit: the signal becomes external bytes again.
    let external = adapter.emit(signal).expect("a signal emits as JSON");
    assert_eq!(external.content_type, "application/json");
    assert_eq!(external.data, br#"{"user_id":"abc123"}"#.to_vec());

    // Capabilities are declared, not inferred.
    let capabilities = adapter.capabilities();
    assert!(capabilities.can_ingest && capabilities.can_emit);
    assert!(!capabilities.streaming);

    // Errors are `Error::Adapter`.
    let wrong_type = ExternalPayload {
        data: b"<xml/>".to_vec(),
        content_type: "application/xml".to_string(),
        metadata: HashMap::new(),
    };
    let err = adapter
        .ingest(wrong_type)
        .expect_err("an unsupported content type is refused");
    assert!(matches!(err, ntl_core::Error::Adapter(_)));

    assert_eq!(describe(&adapter), "json-echo (healthy)");
}

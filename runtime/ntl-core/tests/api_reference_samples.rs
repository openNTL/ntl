//! Every code sample in `docs/api-reference/overview.mdx`, compiled.
//!
//! The page previously documented an API that did not exist: `use ntl::` for a
//! crate named `ntl_core`, `.await` on a crate with no async functions at all,
//! and four identifiers (`SignalHandler`, `register_handler`,
//! `with_crypto_module`, `node.synapses()`) that appear nowhere in the source.
//! Prose cannot be type-checked, so the samples live here and the page quotes
//! them. If the API changes, this file stops compiling.

use std::sync::Arc;

use ntl_core::store::{MemoryStore, SynapseFilter};
use ntl_core::{Node, Signal};

/// Creating a node. Synchronous — `ntl-core` holds no async runtime, which is
/// what keeps it building for `wasm32-unknown-unknown`.
fn creating_a_node() -> ntl_core::Result<Node> {
    let store = Arc::new(MemoryStore::new());
    Node::builder().with_store(store).build()
}

/// Emitting a signal. The builder is handed to the node; the node stamps
/// origin, identifier and timestamp, so there is no `emit` on the builder.
fn emitting_signals(node: &Node) -> ntl_core::Result<()> {
    let signal = node.emit(
        Signal::data("user-created")
            .with_payload(serde_json::json!({ "user_id": "abc123" }))
            .with_weight(0.7)
            .with_tags(vec!["user", "event"]),
    )?;
    assert_eq!(signal.weight, 0.7);
    Ok(())
}

/// Receiving a signal that originated locally.
fn receiving_signals(node: &Node, signal: &Signal) -> ntl_core::Result<()> {
    let disposition = node.receive_local(signal)?;
    let _ = disposition.queued;
    Ok(())
}

/// Listing synapses. There is no `node.synapses()`; synapses are store state,
/// read through a filter.
///
/// Note the error type: the store's failures are `StoreError`, which does not
/// convert into `ntl_core::Error`. A sample returning `ntl_core::Result` here
/// does not compile — a detail prose would have got wrong.
fn managing_synapses(node: &Node) -> Result<(), ntl_core::StoreError> {
    for synapse in node.store().list_synapses(&SynapseFilter::eligible())? {
        let _ = (&synapse.id, synapse.weight, synapse.state);
    }
    Ok(())
}

#[test]
fn every_sample_on_the_api_reference_page_compiles_and_runs() {
    let node = creating_a_node().expect("a node with a memory store builds");
    emitting_signals(&node).expect("emitting a data signal");

    let signal = node
        .emit(Signal::data("probe").with_weight(0.5))
        .expect("emit for the receive sample");
    receiving_signals(&node, &signal).expect("receiving a local signal");
    managing_synapses(&node).expect("listing synapses through the store");
}

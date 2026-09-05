//! Every code sample on `docs/api-reference/node.mdx`, compiled and run.
//!
//! The page quotes this file. It previously documented a node API that does
//! not exist anywhere in the source: `.await` on a crate with no `async fn`
//! at all, plus `with_crypto_module`, `with_bootstrap`, `with_max_synapses`,
//! `register_handler`, `register_adapter`, `run_until_shutdown`,
//! `node.synapses()`, `node.status()`, `node.listen()` and
//! `node.wait_correlation()` — none of which are identifiers this crate
//! defines. Prose cannot be type-checked, so the samples live here instead
//! and the page quotes them. If the API changes, this file stops compiling.

use std::sync::Arc;

use ntl_core::activation::QueuedSignal;
use ntl_core::delivery::Receipt;
use ntl_core::learning::WeightUpdate;
use ntl_core::node::{Disposition, LearningHealth};
use ntl_core::rng::SplitMix64;
use ntl_core::store::{JournalId, MemoryStore, SynapseFilter, SynapseRecord};
use ntl_core::synapse::{SignatureFailureOutcome, SynapseId};
use ntl_core::time::ManualClock;
use ntl_core::{Node, NodeConfig, NodeId, Signal};

// ---------------------------------------------------------------------------
// Building a node
// ---------------------------------------------------------------------------

/// Building a node. `build()` is synchronous: `ntl-core` holds no async
/// runtime, which is what keeps it building for `wasm32-unknown-unknown`.
fn building_a_node() -> ntl_core::Result<Node> {
    let store = Arc::new(MemoryStore::new());

    Node::builder()
        .with_config(NodeConfig::default())
        .with_store(store)
        .build()
}

/// A node with time and randomness supplied by the caller. Nothing in
/// `ntl-core` reaches for an ambient clock or an ambient RNG, so a node built
/// this way replays identically.
fn a_deterministic_node(identity: NodeId) -> ntl_core::Result<(Node, Arc<ManualClock>)> {
    let clock = Arc::new(ManualClock::starting_at(1_700_000_000_000_000_000));

    let node = Node::builder()
        .with_config(NodeConfig::default())
        .with_identity(identity)
        .with_store(Arc::new(MemoryStore::new()))
        .with_clock(clock.clone())
        .with_rng(Box::new(SplitMix64::seeded(7)))
        .build()?;

    Ok((node, clock))
}

/// What a built node will tell you about itself.
fn inspecting_a_node(node: &Node) -> (String, u32, u64) {
    let identity = node.identity().to_string();
    let max_synapses = node.config().synapse.max_synapses;
    let now_ns = node.now_ns();
    let _store = node.store();

    (identity, max_synapses, now_ns)
}

// ---------------------------------------------------------------------------
// Signals in and out
// ---------------------------------------------------------------------------

/// Emitting a signal. The builder is handed to the node; the node stamps
/// origin, identifier and timestamp, so there is no `emit` on the builder.
fn emitting_a_signal(node: &Node) -> ntl_core::Result<Signal> {
    node.emit(
        Signal::data("order-placed")
            .with_payload(serde_json::json!({ "order_id": "abc123" }))
            .with_weight(0.8)
            .acknowledged(),
    )
}

/// Planning routes for a signal this node emitted itself.
fn routing_our_own_signal(node: &Node, signal: &Signal) -> ntl_core::Result<Disposition> {
    let disposition = node.receive_local(signal)?;

    for forward in &disposition.forward_to {
        // Transmit over `forward.synapse`; keep `forward.journal_id` so the
        // outcome can be attributed when a receipt comes back.
        let _ = (&forward.synapse, &forward.peer, forward.journal_id);
    }

    Ok(disposition)
}

/// Handling a signal that arrived from a peer. There is no handler
/// registration: you hand the node a signal and act on the `Disposition` it
/// hands back.
fn handling_an_arrival(
    node: &Node,
    arrival: &Signal,
    arrival_synapse: Option<&SynapseId>,
) -> ntl_core::Result<Disposition> {
    let disposition = node.receive(arrival, arrival_synapse)?;

    if let Some(reason) = disposition.rejected {
        // Refused. `disposition.receipt` is already built when the signal's
        // delivery class requires the sender to be told.
        let _ = reason;
    }
    if disposition.queued {
        // The activation gate is holding it. Retain the body — and only
        // while this stays true.
    }
    for released in &disposition.handle_locally {
        let _ = &released.id;
    }
    if let Some(evicted) = &disposition.evicted {
        // A *different* signal was displaced to make room, and its sender is
        // owed a receipt of its own.
        let _ = evicted.needs_receipt();
    }

    Ok(disposition)
}

// ---------------------------------------------------------------------------
// Driving the node
// ---------------------------------------------------------------------------

/// One turn of the loop you own. Nothing here happens on its own: the node
/// has no thread and no timer.
fn one_turn_of_the_drive_loop(node: &Node) -> ntl_core::Result<(Vec<QueuedSignal>, usize)> {
    // Release anything the gate has held past its latency guard.
    let released = node.poll_activation()?;
    for queued in &released {
        // `queued.id` and `queued.origin` are enough to acknowledge; the body
        // comes from whatever you retained while it was queued.
        let _ = (&queued.id, &queued.origin, queued.delivery);
    }

    // Turn decisions past their deadline into learnable failures.
    let resolved = node.sweep_timeouts(128)?;

    // Persist the activation snapshot and flush the store.
    node.checkpoint()?;

    Ok((released, resolved))
}

/// A signal the gate released still needs a route: `receive` could not have
/// planned one, because it did not yet know the signal would fire.
fn routing_a_released_signal(node: &Node, signal: &Signal) -> ntl_core::Result<usize> {
    Ok(node.plan_release(signal, None)?.len())
}

// ---------------------------------------------------------------------------
// Synapses
// ---------------------------------------------------------------------------

/// Registering a peer, once its handshake has completed.
fn connecting_a_peer(node: &Node, peer: &NodeId) -> ntl_core::Result<SynapseRecord> {
    node.upsert_synapse(peer)
}

/// Listing synapses. There is no `node.synapses()`; synapses are store state,
/// read through a filter.
///
/// Note the error type: the store's failures are `StoreError`, which does not
/// convert into `ntl_core::Error`. A version of this returning
/// `ntl_core::Result` does not compile.
fn listing_synapses(node: &Node) -> Result<usize, ntl_core::StoreError> {
    let synapses = node.store().list_synapses(&SynapseFilter::eligible())?;
    for synapse in &synapses {
        let _ = (&synapse.peer, synapse.weight, synapse.state);
    }
    Ok(synapses.len())
}

// ---------------------------------------------------------------------------
// Outcomes — the half that makes the node learn
// ---------------------------------------------------------------------------

/// A peer acknowledged. This is what closes the learning loop.
fn applying_a_receipt(
    node: &Node,
    receipt: &Receipt,
    from_peer: &NodeId,
) -> ntl_core::Result<Option<WeightUpdate>> {
    node.apply_receipt(receipt, from_peer)
}

/// A forward that never made it onto the wire. Resolve it now rather than
/// letting the timeout sweep blame the path for a transport failure.
fn a_forward_that_never_left(
    node: &Node,
    journal_id: JournalId,
) -> ntl_core::Result<Option<WeightUpdate>> {
    node.fail_forward(journal_id)
}

/// A peer whose signature would not verify.
fn an_unverifiable_signature(
    node: &Node,
    synapse: &SynapseId,
) -> ntl_core::Result<Option<SignatureFailureOutcome>> {
    node.penalize_signature_failure(synapse)
}

/// Rescale outbound weights back inside the node's total budget.
fn rebalancing(node: &Node) -> ntl_core::Result<Option<f32>> {
    node.normalize_outbound_weights()
}

/// Health of the routing model. Exploration near zero means the node has
/// stopped learning; a pending ratio near one means receipts are not coming
/// back, so the weights reflect nothing.
fn checking_learning_health(node: &Node) -> ntl_core::Result<LearningHealth> {
    node.learning_health(256)
}

// ---------------------------------------------------------------------------
// Fixtures — not quoted on the page
// ---------------------------------------------------------------------------

/// A 32-byte identity with a recognisable byte pattern.
fn node_id(byte: u8) -> NodeId {
    NodeId(vec![byte; 32])
}

/// Turn an emitted signal into one that looks like it arrived from a peer:
/// a different origin, and an identifier the node has not already claimed in
/// its dedup cache.
fn arriving_from(signal: &Signal, origin: u8) -> Signal {
    let mut arrival = signal.clone();
    arrival.origin = node_id(origin);
    let mut bytes = signal.id.to_bytes();
    bytes[15] ^= origin;
    bytes[14] ^= 0xA5;
    arrival.id = ntl_core::SignalId::from_bytes(bytes);
    arrival
}

#[test]
fn every_sample_on_the_node_page_compiles_and_runs() {
    // Building.
    let plain = building_a_node().expect("a node with a memory store builds");
    assert_eq!(
        plain.config().synapse.max_synapses,
        NodeConfig::default().synapse.max_synapses
    );

    let (node, clock) = a_deterministic_node(node_id(200)).expect("a deterministic node builds");
    assert_eq!(node.identity(), &node_id(200));
    assert_eq!(node.now_ns(), 1_700_000_000_000_000_000);

    let (identity, max_synapses, now_ns) = inspecting_a_node(&node);
    assert!(!identity.is_empty());
    assert!(max_synapses > 0);
    assert_eq!(now_ns, node.now_ns());

    // A peer to route to.
    let peer = node_id(1);
    let synapse = connecting_a_peer(&node, &peer).expect("the peer's synapse is registered");
    assert_eq!(synapse.peer, peer);
    assert_eq!(listing_synapses(&node).expect("listing synapses"), 1);

    // Emitting and routing our own signal.
    let signal = emitting_a_signal(&node).expect("emitting an acknowledged data signal");
    assert!(signal.requires_receipt());
    let disposition = routing_our_own_signal(&node, &signal).expect("planning routes");
    assert!(
        !disposition.was_rejected(),
        "one eligible synapse is a route"
    );
    let local_forward = disposition
        .forward_to
        .first()
        .cloned()
        .expect("the one eligible synapse carries it");

    // An arrival from a peer.
    let arrival = arriving_from(&signal, 9);
    let handled = handling_an_arrival(&node, &arrival, None).expect("handling an arrival");
    let forward = handled
        .forward_to
        .first()
        .cloned()
        .expect("the arrival is forwarded over the one eligible synapse");

    // The receipt that closes the loop.
    let receipt = Receipt::delivered(arrival.id, 1);
    let update = applying_a_receipt(&node, &receipt, &forward.peer)
        .expect("applying the receipt")
        .expect("the receipt matches a pending decision");
    assert!(update.applied_delta > 0.0, "delivery strengthens the path");

    // A forward that never went out. `receive_local` journalled this one, and
    // no receipt has resolved it.
    a_forward_that_never_left(&node, local_forward.journal_id)
        .expect("resolving an untransmitted forward")
        .expect("an unresolved decision is resolved exactly once");

    // Signature failures and rebalancing.
    an_unverifiable_signature(&node, &synapse.id)
        .expect("penalising a signature failure")
        .expect("the synapse exists");
    rebalancing(&node).expect("normalising outbound weights");

    // The drive loop.
    clock.advance_secs(60);
    let (released, resolved) = one_turn_of_the_drive_loop(&node).expect("one turn of the loop");
    let _ = (released.len(), resolved);
    routing_a_released_signal(&node, &signal).expect("planning a route for a released signal");

    // Observability.
    let health = checking_learning_health(&node).expect("reading learning health");
    assert!(health.decisions_sampled > 0, "decisions were journalled");
}

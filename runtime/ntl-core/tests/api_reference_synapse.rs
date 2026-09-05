//! Every code sample in `docs/api-reference/synapse.mdx`, compiled and run.
//!
//! The page quotes this file. It previously documented an API that does not
//! exist: five `.await` calls on a crate with no `async fn` anywhere, two uses
//! of `node.synapses()` (there is no such method — synapses are store state),
//! a `node.synapse_to()`, a `node.connect()`/`node.disconnect()` pair, and a
//! field table naming `established_at`, `last_active` and `remote_node` on a
//! record whose fields are `established_at_ns`, `last_active_ns` and `peer`.
//!
//! Prose cannot be type-checked, so the samples live here and the page quotes
//! them. If the API changes, this file stops compiling.

use std::sync::Arc;

use ntl_core::store::{MemoryStore, SynapseFilter, SynapseRecord};
use ntl_core::synapse::Synapse;
use ntl_core::{Node, NodeId, StoreError, SynapseId, SynapseState};

/// Building a node to hold the synapses. Synchronous: `ntl-core` has no async
/// runtime, which is what keeps it building for `wasm32-unknown-unknown`.
fn creating_a_node() -> ntl_core::Result<Node> {
    let store = Arc::new(MemoryStore::new());
    Node::builder().with_store(store).build()
}

/// Forming a synapse to a peer, or reactivating an existing one.
///
/// Returns `ntl_core::Result` — `upsert_synapse` is a node method, and the
/// node converts store failures into `ntl_core::Error` on the way out.
fn forming_a_synapse(node: &Node, peer: &NodeId) -> ntl_core::Result<SynapseRecord> {
    node.upsert_synapse(peer)
}

/// Listing synapses. There is no `node.synapses()`; synapses are store state,
/// read through a filter.
///
/// Note the error type: the store's failures are `StoreError`, which does not
/// convert into `ntl_core::Error`. A function returning `ntl_core::Result`
/// cannot use `?` on a store call.
fn listing_eligible_synapses(node: &Node) -> Result<Vec<SynapseRecord>, StoreError> {
    let eligible = node.store().list_synapses(&SynapseFilter::eligible())?;
    for synapse in &eligible {
        let _ = format!(
            "{} -> {}: weight={:.3} state={:?}",
            synapse.id, synapse.peer, synapse.weight, synapse.state
        );
    }
    Ok(eligible)
}

/// A narrower filter, built from the four public fields.
fn narrowing_the_filter(node: &Node) -> Result<Vec<SynapseRecord>, StoreError> {
    let filter = SynapseFilter {
        states: vec![SynapseState::Weakening],
        min_weight: Some(0.02),
        last_active_before_ns: Some(node.now_ns()),
        limit: Some(50),
    };
    node.store().list_synapses(&filter)
}

/// Looking one synapse up, by peer or by identifier.
fn looking_up_one_synapse(node: &Node, peer: &NodeId) -> Result<Option<SynapseId>, StoreError> {
    let Some(record) = node.store().synapse_for_peer(peer)? else {
        return Ok(None);
    };
    let same = node.store().get_synapse(&record.id)?;
    assert_eq!(same.as_ref().map(|r| &r.id), Some(&record.id));
    Ok(Some(record.id))
}

/// Reading the persisted record. Every timestamp field is nanoseconds since
/// the Unix epoch, and the field names say so.
fn reading_a_record(record: &SynapseRecord) -> u64 {
    let idle_ns = record
        .last_active_ns
        .saturating_sub(record.established_at_ns);
    let _ = (
        record.weight,
        record.attenuation_factor,
        record.error_rate,
        record.avg_latency_ns,
        record.signals_transmitted,
        record.signals_received,
        record.signature_failures,
        record.failure_window_start_ns,
        record.type_affinity.len(),
    );
    idle_ns
}

/// Writing a record back, and removing one. Deleting an absent synapse is not
/// an error.
fn writing_and_deleting(node: &Node, record: &SynapseRecord) -> Result<(), StoreError> {
    let mut updated = record.clone();
    updated.weight = 0.5;
    node.store().put_synapse(&updated)?;
    node.store().delete_synapse(&updated.id)?;
    node.store().delete_synapse(&updated.id)
}

/// Which states may carry a signal. `Weakening` counts: it means "below the
/// active threshold, still connected", and weight is only earned by carrying
/// traffic that succeeds, so excluding it would be a one-way trap.
fn eligibility_by_state() {
    assert!(SynapseState::Active.can_carry());
    assert!(SynapseState::Weakening.can_carry());
    assert!(!SynapseState::Forming.can_carry());
    assert!(!SynapseState::Dormant.can_carry());
    assert!(!SynapseState::Pruned.can_carry());
    assert!(SynapseState::Pruned.is_terminal());
}

/// Rehydrating the live struct from a record, to read derived values the
/// record does not store.
fn live_synapse_from_record(node: &Node, record: &SynapseRecord) -> f32 {
    let synapse = Synapse::from_record(record, node.identity().clone(), &node.config().synapse);
    assert_eq!(synapse.remote_node, record.peer);
    let affinity = synapse.affinity_for("data");
    assert_eq!(synapse.to_record().id, record.id);
    affinity
}

/// A synapse pruned for signature failures refuses to re-form until its
/// cooldown elapses. Without that, a prune costs an attacker one handshake.
fn pruned_synapses_refuse_to_re_form(node: &Node, peer: &NodeId) -> ntl_core::Result<()> {
    let record = node.upsert_synapse(peer)?;
    let threshold = node.config().learning.signature_failure_prune_threshold;
    for _ in 0..threshold {
        node.penalize_signature_failure(&record.id)?;
    }

    let pruned = node
        .store()
        .get_synapse(&record.id)
        .expect("the store is in memory and cannot fail here")
        .expect("the synapse is still on record after pruning");
    assert_eq!(pruned.state, SynapseState::Pruned);

    // Re-forming during the cooldown is refused rather than silently granted.
    assert!(node.upsert_synapse(peer).is_err());
    Ok(())
}

#[test]
fn every_sample_on_the_synapse_page_compiles_and_runs() {
    let node = creating_a_node().expect("a node with a memory store builds");
    let peer = NodeId(vec![7u8; 32]);

    let record = forming_a_synapse(&node, &peer).expect("forming a synapse to a peer");
    assert_eq!(record.peer, peer);
    assert_eq!(record.state, SynapseState::Active);

    let eligible = listing_eligible_synapses(&node).expect("listing eligible synapses");
    assert_eq!(eligible.len(), 1, "the freshly formed synapse is eligible");

    narrowing_the_filter(&node).expect("a narrowed filter is still a valid query");

    let found = looking_up_one_synapse(&node, &peer).expect("looking a synapse up");
    assert_eq!(found.as_ref(), Some(&record.id));

    assert_eq!(
        reading_a_record(&record),
        0,
        "a new synapse has not been idle"
    );
    let _ = live_synapse_from_record(&node, &record);

    writing_and_deleting(&node, &record).expect("writing a record back and removing it");

    eligibility_by_state();

    // A second node, so the pruning sample starts from a clean store.
    let victim = creating_a_node().expect("a second node builds");
    pruned_synapses_refuse_to_re_form(&victim, &peer).expect("pruning on signature failures");
}

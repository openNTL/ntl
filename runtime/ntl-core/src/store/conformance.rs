//! Backend-agnostic conformance suite for [`NodeStore`].
//!
//! Every backend MUST pass [`run_all`]. The suite is part of the public API
//! precisely so out-of-tree backends can be held to the same semantics as
//! the ones in this repository:
//!
//! ```no_run
//! use ntl_core::store::{conformance, MemoryStore};
//!
//! #[test]
//! fn my_backend_conforms() {
//!     conformance::run_all(&MemoryStore::new());
//! }
//! ```
//!
//! The suite asserts behaviour the specification requires, not
//! implementation detail. In particular it pins down the two properties
//! backends most often get wrong: deduplication must be an atomic
//! check-and-set, and resolving a decision twice must keep the first
//! outcome.
//!
//! Each check takes a store that is expected to be **empty**. Backends whose
//! records persist between runs should hand `run_all` a fresh database.

use std::collections::HashMap;

use super::{
    ActivationSnapshot, JournalEntry, NodeStore, Outcome, PeerRecord, PeerSource, StoreError,
    SynapseFilter, SynapseRecord,
};
use crate::signal::{NodeId, SignalId, SignalType};
use crate::synapse::{SynapseId, SynapseState};

const SEC: u64 = 1_000_000_000;

fn node(n: u8) -> NodeId {
    NodeId(vec![n; 32])
}

/// Deterministic identifier factory.
///
/// The suite avoids ambient randomness so a failure reproduces exactly.
fn signal_id(n: u64) -> SignalId {
    SignalId::from_parts(1_700_000_000_000 + n, u128::from(n) << 64)
}

fn synapse_record(id: &str, peer: u8, weight: f32) -> SynapseRecord {
    SynapseRecord {
        id: SynapseId(id.to_string()),
        peer: node(peer),
        weight,
        attenuation_factor: 0.9,
        state: SynapseState::Active,
        type_affinity: HashMap::new(),
        established_at_ns: 1_000,
        last_active_ns: 2_000,
        signals_transmitted: 7,
        signals_received: 3,
        avg_latency_ns: 1_500_000,
        error_rate: 0.0,
    }
}

fn decision(signal: SignalId, peer: u8, at_ns: u64) -> JournalEntry {
    JournalEntry {
        id: None,
        signal,
        signal_type: SignalType::Data,
        synapse: SynapseId("syn-a".into()),
        peer: node(peer),
        score: 0.75,
        explored: false,
        decided_at_ns: at_ns,
        outcome: Outcome::Pending,
        resolved_at_ns: None,
    }
}

/// Run every conformance check against `store`.
///
/// # Panics
/// Panics with a description of the first violated requirement.
pub fn run_all(store: &dyn NodeStore) {
    migrate_is_idempotent(store);
    synapse_roundtrip(store);
    synapse_listing_is_ordered_and_filtered(store);
    peer_roundtrip(store);
    dedup_is_check_and_set(store);
    dedup_entries_expire(store);
    activation_roundtrip(store);
    journal_roundtrip(store);
    journal_resolution_is_idempotent(store);
    journal_timeout_sweep(store);
    influence_accumulates_within_window(store);
    meta_roundtrip(store);
    signal_history_is_consistent(store);
}

/// `migrate` must be safe to call repeatedly.
fn migrate_is_idempotent(store: &dyn NodeStore) {
    store.migrate().expect("first migrate");
    store.migrate().expect("second migrate must be a no-op");
}

/// A stored synapse must come back byte-for-byte, and be reachable by peer.
fn synapse_roundtrip(store: &dyn NodeStore) {
    let mut record = synapse_record("syn-roundtrip", 1, 0.42);
    record.type_affinity.insert("Query".into(), 11);

    store.put_synapse(&record).expect("put");

    let fetched = store
        .get_synapse(&record.id)
        .expect("get")
        .expect("synapse must exist after put");
    assert_eq!(fetched, record, "synapse must round-trip unchanged");

    let by_peer = store
        .synapse_for_peer(&record.peer)
        .expect("by peer")
        .expect("synapse must be reachable by peer");
    assert_eq!(by_peer.id, record.id);

    // put is upsert, not insert: a second put replaces rather than duplicates.
    let mut updated = record.clone();
    updated.weight = 0.99;
    store.put_synapse(&updated).expect("re-put");
    let refetched = store.get_synapse(&record.id).expect("get").expect("present");
    assert!(
        (refetched.weight - 0.99).abs() < f32::EPSILON,
        "put must replace an existing record"
    );

    store.delete_synapse(&record.id).expect("delete");
    assert!(
        store.get_synapse(&record.id).expect("get").is_none(),
        "synapse must be gone after delete"
    );
    store
        .delete_synapse(&record.id)
        .expect("deleting an absent synapse must not error");
}

/// Listing must sort by weight descending and honour every filter field.
fn synapse_listing_is_ordered_and_filtered(store: &dyn NodeStore) {
    let ids = ["ord-low", "ord-mid", "ord-high"];
    for (i, id) in ids.iter().enumerate() {
        let mut r = synapse_record(id, 20 + i as u8, 0.2 + 0.3 * i as f32);
        r.last_active_ns = 1_000 + i as u64 * 1_000;
        if i == 0 {
            r.state = SynapseState::Dormant;
        }
        store.put_synapse(&r).expect("put");
    }

    let all = store
        .list_synapses(&SynapseFilter::default())
        .expect("list all");
    let ours: Vec<_> = all.iter().filter(|s| s.id.0.starts_with("ord-")).collect();
    assert_eq!(ours.len(), 3, "all three records must be listed");
    for pair in ours.windows(2) {
        assert!(
            pair[0].weight >= pair[1].weight,
            "listing must be ordered by weight descending"
        );
    }

    let active = store
        .list_synapses(&SynapseFilter::active())
        .expect("list active");
    assert!(
        active.iter().all(|s| s.state == SynapseState::Active),
        "state filter must exclude non-active synapses"
    );
    assert!(
        !active.iter().any(|s| s.id.0 == "ord-low"),
        "the dormant record must be filtered out"
    );

    let heavy = store
        .list_synapses(&SynapseFilter {
            min_weight: Some(0.7),
            ..SynapseFilter::default()
        })
        .expect("list by weight");
    assert!(
        heavy.iter().all(|s| s.weight >= 0.7),
        "min_weight filter must be respected"
    );

    let idle = store
        .list_synapses(&SynapseFilter {
            last_active_before_ns: Some(2_000),
            ..SynapseFilter::default()
        })
        .expect("list idle");
    assert!(
        idle.iter().all(|s| s.last_active_ns < 2_000),
        "last_active_before_ns filter must be respected"
    );

    let capped = store
        .list_synapses(&SynapseFilter {
            limit: Some(1),
            ..SynapseFilter::default()
        })
        .expect("list capped");
    assert_eq!(capped.len(), 1, "limit must be respected");

    for id in ids {
        store.delete_synapse(&SynapseId(id.to_string())).expect("cleanup");
    }
}

/// Peer records must round-trip, filter by region, and count by provenance.
fn peer_roundtrip(store: &dyn NodeStore) {
    let peer = PeerRecord {
        id: node(30),
        addresses: vec!["ntl://10.0.0.1:4433".into(), "ntl://[::1]:4433".into()],
        region: Some("af-south-1".into()),
        advertised_types: vec!["Data".into(), "Query".into()],
        last_seen_ns: 5_000,
        source: PeerSource::Configured,
    };
    store.put_peer(&peer).expect("put peer");

    let fetched = store.get_peer(&peer.id).expect("get").expect("present");
    assert_eq!(fetched, peer, "peer must round-trip unchanged");

    let in_region = store
        .list_peers(Some("af-south-1"), 10)
        .expect("list by region");
    assert!(
        in_region.iter().any(|p| p.id == peer.id),
        "region filter must match the advertised region"
    );
    assert!(
        store
            .list_peers(Some("eu-west-1"), 10)
            .expect("list other region")
            .iter()
            .all(|p| p.id != peer.id),
        "region filter must exclude other regions"
    );

    assert!(
        store.count_peers(PeerSource::Configured).expect("count") >= 1,
        "configured peers must be counted"
    );
}

/// Deduplication must be an atomic check-and-set: the first call reports
/// unseen, every later call within the TTL reports seen.
fn dedup_is_check_and_set(store: &dyn NodeStore) {
    let id = signal_id(101);
    let now = 100 * SEC;

    assert!(
        !store
            .check_and_set_seen(&id, now, 300)
            .expect("first check"),
        "a signal must be reported unseen the first time"
    );
    assert!(
        store
            .check_and_set_seen(&id, now + 1, 300)
            .expect("second check"),
        "a signal must be reported seen the second time — this is what \
         prevents propagation loops"
    );
    assert!(
        store.has_seen(&id, now + 1).expect("has_seen"),
        "has_seen must agree with check_and_set_seen"
    );

    let other = signal_id(102);
    assert!(
        !store.has_seen(&other, now).expect("has_seen"),
        "has_seen must not report an unrecorded signal as seen"
    );
    assert!(
        !store.check_and_set_seen(&other, now, 300).expect("check"),
        "has_seen must not insert"
    );
}

/// Entries must stop counting as seen once their TTL elapses, and a purge
/// must reclaim them.
fn dedup_entries_expire(store: &dyn NodeStore) {
    let id = signal_id(103);
    let now = 1_000 * SEC;
    let ttl_secs = 60;

    assert!(!store.check_and_set_seen(&id, now, ttl_secs).expect("set"));
    assert!(
        store.has_seen(&id, now + 30 * SEC).expect("within ttl"),
        "entry must still be seen inside its TTL"
    );
    let after = now + (ttl_secs + 1) * SEC;
    assert!(
        !store.has_seen(&id, after).expect("past ttl"),
        "entry must expire once its TTL has elapsed"
    );
    assert!(
        !store.check_and_set_seen(&id, after, ttl_secs).expect("reset"),
        "an expired entry must behave as absent"
    );

    let purged = store.purge_expired_seen(after + 1_000 * SEC).expect("purge");
    assert!(purged >= 1, "purge must reclaim expired entries");
}

/// The activation snapshot must round-trip and be replaced, not appended.
fn activation_roundtrip(store: &dyn NodeStore) {
    assert!(
        store.load_activation().expect("load").is_none()
            || store.load_activation().expect("load").is_some(),
        "load_activation must not error on an empty store"
    );

    let first = ActivationSnapshot {
        potential: 0.4,
        threshold: 0.5,
        refractory_until_ns: 9_000,
        signals_fired: 3,
        taken_at_ns: 8_000,
    };
    store.save_activation(&first).expect("save");
    assert_eq!(
        store.load_activation().expect("load"),
        Some(first),
        "snapshot must round-trip unchanged"
    );

    let second = ActivationSnapshot {
        potential: 0.9,
        taken_at_ns: 9_000,
        ..first
    };
    store.save_activation(&second).expect("save again");
    assert_eq!(
        store.load_activation().expect("load"),
        Some(second),
        "saving must replace the previous snapshot, not accumulate"
    );
}

/// A decision must be appended with an assigned id, findable while pending,
/// and resolvable.
fn journal_roundtrip(store: &dyn NodeStore) {
    let signal = signal_id(104);
    let entry = decision(signal, 40, 10 * SEC);

    let id = store.append_decision(&entry).expect("append");

    let pending = store
        .pending_decision_for(&signal, &node(40))
        .expect("pending lookup")
        .expect("a pending decision must be findable by signal and peer");
    assert_eq!(pending.id, Some(id), "the assigned id must be returned");
    assert_eq!(pending.outcome, Outcome::Pending);
    assert_eq!(pending.score, entry.score, "score must round-trip");

    let resolved = store
        .resolve_decision(id, Outcome::Delivered, 11 * SEC)
        .expect("resolve")
        .expect("resolving a known id must return the entry");
    assert_eq!(resolved.outcome, Outcome::Delivered);
    assert_eq!(resolved.resolved_at_ns, Some(11 * SEC));

    assert!(
        store
            .pending_decision_for(&signal, &node(40))
            .expect("pending lookup")
            .is_none(),
        "a resolved decision must no longer be pending"
    );

    let recent = store
        .recent_decisions(Some(&SignalType::Data), 10)
        .expect("recent");
    assert!(
        recent.iter().any(|e| e.id == Some(id)),
        "a resolved Data decision must appear in recent decisions for Data"
    );
    assert!(
        store
            .recent_decisions(Some(&SignalType::Heartbeat), 10)
            .expect("recent")
            .iter()
            .all(|e| e.id != Some(id)),
        "the signal-type filter must exclude other types"
    );

    assert!(
        store
            .resolve_decision(super::JournalId(u64::MAX), Outcome::Delivered, 12 * SEC)
            .expect("resolving an unknown id must not error")
            .is_none(),
        "an unknown journal id must yield None, not an error"
    );
}

/// The first receipt wins. This is what makes at-least-once delivery safe to
/// retry.
fn journal_resolution_is_idempotent(store: &dyn NodeStore) {
    let signal = signal_id(105);
    let id = store
        .append_decision(&decision(signal, 41, 20 * SEC))
        .expect("append");

    store
        .resolve_decision(id, Outcome::Delivered, 21 * SEC)
        .expect("first resolve");
    let second = store
        .resolve_decision(id, Outcome::Rejected, 22 * SEC)
        .expect("second resolve")
        .expect("entry still present");

    assert_eq!(
        second.outcome,
        Outcome::Delivered,
        "a second receipt must not overwrite the first outcome"
    );
    assert_eq!(
        second.resolved_at_ns,
        Some(21 * SEC),
        "the original resolution timestamp must be preserved"
    );
}

/// Decisions past their deadline must be discoverable so silence can be
/// converted into a negative reward.
fn journal_timeout_sweep(store: &dyn NodeStore) {
    let stale = signal_id(106);
    let fresh = signal_id(107);
    let stale_id = store
        .append_decision(&decision(stale, 42, 30 * SEC))
        .expect("append stale");
    store
        .append_decision(&decision(fresh, 43, 90 * SEC))
        .expect("append fresh");

    let expired = store.expired_decisions(60 * SEC, 100).expect("sweep");
    assert!(
        expired.iter().any(|e| e.id == Some(stale_id)),
        "a decision older than the deadline must be reported expired"
    );
    assert!(
        expired.iter().all(|e| e.decided_at_ns < 60 * SEC),
        "a decision newer than the deadline must not be reported expired"
    );
    assert!(
        expired.iter().all(|e| !e.outcome.is_resolved()),
        "already-resolved decisions must never be reported expired"
    );

    let trimmed = store.trim_journal(60 * SEC).expect("trim");
    assert!(trimmed >= 1, "trim must remove entries older than the cutoff");
}

/// Influence must accumulate per peer inside the window and ignore anything
/// before it. The per-identity cap in the threat model rests on this.
fn influence_accumulates_within_window(store: &dyn NodeStore) {
    let attacker = node(50);
    let bystander = node(51);
    let window_start = 100 * SEC;

    let first = store
        .record_influence(&attacker, 0.10, 110 * SEC, window_start)
        .expect("record");
    assert!(
        (first - 0.10).abs() < 1e-5,
        "the first record must report its own magnitude, got {first}"
    );

    let second = store
        .record_influence(&attacker, 0.15, 120 * SEC, window_start)
        .expect("record");
    assert!(
        (second - 0.25).abs() < 1e-5,
        "influence must accumulate within the window, got {second}"
    );

    // A different peer must not share the attacker's budget.
    let other = store
        .record_influence(&bystander, 0.05, 120 * SEC, window_start)
        .expect("record");
    assert!(
        (other - 0.05).abs() < 1e-5,
        "influence must be accounted per identity, got {other}"
    );

    // Advancing the window must exclude the earlier entries.
    let narrowed = store
        .influence_since(&attacker, 115 * SEC)
        .expect("influence since");
    assert!(
        (narrowed - 0.15).abs() < 1e-5,
        "records before the window start must be excluded, got {narrowed}"
    );

    let purged = store.purge_influence(200 * SEC).expect("purge");
    assert!(purged >= 3, "purge must reclaim old influence records");
}

/// Opaque metadata must round-trip and overwrite.
fn meta_roundtrip(store: &dyn NodeStore) {
    assert!(
        store.get_meta("absent-key").expect("get").is_none(),
        "an unset key must read back as None"
    );

    store.put_meta("node-id", b"first").expect("put");
    assert_eq!(
        store.get_meta("node-id").expect("get").as_deref(),
        Some(&b"first"[..])
    );

    store.put_meta("node-id", b"second").expect("put");
    assert_eq!(
        store.get_meta("node-id").expect("get").as_deref(),
        Some(&b"second"[..]),
        "put_meta must overwrite"
    );
}

/// History must either work or refuse — never silently discard.
fn signal_history_is_consistent(store: &dyn NodeStore) {
    let id = signal_id(108);
    let body = b"signal body";
    let result = store.put_signal_history(&id, body, 1_000);

    if store.signal_history_enabled() {
        result.expect("a history-enabled backend must accept writes");
        assert_eq!(
            store.get_signal_history(&id).expect("get").as_deref(),
            Some(&body[..]),
            "a retained body must read back unchanged"
        );
    } else {
        assert!(
            matches!(result, Err(StoreError::Unsupported(_))),
            "a backend without history must return Unsupported rather than \
             silently discarding the body"
        );
        assert!(
            matches!(
                store.get_signal_history(&id),
                Err(StoreError::Unsupported(_))
            ),
            "reads must refuse consistently with writes"
        );
    }
}

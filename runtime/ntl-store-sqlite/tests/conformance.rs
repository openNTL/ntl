//! The `SQLite` backend must satisfy the same contract as every other backend.

use ntl_core::signal::{NodeId, SignalId, SignalType};
use ntl_core::store::{
    ActivationSnapshot, Durability, JournalEntry, NodeStore, Outcome, PeerRecord, PeerSource,
    StoreError, SynapseFilter, SynapseRecord, conformance,
};
use ntl_core::synapse::{SynapseId, SynapseState};
use ntl_store_sqlite::{SqliteConfig, SqliteStore, Synchronous};

fn store() -> SqliteStore {
    let s = SqliteStore::in_memory().expect("open");
    s.migrate().expect("migrate");
    s
}

fn on_disk(dir: &tempfile::TempDir) -> SqliteStore {
    let s = SqliteStore::with_config(SqliteConfig {
        // FULL so durability() reports Durable and the assertion below is
        // meaningful.
        synchronous: Synchronous::Full,
        ..SqliteConfig::at(dir.path().join("node.db"))
    })
    .expect("open");
    s.migrate().expect("migrate");
    s
}

#[test]
fn sqlite_store_conforms() {
    conformance::run_all(&store());
}

#[test]
fn sqlite_store_with_history_conforms() {
    let s = SqliteStore::in_memory_with_history().expect("open");
    s.migrate().expect("migrate");
    conformance::run_all(&s);
}

#[test]
fn sqlite_store_on_disk_conforms() {
    let dir = tempfile::tempdir().expect("tempdir");
    conformance::run_all(&on_disk(&dir));
}

// -- migrations ------------------------------------------------------------

#[test]
fn migrate_is_idempotent_and_sets_the_version() {
    let s = SqliteStore::in_memory().expect("open");
    assert_eq!(s.schema_version().expect("version"), 0);
    s.migrate().expect("first");
    assert_eq!(
        s.schema_version().expect("version"),
        ntl_store_sqlite::CURRENT_VERSION
    );
    s.migrate().expect("second must be a no-op");
    assert_eq!(
        s.schema_version().expect("version"),
        ntl_store_sqlite::CURRENT_VERSION
    );
}

#[test]
fn a_newer_schema_is_refused_rather_than_guessed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("future.db");
    {
        let conn = rusqlite::Connection::open(&path).expect("open raw");
        conn.pragma_update(None, "user_version", 9_999u32)
            .expect("bump version");
    }
    let s = SqliteStore::open(path.to_str().expect("utf8")).expect("open");
    let err = s.migrate().expect_err("must refuse a future schema");
    assert!(
        matches!(err, StoreError::Migration { .. }),
        "expected a migration error, got {err:?}"
    );
    assert!(
        err.to_string().contains("Upgrade NTL"),
        "the error should tell the operator what to do: {err}"
    );
}

#[test]
fn open_creates_missing_parent_directories() {
    // `ntl init` must not fail because ~/.ntl does not exist yet.
    let dir = tempfile::tempdir().expect("tempdir");
    let nested = dir.path().join("a").join("b").join("node.db");
    let s = SqliteStore::open(nested.to_str().expect("utf8")).expect("open");
    s.migrate().expect("migrate");
    assert!(
        nested.exists(),
        "the database file should have been created"
    );
}

// -- durability honesty ----------------------------------------------------

#[test]
fn durability_is_reported_honestly() {
    let dir = tempfile::tempdir().expect("tempdir");

    assert_eq!(
        on_disk(&dir).durability(),
        Durability::Durable,
        "synchronous=FULL survives power loss"
    );

    let normal = SqliteStore::with_config(SqliteConfig {
        synchronous: Synchronous::Normal,
        ..SqliteConfig::at(dir.path().join("normal.db"))
    })
    .expect("open");
    assert_eq!(
        normal.durability(),
        Durability::BestEffort,
        "synchronous=NORMAL leaves a power-loss window, and claiming \
         otherwise would mislead an operator sizing their deployment"
    );

    assert_eq!(
        SqliteStore::in_memory().expect("open").durability(),
        Durability::Memory
    );
}

// -- persistence across reopen ---------------------------------------------

#[test]
fn state_survives_reopening_the_database() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("node.db");
    let peer = NodeId(vec![7u8; 32]);

    {
        let s = SqliteStore::open(path.to_str().expect("utf8")).expect("open");
        s.migrate().expect("migrate");
        s.put_synapse(&SynapseRecord {
            id: SynapseId("persisted".into()),
            peer: peer.clone(),
            weight: 0.77,
            attenuation_factor: 0.9,
            state: SynapseState::Active,
            type_affinity: [("Query".to_string(), 5u64)].into_iter().collect(),
            established_at_ns: 1_000,
            last_active_ns: 2_000,
            signals_transmitted: 11,
            signals_received: 4,
            avg_latency_ns: 1_234,
            error_rate: 0.02,
        })
        .expect("put");
        s.put_meta("node-id", b"stable-identity").expect("meta");
        s.save_activation(&ActivationSnapshot {
            potential: 0.3,
            threshold: 0.6,
            refractory_until_ns: 5_000,
            signals_fired: 42,
            taken_at_ns: 4_000,
        })
        .expect("activation");
        s.flush().expect("flush");
    }

    let reopened = SqliteStore::open(path.to_str().expect("utf8")).expect("reopen");
    reopened.migrate().expect("migrate");

    let synapse = reopened
        .synapse_for_peer(&peer)
        .expect("read")
        .expect("the learned weight must survive a restart");
    assert!((synapse.weight - 0.77).abs() < 1e-6);
    assert_eq!(synapse.type_affinity.get("Query"), Some(&5));
    assert_eq!(synapse.state, SynapseState::Active);

    assert_eq!(
        reopened.get_meta("node-id").expect("meta").as_deref(),
        Some(&b"stable-identity"[..]),
        "identity must persist, or the node loses every synapse it formed"
    );
    let snapshot = reopened.load_activation().expect("read").expect("present");
    assert_eq!(snapshot.signals_fired, 42);
    assert_eq!(
        snapshot.refractory_until_ns, 5_000,
        "backpressure must survive a restart"
    );
}

// -- SQL-level correctness --------------------------------------------------

#[test]
fn one_synapse_per_peer_is_enforced() {
    let s = store();
    let peer = NodeId(vec![3u8; 32]);
    let base = SynapseRecord {
        id: SynapseId("first".into()),
        peer: peer.clone(),
        weight: 0.5,
        attenuation_factor: 0.9,
        state: SynapseState::Active,
        type_affinity: std::collections::HashMap::new(),
        established_at_ns: 1,
        last_active_ns: 1,
        signals_transmitted: 0,
        signals_received: 0,
        avg_latency_ns: 0,
        error_rate: 0.0,
    };
    s.put_synapse(&base).expect("first");

    // A second synapse to the same peer must not silently create a duplicate.
    let duplicate = SynapseRecord {
        id: SynapseId("second".into()),
        ..base.clone()
    };
    let result = s.put_synapse(&duplicate);
    assert!(
        result.is_err(),
        "a unique index on peer should reject a second synapse to it"
    );

    assert_eq!(
        s.synapse_for_peer(&peer).expect("read").map(|r| r.id),
        Some(SynapseId("first".into()))
    );
}

#[test]
fn custom_signal_types_round_trip_through_the_journal() {
    let s = store();
    let custom = SignalType::Custom("mukoko.order".into());
    let entry = JournalEntry {
        id: None,
        signal: SignalId::from_parts(1_700_000_000_000, 1),
        signal_type: custom.clone(),
        synapse: SynapseId("syn".into()),
        peer: NodeId(vec![1u8; 32]),
        score: 0.5,
        signal_weight: 0.8,
        explored: true,
        decided_at_ns: 1_000,
        outcome: Outcome::Pending,
        resolved_at_ns: None,
    };
    s.append_decision(&entry).expect("append");

    let recent = s.recent_decisions(Some(&custom), 10).expect("recent");
    assert_eq!(recent.len(), 1, "a custom type must be findable by name");
    assert_eq!(recent[0].signal_type, custom);
    assert!(recent[0].explored, "the exploration flag must round-trip");

    // And must not be confused with a built-in type.
    assert!(
        s.recent_decisions(Some(&SignalType::Data), 10)
            .expect("recent")
            .is_empty()
    );
}

#[test]
fn dedup_check_and_set_is_a_single_statement() {
    // Contract: the first caller sees "unseen", every later caller inside the
    // TTL sees "seen". A read-then-write implementation would race here.
    let s = store();
    let id = SignalId::from_parts(1_700_000_000_000, 99);
    let now = 1_000 * 1_000_000_000;

    assert!(!s.check_and_set_seen(&id, now, 300).expect("first"));
    for i in 1..50 {
        assert!(
            s.check_and_set_seen(&id, now + i, 300).expect("repeat"),
            "repeat {i} must report seen"
        );
    }
}

#[test]
fn expired_dedup_entries_are_treated_as_absent() {
    let s = store();
    let id = SignalId::from_parts(1_700_000_000_000, 100);
    let now = 1_000 * 1_000_000_000;
    let ttl = 60;

    assert!(!s.check_and_set_seen(&id, now, ttl).expect("set"));
    let after = now + (ttl + 1) * 1_000_000_000;
    assert!(
        !s.has_seen(&id, after).expect("read"),
        "should have expired"
    );
    assert!(
        !s.check_and_set_seen(&id, after, ttl).expect("reset"),
        "an expired entry must behave as a first sighting"
    );
}

#[test]
fn resolving_twice_keeps_the_first_outcome() {
    let s = store();
    let id = s
        .append_decision(&JournalEntry {
            id: None,
            signal: SignalId::from_parts(1_700_000_000_000, 5),
            signal_type: SignalType::Data,
            synapse: SynapseId("syn".into()),
            peer: NodeId(vec![1u8; 32]),
            score: 0.5,
            signal_weight: 0.8,
            explored: false,
            decided_at_ns: 1_000,
            outcome: Outcome::Pending,
            resolved_at_ns: None,
        })
        .expect("append");

    s.resolve_decision(id, Outcome::Delivered, 2_000)
        .expect("first");
    let second = s
        .resolve_decision(id, Outcome::Rejected, 3_000)
        .expect("second")
        .expect("row present");

    assert_eq!(
        second.outcome,
        Outcome::Delivered,
        "the first receipt must win, which is what makes retries safe"
    );
    assert_eq!(second.resolved_at_ns, Some(2_000));
}

#[test]
fn listing_orders_by_weight_and_breaks_ties_stably() {
    let s = store();
    for (i, (id, weight)) in [("b", 0.5), ("a", 0.5), ("c", 0.9)].iter().enumerate() {
        s.put_synapse(&SynapseRecord {
            id: SynapseId((*id).to_string()),
            peer: NodeId(vec![i as u8 + 1; 32]),
            weight: *weight,
            attenuation_factor: 0.9,
            state: SynapseState::Active,
            type_affinity: std::collections::HashMap::new(),
            established_at_ns: 1,
            last_active_ns: 1,
            signals_transmitted: 0,
            signals_received: 0,
            avg_latency_ns: 0,
            error_rate: 0.0,
        })
        .expect("put");
    }

    let listed = s.list_synapses(&SynapseFilter::default()).expect("list");
    let ids: Vec<String> = listed.iter().map(|r| r.id.0.clone()).collect();
    assert_eq!(
        ids,
        vec!["c", "a", "b"],
        "weight descending, then id ascending, so routing is reproducible"
    );
}

#[test]
fn eligible_filter_includes_weakening_synapses() {
    // Regression guard mirroring the core fix: a Weakening synapse is still
    // connected and must remain a routing candidate, or it can never earn
    // its weight back.
    let s = store();
    for (i, state) in [
        SynapseState::Active,
        SynapseState::Weakening,
        SynapseState::Dormant,
    ]
    .iter()
    .enumerate()
    {
        s.put_synapse(&SynapseRecord {
            id: SynapseId(format!("s{i}")),
            peer: NodeId(vec![i as u8 + 1; 32]),
            weight: 0.5,
            attenuation_factor: 0.9,
            state: *state,
            type_affinity: std::collections::HashMap::new(),
            established_at_ns: 1,
            last_active_ns: 1,
            signals_transmitted: 0,
            signals_received: 0,
            avg_latency_ns: 0,
            error_rate: 0.0,
        })
        .expect("put");
    }

    let eligible = s.list_synapses(&SynapseFilter::eligible()).expect("list");
    assert_eq!(eligible.len(), 2, "Active and Weakening, not Dormant");
    assert!(eligible.iter().any(|r| r.state == SynapseState::Weakening));
    assert!(!eligible.iter().any(|r| r.state == SynapseState::Dormant));
}

#[test]
fn influence_is_summed_per_peer_within_the_window() {
    let s = store();
    let attacker = NodeId(vec![1u8; 32]);
    let bystander = NodeId(vec![2u8; 32]);
    let sec = 1_000_000_000u64;

    s.record_influence(&attacker, 0.1, 100 * sec, 0).expect("a");
    s.record_influence(&attacker, 0.1, 110 * sec, 0).expect("b");
    s.record_influence(&bystander, 0.9, 110 * sec, 0)
        .expect("c");

    let total = s.influence_since(&attacker, 0).expect("sum");
    assert!((total - 0.2).abs() < 1e-5, "got {total}");
    assert!(
        (s.influence_since(&attacker, 105 * sec).expect("windowed") - 0.1).abs() < 1e-5,
        "records before the window start must be excluded"
    );
}

#[test]
fn peers_are_counted_by_provenance() {
    // The eclipse mitigation needs this: discovered peers must not evict
    // configured ones.
    let s = store();
    for (i, source) in [
        PeerSource::Configured,
        PeerSource::Configured,
        PeerSource::Discovered,
    ]
    .iter()
    .enumerate()
    {
        s.put_peer(&PeerRecord {
            id: NodeId(vec![i as u8 + 1; 32]),
            addresses: vec!["ntl://host:4433".into()],
            region: None,
            advertised_types: Vec::new(),
            last_seen_ns: 1_000,
            source: *source,
        })
        .expect("put");
    }
    assert_eq!(s.count_peers(PeerSource::Configured).expect("count"), 2);
    assert_eq!(s.count_peers(PeerSource::Discovered).expect("count"), 1);
    assert_eq!(s.count_peers(PeerSource::Bootstrap).expect("count"), 0);
}

#[test]
fn history_disabled_refuses_rather_than_discarding() {
    let s = store();
    let id = SignalId::from_parts(1_700_000_000_000, 1);
    assert!(!s.signal_history_enabled());
    assert!(
        matches!(
            s.put_signal_history(&id, b"body", 1),
            Err(StoreError::Unsupported(_))
        ),
        "accepting and discarding would mislead an operator relying on replay"
    );
}

#[test]
fn history_enabled_round_trips() {
    let s = SqliteStore::in_memory_with_history().expect("open");
    s.migrate().expect("migrate");
    let id = SignalId::from_parts(1_700_000_000_000, 1);
    s.put_signal_history(&id, b"payload", 1).expect("put");
    assert_eq!(
        s.get_signal_history(&id).expect("get").as_deref(),
        Some(&b"payload"[..])
    );
}

#[test]
fn far_future_timestamps_do_not_wrap() {
    // SQLite integers are signed; a u64 near the maximum must not become
    // negative and re-order.
    let s = store();
    s.put_synapse(&SynapseRecord {
        id: SynapseId("future".into()),
        peer: NodeId(vec![9u8; 32]),
        weight: 0.5,
        attenuation_factor: 0.9,
        state: SynapseState::Active,
        type_affinity: std::collections::HashMap::new(),
        established_at_ns: u64::MAX,
        last_active_ns: u64::MAX,
        signals_transmitted: u64::MAX,
        signals_received: 0,
        avg_latency_ns: 0,
        error_rate: 0.0,
    })
    .expect("put");

    let read = s
        .get_synapse(&SynapseId("future".into()))
        .expect("read")
        .expect("present");
    assert!(
        read.last_active_ns > 0,
        "a far-future timestamp must not wrap to a negative value"
    );
}

//! End-to-end tests for the closed learning loop.
//!
//! These exercise the property the whole design rests on: **an outcome
//! flowing back changes what the node does next.** Unit tests cover each
//! piece; these check that the pieces compose.

use std::sync::Arc;

use ntl_core::delivery::{DeliveryClass, Receipt, RejectReason};
use ntl_core::learning::DeploymentClass;
use ntl_core::signal::{NodeId, Signal};
use ntl_core::store::{Outcome, SynapseFilter};
use ntl_core::testing::{test_node, test_node_id, test_node_with_config, TEST_EPOCH_NS};
use ntl_core::{NodeConfig, NodeStore};

/// Wire a node up to `n` peers and return their identities.
fn with_peers(node: &ntl_core::Node, n: u8) -> Vec<NodeId> {
    (1..=n)
        .map(|i| {
            let peer = test_node_id(i);
            node.upsert_synapse(&peer).expect("synapse");
            peer
        })
        .collect()
}

fn weight_of(node: &ntl_core::Node, peer: &NodeId) -> f32 {
    node.store()
        .synapse_for_peer(peer)
        .expect("store")
        .expect("synapse must exist")
        .weight
}

#[test]
fn a_delivered_receipt_strengthens_the_path_that_carried_it() {
    let (node, _clock) = test_node(200);
    let peers = with_peers(&node, 1);
    let before = weight_of(&node, &peers[0]);

    let signal = node
        .emit(Signal::data("order").with_weight(0.8).acknowledged())
        .expect("emit");

    // Route it, which journals the decision.
    let arrived = incoming(&signal, 9);
    let disposition = node.receive(&arrived, None).expect("receive");
    assert!(
        !disposition.forward_to.is_empty(),
        "with one active synapse the signal should be forwarded"
    );
    let forward = &disposition.forward_to[0];

    // The peer acknowledges.
    let receipt = Receipt::delivered(arrived.id, 1);
    let update = node
        .apply_receipt(&receipt, &forward.peer)
        .expect("apply receipt")
        .expect("a matching decision must be found");

    assert!(update.applied_delta > 0.0, "delivery must strengthen");
    assert!(
        weight_of(&node, &peers[0]) > before,
        "the synapse that delivered should carry more weight than before"
    );
}

#[test]
fn a_rejection_weakens_the_path() {
    let (node, _clock) = test_node(201);
    let peers = with_peers(&node, 1);

    // Give it headroom to fall from.
    let signal = node
        .emit(Signal::data("order").with_weight(0.9).acknowledged())
        .expect("emit");
    let arrived = incoming(&signal, 9);
    let disposition = node.receive(&arrived, None).expect("receive");
    let forward = &disposition.forward_to[0];
    let before = weight_of(&node, &peers[0]);

    let receipt = Receipt::rejected(arrived.id, RejectReason::NoRoute, 1);
    let update = node
        .apply_receipt(&receipt, &forward.peer)
        .expect("apply")
        .expect("matched");

    assert!(update.applied_delta < 0.0, "rejection must weaken");
    assert!(weight_of(&node, &peers[0]) < before);
}

#[test]
fn a_replayed_receipt_has_no_second_effect() {
    let (node, _clock) = test_node(202);
    with_peers(&node, 1);

    let signal = node
        .emit(Signal::data("order").with_weight(0.8).acknowledged())
        .expect("emit");
    let arrived = incoming(&signal, 9);
    let forward = node.receive(&arrived, None).expect("receive").forward_to[0].clone();

    let receipt = Receipt::delivered(arrived.id, 1);
    let first = node
        .apply_receipt(&receipt, &forward.peer)
        .expect("apply")
        .expect("matched");
    let after_first = first.after;

    // At-least-once delivery means duplicate receipts are routine.
    let second = node.apply_receipt(&receipt, &forward.peer).expect("apply");
    assert!(
        second.is_none(),
        "a replayed receipt must not resolve the decision again"
    );

    let peer = test_node_id(1);
    assert!(
        (weight_of(&node, &peer) - after_first).abs() < 1e-6,
        "the weight must be unchanged by the replay"
    );
}

#[test]
fn an_unmatched_receipt_is_discarded_without_changing_weights() {
    let (node, _clock) = test_node(203);
    let peers = with_peers(&node, 1);
    let before = weight_of(&node, &peers[0]);

    // A receipt for a signal this node never routed — i.e. a forgery.
    let forged = Receipt::delivered(
        ntl_core::SignalId::from_parts(TEST_EPOCH_NS / 1_000_000, 12345),
        1,
    );
    assert!(
        node.apply_receipt(&forged, &peers[0])
            .expect("apply")
            .is_none(),
        "a receipt matching no journalled decision must be discarded"
    );
    assert!(
        (weight_of(&node, &peers[0]) - before).abs() < f32::EPSILON,
        "forged receipts must not move weights — this is the cheapest \
         possible attack on the model"
    );
}

#[test]
fn silence_becomes_a_negative_reward_after_the_window() {
    let (node, clock) = test_node(204);
    let peers = with_peers(&node, 1);

    let signal = node
        .emit(Signal::data("order").with_weight(0.9).acknowledged())
        .expect("emit");
    let arrived = incoming(&signal, 9);
    node.receive(&arrived, None).expect("receive");
    let before = weight_of(&node, &peers[0]);

    // Nothing resolves before the window elapses.
    assert_eq!(
        node.sweep_timeouts(10).expect("sweep"),
        0,
        "a decision inside its window must not be timed out"
    );

    clock.advance_secs(node.config().learning.receipt_window_secs + 1);
    let resolved = node.sweep_timeouts(10).expect("sweep");

    assert_eq!(resolved, 1, "the expired decision must be resolved");
    assert!(
        weight_of(&node, &peers[0]) < before,
        "silence must cost the path something, or failure is unlearnable"
    );
}

#[test]
fn the_network_learns_which_peer_delivers() {
    // The headline claim: repeated evidence moves traffic toward the peer
    // that actually delivers.
    let (node, clock) = test_node(205);
    let good = test_node_id(1);
    let bad = test_node_id(2);
    node.upsert_synapse(&good).expect("synapse");
    node.upsert_synapse(&bad).expect("synapse");

    let mut good_chosen = 0;
    let mut bad_chosen = 0;

    for round in 0..300 {
        let signal = node
            .emit(Signal::data("query").with_weight(0.8).acknowledged())
            .expect("emit");
        let arrived = incoming(&signal, 9);
        let disposition = node.receive(&arrived, None).expect("receive");

        for forward in &disposition.forward_to {
            if forward.peer == good {
                good_chosen += 1;
                let r = Receipt::delivered(arrived.id, 1);
                node.apply_receipt(&r, &forward.peer).expect("apply");
            } else {
                bad_chosen += 1;
                let r = Receipt::rejected(arrived.id, RejectReason::NoRoute, 1);
                node.apply_receipt(&r, &forward.peer).expect("apply");
            }
        }
        // Advance a little so refractory periods and dedup do not interfere.
        clock.advance_secs(1);
        if round % 50 == 0 {
            node.checkpoint().expect("checkpoint");
        }
    }

    let good_weight = weight_of(&node, &good);
    let bad_weight = weight_of(&node, &bad);

    assert!(
        good_weight > bad_weight,
        "the delivering peer must end stronger: good={good_weight}, bad={bad_weight}"
    );
    assert!(
        bad_chosen > 0,
        "the failing peer must have been tried at all — otherwise the test \
         proves nothing about learning"
    );
    assert!(
        good_chosen > bad_chosen,
        "traffic should have shifted toward the delivering peer: \
         good={good_chosen}, bad={bad_chosen}"
    );
}

#[test]
fn exploration_keeps_probing_a_weak_path() {
    // Anti-ossification, end to end: a synapse that starts weak must still
    // be tried, or it can never recover.
    let (node, clock) = test_node(206);
    let strong = test_node_id(1);
    let weak = test_node_id(2);
    node.upsert_synapse(&strong).expect("synapse");
    node.upsert_synapse(&weak).expect("synapse");

    // Drive the strong synapse up.
    for _ in 0..40 {
        let signal = node
            .emit(Signal::data("q").with_weight(0.9).acknowledged())
            .expect("emit");
        let arrived = incoming(&signal, 9);
        for f in node.receive(&arrived, None).expect("receive").forward_to {
            if f.peer == strong {
                node.apply_receipt(&Receipt::delivered(arrived.id, 1), &f.peer)
                    .expect("apply");
            }
        }
        clock.advance_secs(1);
    }

    // Now count how often the weak synapse still gets tried.
    let mut weak_tried = 0;
    for _ in 0..300 {
        let signal = node.emit(Signal::data("q").with_weight(0.5)).expect("emit");
        let arrived = incoming(&signal, 9);
        for f in node.receive(&arrived, None).expect("receive").forward_to {
            if f.peer == weak {
                weak_tried += 1;
            }
        }
        clock.advance_secs(1);
    }

    assert!(
        weak_tried > 0,
        "a weak synapse must still be probed; without exploration its weight \
         only ever decays and routing ossifies"
    );
}

#[test]
fn a_failing_path_is_deprioritised_but_still_reachable() {
    // Regression for a real bug: with initial_weight and the Active floor
    // both at 0.1, a single rejection dropped a synapse to Weakening. When
    // routing filtered on `state == Active`, that synapse became permanently
    // unreachable — and since weight is only earned by carrying traffic, it
    // could never recover. Routing ossified around whichever peer happened
    // to succeed first.
    let (node, clock) = test_node(219);
    let good = test_node_id(1);
    let bad = test_node_id(2);
    node.upsert_synapse(&good).expect("synapse");
    node.upsert_synapse(&bad).expect("synapse");

    let mut bad_tried = 0;
    for _ in 0..300 {
        let signal = node
            .emit(Signal::data("q").with_weight(0.8).acknowledged())
            .expect("emit");
        let arrived = incoming(&signal, 9);
        for f in node.receive(&arrived, None).expect("receive").forward_to {
            if f.peer == bad {
                bad_tried += 1;
                node.apply_receipt(&Receipt::rejected(arrived.id, RejectReason::NoRoute, 1), &f.peer)
                    .expect("apply");
            } else {
                node.apply_receipt(&Receipt::delivered(arrived.id, 1), &f.peer)
                    .expect("apply");
            }
        }
        clock.advance_secs(1);
    }

    assert!(
        bad_tried > 1,
        "the failing peer must keep being probed rather than being excluded \
         after its first failure; it was tried {bad_tried} times"
    );
    assert!(
        weight_of(&node, &good) > weight_of(&node, &bad) * 5.0,
        "the delivering peer should end clearly stronger: good={}, bad={}",
        weight_of(&node, &good),
        weight_of(&node, &bad)
    );
}

#[test]
fn total_outbound_weight_stays_within_budget() {
    let (node, clock) = test_node_with_config(207, NodeConfig::for_class(DeploymentClass::Edge));
    let peers = with_peers(&node, 20);

    // Reward everything relentlessly. Without normalisation every weight
    // would saturate and the scoring function would stop discriminating.
    for _ in 0..200 {
        let signal = node
            .emit(Signal::data("q").with_weight(1.0).acknowledged())
            .expect("emit");
        let arrived = incoming(&signal, 9);
        for f in node.receive(&arrived, None).expect("receive").forward_to {
            node.apply_receipt(&Receipt::delivered(arrived.id, 1), &f.peer)
                .expect("apply");
        }
        clock.advance_secs(1);
    }

    let total: f32 = peers.iter().map(|p| weight_of(&node, p)).sum();
    let budget = node.config().learning.max_total_outbound_weight;
    assert!(
        total <= budget + 0.05,
        "total outbound weight {total} must stay within the budget {budget}"
    );
}

#[test]
fn influence_cap_bounds_a_single_peer() {
    let (node, clock) = test_node(208);
    let attacker = test_node_id(1);
    node.upsert_synapse(&attacker).expect("synapse");

    let cap = node.config().learning.influence_cap_per_peer;

    // Flood favourable outcomes from one identity.
    for _ in 0..500 {
        let signal = node
            .emit(Signal::data("q").with_weight(1.0).acknowledged())
            .expect("emit");
        let arrived = incoming(&signal, 9);
        for f in node.receive(&arrived, None).expect("receive").forward_to {
            node.apply_receipt(&Receipt::delivered(arrived.id, 1), &f.peer)
                .expect("apply");
        }
        // Stay inside one influence window.
        clock.advance_secs(1);
    }

    let used = node
        .store()
        .influence_since(
            &attacker,
            node.config()
                .learning
                .influence_window_start(node.now_ns()),
        )
        .expect("influence");

    assert!(
        used <= cap + 0.01,
        "one identity accumulated {used} influence against a cap of {cap}"
    );
}

#[test]
fn dedup_stops_a_signal_being_processed_twice() {
    let (node, _clock) = test_node(209);
    with_peers(&node, 2);

    let signal = node.emit(Signal::data("q").with_weight(0.9)).expect("emit");
    let arrived = incoming(&signal, 9);

    let first = node.receive(&arrived, None).expect("receive");
    let second = node.receive(&arrived, None).expect("receive");

    assert!(!first.forward_to.is_empty(), "the first arrival should route");
    assert!(
        second.forward_to.is_empty() && second.handle_locally.is_empty(),
        "a repeat arrival must be dropped, or cyclic topologies loop forever"
    );
}

#[test]
fn a_signal_is_never_sent_back_where_it_came_from() {
    let (node, _clock) = test_node(210);
    let peers = with_peers(&node, 3);
    let arrival = node
        .store()
        .synapse_for_peer(&peers[0])
        .expect("store")
        .expect("synapse")
        .id;

    for i in 0..30u8 {
        let signal = node
            .emit(Signal::data("q").with_weight(0.9))
            .expect("emit");
        let arrived = incoming(&signal, 100 + i);
        let disposition = node.receive(&arrived, Some(&arrival)).expect("receive");
        assert!(
            disposition.forward_to.iter().all(|f| f.synapse != arrival),
            "the arrival synapse must be excluded from forwarding"
        );
    }
}

#[test]
fn an_acknowledged_signal_with_no_route_gets_a_negative_receipt() {
    // No synapses at all: nowhere to forward.
    let (node, _clock) = test_node(211);

    let signal = node
        .emit(Signal::data("order").with_weight(0.9).acknowledged())
        .expect("emit");
    let arrived = incoming(&signal, 9);
    let disposition = node.receive(&arrived, None).expect("receive");

    let receipt = disposition
        .receipt
        .expect("an acknowledged signal must never fail silently");
    assert!(!receipt.is_delivered());
    assert_eq!(receipt.correlation_id, arrived.id);
}

#[test]
fn a_best_effort_signal_with_no_route_fails_silently() {
    let (node, _clock) = test_node(212);

    let signal = node.emit(Signal::data("telemetry").with_weight(0.9)).expect("emit");
    let arrived = incoming(&signal, 9);
    let disposition = node.receive(&arrived, None).expect("receive");

    assert!(
        disposition.receipt.is_none(),
        "best-effort is allowed to absorb silently — that is the distinction"
    );
}

#[test]
fn an_acknowledged_signal_below_the_weight_floor_is_reported() {
    let (node, _clock) = test_node(213);
    with_peers(&node, 1);

    let mut arrived = incoming(
        &node
            .emit(Signal::data("order").with_weight(0.9).acknowledged())
            .expect("emit"),
        9,
    );
    arrived.weight = 0.0001; // below min_propagation_weight

    let disposition = node.receive(&arrived, None).expect("receive");
    let receipt = disposition.receipt.expect("must not be absorbed silently");
    assert!(!receipt.is_delivered());
    assert_eq!(disposition.rejected, Some(RejectReason::BelowThreshold));
}

#[test]
fn decay_erodes_an_idle_synapse() {
    let (node, clock) = test_node(214);
    let peers = with_peers(&node, 1);

    // Build the weight up.
    for _ in 0..20 {
        let signal = node
            .emit(Signal::data("q").with_weight(0.9).acknowledged())
            .expect("emit");
        let arrived = incoming(&signal, 9);
        for f in node.receive(&arrived, None).expect("receive").forward_to {
            node.apply_receipt(&Receipt::delivered(arrived.id, 1), &f.peer)
                .expect("apply");
        }
        clock.advance_secs(1);
    }
    let peak = weight_of(&node, &peers[0]);

    // Two half-lives of silence.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let half_life_hours = node.config().learning.decay_half_life_hours as u64;
    clock.advance_hours(half_life_hours * 2);

    let record = node
        .store()
        .synapse_for_peer(&peers[0])
        .expect("store")
        .expect("synapse");
    let decayed = ntl_core::learning::decayed_weight(
        record.weight,
        record.last_active_ns,
        node.now_ns(),
        &node.config().learning,
    );

    assert!(
        decayed < peak * 0.4,
        "after two half-lives the weight should be near a quarter: \
         peak={peak}, decayed={decayed}"
    );
}

#[test]
fn learning_health_reports_exploration_and_pending() {
    let (node, clock) = test_node(215);
    with_peers(&node, 4);

    for _ in 0..40 {
        let signal = node
            .emit(Signal::data("q").with_weight(0.9).acknowledged())
            .expect("emit");
        let arrived = incoming(&signal, 9);
        node.receive(&arrived, None).expect("receive");
        clock.advance_secs(1);
    }

    let health = node.learning_health(200).expect("health");
    assert!(health.decisions_sampled > 0);
    assert!(
        health.pending_ratio > 0.5,
        "with no receipts arriving, most decisions should read as pending — \
         this is the signal that a node's weights reflect nothing"
    );
    assert!((0.0..=1.0).contains(&health.exploration_ratio));
}

#[test]
fn identity_and_learned_weights_survive_a_restart() {
    // Losing weights on restart would discard everything the node learned.
    let store: Arc<dyn NodeStore> = Arc::new(ntl_core::store::MemoryStore::new());
    let clock = Arc::new(ntl_core::time::ManualClock::starting_at(TEST_EPOCH_NS));
    let peer = test_node_id(1);

    let (identity, weight_before) = {
        let node = ntl_core::Node::builder()
            .with_config(NodeConfig::default())
            .with_store(store.clone())
            .with_clock(clock.clone())
            .build()
            .expect("build");
        node.upsert_synapse(&peer).expect("synapse");

        for _ in 0..20 {
            let signal = node
                .emit(Signal::data("q").with_weight(0.9).acknowledged())
                .expect("emit");
            let arrived = incoming(&signal, 9);
            for f in node.receive(&arrived, None).expect("receive").forward_to {
                node.apply_receipt(&Receipt::delivered(arrived.id, 1), &f.peer)
                    .expect("apply");
            }
            clock.advance_secs(1);
        }
        node.checkpoint().expect("checkpoint");
        (
            node.identity().clone(),
            store
                .synapse_for_peer(&peer)
                .expect("store")
                .expect("synapse")
                .weight,
        )
    };

    // Restart against the same store.
    let restarted = ntl_core::Node::builder()
        .with_config(NodeConfig::default())
        .with_store(store.clone())
        .with_clock(clock.clone())
        .build()
        .expect("rebuild");

    assert_eq!(
        restarted.identity(),
        &identity,
        "a node must keep its identity, or it loses every synapse it formed"
    );
    let after = store
        .synapse_for_peer(&peer)
        .expect("store")
        .expect("synapse")
        .weight;
    assert!(
        (after - weight_before).abs() < 1e-6,
        "learned weight must survive a restart: {weight_before} -> {after}"
    );
}

#[test]
fn a_signature_failure_costs_more_than_a_timeout() {
    let (node, _clock) = test_node(216);
    let peers = with_peers(&node, 1);
    let synapse_id = node
        .store()
        .synapse_for_peer(&peers[0])
        .expect("store")
        .expect("synapse")
        .id;

    let before = weight_of(&node, &peers[0]);
    node.penalize_signature_failure(&synapse_id).expect("penalize");
    let after = weight_of(&node, &peers[0]);

    assert!(
        after <= before * 0.6,
        "a signature failure should roughly halve the weight: {before} -> {after}"
    );
}

#[test]
fn outcome_rewards_are_ordered_as_specified() {
    // A guard on the reward table itself: the ordering is load-bearing for
    // how the model behaves on lossy links.
    assert!(Outcome::Delivered.reward() > 0.0);
    assert_eq!(Outcome::Pending.reward(), 0.0);
    assert!(Outcome::TimedOut.reward() < 0.0);
    assert!(
        Outcome::TimedOut.reward() > Outcome::Rejected.reward(),
        "ambiguous silence must cost less than definite rejection"
    );
    assert!(
        Outcome::SignatureFailure.reward() <= Outcome::Rejected.reward(),
        "a signature failure is the strongest negative signal"
    );
}

#[test]
fn active_synapse_listing_is_weight_ordered() {
    let (node, clock) = test_node(217);
    with_peers(&node, 6);

    for i in 0..60u8 {
        let signal = node
            .emit(Signal::data("q").with_weight(0.9).acknowledged())
            .expect("emit");
        let arrived = incoming(&signal, 50 + i % 5);
        for f in node.receive(&arrived, None).expect("receive").forward_to {
            node.apply_receipt(&Receipt::delivered(arrived.id, 1), &f.peer)
                .expect("apply");
        }
        clock.advance_secs(1);
    }

    let listed = node
        .store()
        .list_synapses(&SynapseFilter::active())
        .expect("list");
    for w in listed.windows(2) {
        assert!(
            w[0].weight >= w[1].weight,
            "routing decisions depend on a stable weight ordering"
        );
    }
}

#[test]
fn delivery_class_survives_a_wire_roundtrip() {
    // An intermediate node must not be able to downgrade the guarantee.
    let (node, _clock) = test_node(218);
    let signal = node
        .emit(Signal::data("order").with_weight(0.8).acknowledged())
        .expect("emit");

    let bytes = signal.encode().expect("encode");
    let decoded = Signal::decode(&bytes).expect("decode");

    assert_eq!(decoded.delivery, DeliveryClass::Acknowledged);
    assert_eq!(decoded.id, signal.id);
    assert!(decoded.requires_receipt());
}

/// Present a signal as though it arrived from elsewhere.
///
/// A node marks its own emissions as seen, so re-presenting the same signal
/// would be deduplicated. Tests that exercise the receive path therefore
/// restamp the origin and identifier.
fn incoming(signal: &Signal, origin: u8) -> Signal {
    let mut arrived = signal.clone();
    arrived.origin = test_node_id(origin);
    // A fresh identifier so the node's own dedup claim does not swallow it.
    arrived.id = ntl_core::SignalId::from_parts(
        signal.id.timestamp_ms(),
        u128::from(origin) << 96 | u128::from(signal.id.timestamp_ms()),
    );
    arrived
}

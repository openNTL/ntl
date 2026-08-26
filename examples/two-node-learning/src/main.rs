//! Two nodes on loopback, and a synapse weight that visibly changes.
//!
//! Run it:
//!
//! ```text
//! cargo run -p ntl-example-two-node-learning
//! ```
//!
//! What it shows: a node routes acknowledged signals across two peers. One
//! delivers, one refuses. Each returning receipt resolves the journalled
//! routing decision and updates that synapse's weight. The numbers printed
//! are the routing model itself, changing in response to evidence.
//!
//! Two details are easy to miss and both matter.
//!
//! **Time is simulated.** The run advances an injected clock by an hour per
//! round. That is not cosmetic: a per-identity influence cap bounds how much
//! any one peer can move a weight inside one window (one hour by default), so
//! a burst of a thousand signals in a millisecond would correctly move almost
//! nothing. Learning is rate-limited on purpose, and a demo that hid that
//! would be lying about the protocol.
//!
//! **The weight goes down as well as up.** A model that only ever rises is
//! not learning, it is counting.

use std::sync::Arc;

use ntl_core::delivery::{Receipt, RejectReason};
use ntl_core::signal::{NodeId, Signal, SignalId};
use ntl_core::store::MemoryStore;
use ntl_core::time::ManualClock;
use ntl_core::{Node, NodeConfig, NodeStore};

/// Rounds per phase. One simulated hour each.
const ROUNDS: usize = 12;

/// Simulated time per round. Matches the default influence window, so each
/// round gets a fresh per-peer budget.
const HOURS_PER_ROUND: u64 = 1;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let clock = Arc::new(ManualClock::starting_at(1_700_000_000 * 1_000_000_000));
    let store: Arc<dyn NodeStore> = Arc::new(MemoryStore::new());

    let node = Node::builder()
        .with_config(NodeConfig::default())
        .with_store(store)
        .with_clock(clock.clone())
        .build()?;

    // Both peers start identical, so nothing about the outcome is preordained.
    let good = NodeId(vec![1u8; 32]);
    let bad = NodeId(vec![2u8; 32]);
    node.upsert_synapse(&good)?;
    node.upsert_synapse(&bad)?;

    let learning = &node.config().learning;
    println!("NTL two-node learning demo\n");
    println!("  node          {}", short(node.identity()));
    println!("  peer A        {}", short(&good));
    println!("  peer B        {}", short(&bad));
    println!("  learning rate {}", learning.learning_rate);
    println!(
        "  influence cap {} per peer per hour",
        learning.influence_cap_per_peer
    );
    println!("  simulated     {HOURS_PER_ROUND}h per round\n");

    println!("Phase 1 — both peers deliver. Both weights rise.");
    run_phase(&node, &clock, &good, &bad, true)?;
    let (a1, b1) = (weight(&node, &good)?, weight(&node, &bad)?);

    println!("\nPhase 2 — peer B starts refusing every signal.");
    run_phase(&node, &clock, &good, &bad, false)?;
    let (a2, b2) = (weight(&node, &good)?, weight(&node, &bad)?);

    println!("\nWhat changed");
    println!("  peer A  {a1:.4} -> {a2:.4}  ({:+.4})", a2 - a1);
    println!("  peer B  {b1:.4} -> {b2:.4}  ({:+.4})", b2 - b1);

    // Assert the point rather than asserting that a point was made.
    if b2 < b1 && a2 >= a1 {
        println!("\n  The refusing peer lost weight; the delivering peer kept it.");
        println!("  Nothing configured that. The only input was the receipts.");
    } else {
        println!("\n  Weights did not separate this run — see the health readout below.");
    }

    let health = node.learning_health(1_000)?;
    println!("\nModel health");
    println!("  delivered   {:.0}%", health.delivery_ratio * 100.0);
    println!("  exploratory {:.0}%", health.exploration_ratio * 100.0);
    println!("  pending     {:.0}%", health.pending_ratio * 100.0);

    if health.exploration_ratio > 0.0 {
        println!();
        println!("  Exploration stayed above zero, so peer B is still tried");
        println!("  occasionally and could recover if it started delivering.");
        println!("  That is what stops routing ossifying around early winners.");
    }
    Ok(())
}

fn run_phase(
    node: &Node,
    clock: &ManualClock,
    good: &NodeId,
    bad: &NodeId,
    b_delivers: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("  {:>5}  {:>9}  {:>9}  chosen", "round", "peer A", "peer B");
    for round in 1..=ROUNDS {
        let chosen = drive_round(node, good, bad, b_delivers, round)?;
        println!(
            "  {round:>5}  {:>9.4}  {:>9.4}  {chosen}",
            weight(node, good)?,
            weight(node, bad)?
        );
        clock.advance_hours(HOURS_PER_ROUND);
    }
    Ok(())
}

/// Emit one signal, resolve every decision it produced, and report which
/// peers were chosen.
fn drive_round(
    node: &Node,
    good: &NodeId,
    bad: &NodeId,
    b_delivers: bool,
    round: usize,
) -> Result<String, Box<dyn std::error::Error>> {
    // A distinct identifier per round, so deduplication does not swallow it.
    let arriving = SignalId::from_parts(node.now_ns() / 1_000_000, (round as u128) << 64 | 0xABCD);

    let mut signal = Signal::data("demo")
        .with_payload(serde_json::json!({"round": round}))
        .with_weight(0.9)
        .acknowledged()
        .build_unsigned(NodeId(vec![9u8; 32]));
    signal.id = arriving;
    signal.signature = vec![0u8; 64];

    let mut chosen = Vec::new();
    for forward in node.receive(&signal, None)?.forward_to {
        let (label, receipt) = if &forward.peer == good {
            ("A", Receipt::delivered(arriving, 1))
        } else if b_delivers {
            ("B", Receipt::delivered(arriving, 1))
        } else {
            ("B✗", Receipt::rejected(arriving, RejectReason::NoRoute, 1))
        };
        let tag = if forward.explored {
            format!("{label}*")
        } else {
            label.to_string()
        };
        chosen.push(tag);
        node.apply_receipt(&receipt, &forward.peer)?;
    }
    let _ = bad;

    Ok(if chosen.is_empty() {
        "-".to_string()
    } else {
        chosen.join(" ")
    })
}

fn weight(node: &Node, peer: &NodeId) -> Result<f32, Box<dyn std::error::Error>> {
    Ok(node
        .store()
        .synapse_for_peer(peer)?
        .map_or(0.0, |r| r.weight))
}

fn short(id: &NodeId) -> String {
    id.0.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

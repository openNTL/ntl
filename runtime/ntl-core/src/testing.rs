//! Test utilities and fixtures.
//!
//! These are deliberately deterministic: no ambient clock, no ambient
//! randomness. A test failure here reproduces exactly, and the module builds
//! on every target the crate does.

use std::sync::Arc;

use crate::config::NodeConfig;
use crate::delivery::DeliveryClass;
use crate::node::Node;
use crate::rng::SplitMix64;
use crate::signal::{NodeId, Signal, SignalType};
use crate::store::MemoryStore;
use crate::time::ManualClock;

/// A fixed instant used as the base for test timelines: 2023-11-14T22:13:20Z.
pub const TEST_EPOCH_NS: u64 = 1_700_000_000 * 1_000_000_000;

/// Create a test [`NodeId`] with a recognisable byte pattern.
#[must_use]
pub fn test_node_id(id: u8) -> NodeId {
    NodeId(vec![id; 32])
}

/// Create a deterministic unsigned test signal.
#[must_use]
pub fn test_signal(signal_type: SignalType, weight: f32) -> Signal {
    test_signal_from(signal_type, weight, 0)
}

/// Create a deterministic unsigned test signal with a chosen origin and seed.
#[must_use]
pub fn test_signal_from(signal_type: SignalType, weight: f32, origin: u8) -> Signal {
    let builder = match &signal_type {
        SignalType::Data => Signal::data("test"),
        SignalType::Query => Signal::query("test"),
        SignalType::Event => Signal::event("test"),
        SignalType::Command => Signal::command("test"),
        SignalType::Discovery => Signal::discovery(),
        SignalType::Heartbeat => Signal::heartbeat(),
        SignalType::Receipt | SignalType::Custom(_) => Signal::data("test"),
    };

    let clock = ManualClock::starting_at(TEST_EPOCH_NS);
    let mut rng = SplitMix64::seeded(u64::from(origin) + 1);
    let mut signal =
        builder
            .with_weight(weight)
            .build_unsigned_with(test_node_id(origin), &clock, &mut rng);
    // A placeholder signature, so validation that only checks presence passes.
    signal.signature = vec![0u8; 64];
    signal
}

/// Create a deterministic acknowledged test signal.
#[must_use]
pub fn test_acknowledged_signal(weight: f32) -> Signal {
    let mut signal = test_signal(SignalType::Data, weight);
    signal.delivery = DeliveryClass::Acknowledged;
    signal
}

/// A node backed by an in-memory store and a clock the test controls.
///
/// Returns the node alongside its clock, so a test can advance time to
/// exercise decay, refractory periods, and receipt timeouts without sleeping.
///
/// # Panics
/// Panics if the node cannot be built, which would mean the shipped defaults
/// are invalid.
#[must_use]
pub fn test_node(identity: u8) -> (Node, Arc<ManualClock>) {
    test_node_with_config(identity, NodeConfig::default())
}

/// As [`test_node`], with explicit configuration.
///
/// # Panics
/// Panics if the configuration is invalid.
#[must_use]
pub fn test_node_with_config(identity: u8, config: NodeConfig) -> (Node, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::starting_at(TEST_EPOCH_NS));
    let node = Node::builder()
        .with_config(config)
        .with_identity(test_node_id(identity))
        .with_store(Arc::new(MemoryStore::new()))
        .with_clock(clock.clone())
        .with_rng(Box::new(SplitMix64::seeded(u64::from(identity) + 1)))
        .build()
        .expect("test node must build");
    (node, clock)
}

//! Activation model — admission control and backpressure.
//!
//! Implements [spec/activation-model][spec]. Replaces rate limiting with a
//! threshold that rises under load, a bounded queue, and batched processing.
//!
//! [spec]: https://openntl.org/spec/activation-model
//!
//! Two things 0.1.0-draft left ambiguous are settled here.
//!
//! **What firing processes.** Potential accumulates across many signals, then
//! the node "fires (processes the signal)" — which signal? When potential is
//! a sum of contributions, the singular is meaningless, and an implementation
//! that processes only the threshold-crossing signal leaks the rest. Firing
//! now drains a *batch* in weight order.
//!
//! **Whether the queue is bounded.** `load_factor = queue_depth /
//! max_queue_depth` implies a bound, while "no signals are dropped (they
//! queue)" denies one. The queue is bounded, and [`OverflowPolicy`] says what
//! happens when it fills.

use serde::{Deserialize, Serialize};

use crate::delivery::{DeliveryClass, RejectReason};
use crate::rng::Rng;
use crate::signal::SignalId;
use crate::store::ActivationSnapshot;

/// Activation function type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum ActivationFunction {
    /// Binary: fires when potential >= threshold.
    #[default]
    Step,
    /// Probabilistic: firing probability rises smoothly through the threshold.
    Sigmoid,
    /// Like [`Self::Step`], but always passes a small fraction.
    Leaky,
}

/// Hardware class of a node, selecting activation defaults.
///
/// A single global refractory period cannot serve both a solar-powered
/// sensor and a datacentre node: 10 ms caps a node at 100 fires per second,
/// which on server hardware is a self-inflicted throughput ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum NodeClass {
    /// Battery- or CPU-constrained device.
    #[default]
    Edge,
    /// General-purpose node.
    Standard,
    /// Server-class node; no artificial fire ceiling.
    Server,
    /// Bootstrap or infrastructure node.
    Infrastructure,
}

/// What happens when a signal arrives at a full queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum OverflowPolicy {
    /// Drop whichever of the arriving and weakest-queued signal is lighter.
    ///
    /// The default: weight means importance everywhere else in the protocol,
    /// and under load is exactly when honouring it matters.
    #[default]
    DropLowestWeight,
    /// Drop the arriving signal.
    DropNewest,
    /// Drop the oldest queued signal to admit the arriving one.
    DropOldest,
}

// Fields added after 0.1.0-draft carry serde defaults, so upgrading NTL does
// not refuse to start against a config written by an older build. An operator
// gets edge-class defaults for anything absent and can regenerate with
// `ntl init --force`.
fn default_node_class() -> NodeClass {
    NodeClass::Edge
}
fn default_fire_batch_size() -> usize {
    8
}
fn default_max_queue_depth() -> usize {
    256
}
fn default_leak_rate() -> f32 {
    0.01
}
fn default_max_queue_latency_ms() -> u64 {
    1_000
}

/// Configuration for the activation model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivationConfig {
    /// Hardware class, which supplies the defaults below.
    #[serde(default = "default_node_class")]
    pub node_class: NodeClass,
    /// Base activation threshold.
    pub base_threshold: f32,
    /// Activation function.
    pub activation_function: ActivationFunction,
    /// Refractory period in milliseconds. `0` disables it.
    pub refractory_period_ms: u64,
    /// Ceiling on accumulated potential.
    pub max_potential: f32,
    /// Whether the threshold rises with load.
    pub dynamic_threshold: bool,
    /// Signals drained per fire. MUST be at least 1.
    #[serde(default = "default_fire_batch_size")]
    pub fire_batch_size: usize,
    /// Queue bound. `load_factor` is defined against this.
    #[serde(default = "default_max_queue_depth")]
    pub max_queue_depth: usize,
    /// What to drop when the queue is full.
    #[serde(default)]
    pub overflow_policy: OverflowPolicy,
    /// Leak probability for [`ActivationFunction::Leaky`].
    #[serde(default = "default_leak_rate")]
    pub leak_rate: f32,
    /// Longest a signal may wait in the queue before the node fires anyway,
    /// in milliseconds. `0` disables the guard.
    ///
    /// Without this a node can starve a signal indefinitely. Contribution is
    /// `signal_weight × synapse_weight`, but the threshold is absolute, so a
    /// new synapse at weight `0.1` carrying a weight-`0.45` signal
    /// contributes `0.045` against a threshold of `0.5` — the node cannot
    /// fire on that signal no matter how long it waits, and no amount of
    /// idleness helps.
    ///
    /// That is fatal for acknowledged delivery: the sender's receipt window
    /// expires, the decision resolves as `TimedOut`, and the routing model
    /// learns to avoid a path that was working perfectly. The guard bounds
    /// queue latency so backpressure stays a delay rather than becoming a
    /// silent drop.
    #[serde(default = "default_max_queue_latency_ms")]
    pub max_queue_latency_ms: u64,
}

impl ActivationConfig {
    /// Defaults for a node class.
    #[must_use]
    pub fn for_class(class: NodeClass) -> Self {
        let (refractory_ms, batch, depth, max_potential, latency_guard_ms) = match class {
            NodeClass::Edge => (10, 8, 256, 10.0, 1_000),
            NodeClass::Standard => (1, 16, 1_024, 10.0, 500),
            NodeClass::Server => (0, 64, 8_192, 50.0, 250),
            NodeClass::Infrastructure => (0, 128, 32_768, 100.0, 100),
        };
        Self {
            node_class: class,
            base_threshold: 0.5,
            activation_function: ActivationFunction::Step,
            refractory_period_ms: refractory_ms,
            max_potential,
            dynamic_threshold: true,
            fire_batch_size: batch,
            max_queue_depth: depth,
            overflow_policy: OverflowPolicy::DropLowestWeight,
            leak_rate: 0.01,
            max_queue_latency_ms: latency_guard_ms,
        }
    }

    /// Validate the configuration.
    ///
    /// # Errors
    /// Returns a description of the first invalid field.
    pub fn validate(&self) -> Result<(), String> {
        if self.fire_batch_size == 0 {
            return Err(
                "fire_batch_size must be at least 1, or the node can never process anything"
                    .to_string(),
            );
        }
        if self.max_queue_depth == 0 {
            return Err("max_queue_depth must be at least 1".to_string());
        }
        if self.base_threshold <= 0.0 {
            return Err(format!(
                "base_threshold must be positive, got {}",
                self.base_threshold
            ));
        }
        if self.max_potential < self.base_threshold {
            return Err(format!(
                "max_potential ({}) must be at least base_threshold ({}), \
                 or the node can never reach its threshold",
                self.max_potential, self.base_threshold
            ));
        }
        if !(0.0..=1.0).contains(&self.leak_rate) {
            return Err(format!(
                "leak_rate must be in [0, 1], got {}",
                self.leak_rate
            ));
        }
        if self.max_queue_latency_ms > 0 && self.max_queue_latency_ms < self.refractory_period_ms {
            return Err(format!(
                "max_queue_latency_ms ({}) must be at least refractory_period_ms ({}),                  or the guard can never fire",
                self.max_queue_latency_ms, self.refractory_period_ms
            ));
        }
        Ok(())
    }
}

impl Default for ActivationConfig {
    fn default() -> Self {
        Self::for_class(NodeClass::Edge)
    }
}

/// A signal waiting in the activation queue.
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedSignal {
    /// Which signal.
    pub id: SignalId,
    /// Where it came from.
    ///
    /// Carried so a signal released by the starvation guard can still be
    /// acknowledged: the receipt needs somewhere to go, and by then the full
    /// signal body may be long gone.
    pub origin: crate::signal::NodeId,
    /// Its weight, which orders both processing and overflow.
    pub weight: f32,
    /// Its delivery class, which decides whether a drop must be reported.
    pub delivery: DeliveryClass,
    /// When it was enqueued.
    pub enqueued_at_ns: u64,
}

/// What happened when a signal was offered to the gate.
#[derive(Debug, Clone, PartialEq)]
pub struct AdmitOutcome {
    /// Signals to process now. Empty if the node did not fire.
    pub fired: Vec<QueuedSignal>,
    /// A signal dropped to make room, if any.
    ///
    /// When its delivery class is `Acknowledged`, the caller MUST emit a
    /// negative receipt with [`Self::drop_reason`]. Overload is not an
    /// exemption from the delivery guarantee — it is the case the guarantee
    /// exists for.
    pub dropped: Option<QueuedSignal>,
    /// Why the drop happened.
    pub drop_reason: Option<RejectReason>,
}

impl AdmitOutcome {
    /// Whether the node fired.
    #[must_use]
    pub fn did_fire(&self) -> bool {
        !self.fired.is_empty()
    }

    /// Whether a drop needs a negative receipt.
    #[must_use]
    pub fn needs_receipt(&self) -> bool {
        self.dropped
            .as_ref()
            .is_some_and(|d| d.delivery.requires_receipt())
    }
}

/// The activation gate.
#[derive(Debug)]
pub struct ActivationState {
    potential: f32,
    threshold: f32,
    base_threshold: f32,
    refractory_until_ns: u64,
    refractory_period_ns: u64,
    function: ActivationFunction,
    max_potential: f32,
    dynamic: bool,
    fire_batch_size: usize,
    max_queue_depth: usize,
    overflow_policy: OverflowPolicy,
    leak_rate: f32,
    max_queue_latency_ns: u64,
    queue: Vec<QueuedSignal>,
    signals_fired: u64,
    signals_dropped: u64,
    overflow_events: u64,
}

impl ActivationState {
    /// Create a gate from configuration.
    #[must_use]
    pub fn new(config: &ActivationConfig) -> Self {
        Self {
            potential: 0.0,
            threshold: config.base_threshold,
            base_threshold: config.base_threshold,
            refractory_until_ns: 0,
            refractory_period_ns: config.refractory_period_ms.saturating_mul(1_000_000),
            function: config.activation_function,
            max_potential: config.max_potential,
            dynamic: config.dynamic_threshold,
            fire_batch_size: config.fire_batch_size.max(1),
            max_queue_depth: config.max_queue_depth.max(1),
            overflow_policy: config.overflow_policy,
            leak_rate: config.leak_rate,
            max_queue_latency_ns: config.max_queue_latency_ms.saturating_mul(1_000_000),
            queue: Vec::new(),
            signals_fired: 0,
            signals_dropped: 0,
            overflow_events: 0,
        }
    }

    /// Restore from a persisted snapshot.
    ///
    /// The queue is not restored — in-flight signals are not durable — but
    /// potential, threshold, and the refractory deadline are. Discarding them
    /// would make a restart a free reset of backpressure.
    pub fn restore(&mut self, snapshot: &ActivationSnapshot) {
        self.potential = snapshot.potential.clamp(0.0, self.max_potential);
        self.threshold = snapshot.threshold;
        self.refractory_until_ns = snapshot.refractory_until_ns;
        self.signals_fired = snapshot.signals_fired;
    }

    /// Capture a snapshot for persistence.
    #[must_use]
    pub fn snapshot(&self, now_ns: u64) -> ActivationSnapshot {
        ActivationSnapshot {
            potential: self.potential,
            threshold: self.threshold,
            refractory_until_ns: self.refractory_until_ns,
            signals_fired: self.signals_fired,
            taken_at_ns: now_ns,
        }
    }

    /// Offer a signal to the gate.
    ///
    /// The signal is enqueued (subject to overflow), its contribution added to
    /// potential, and — if the gate fires — a batch is drained and returned.
    pub fn admit(
        &mut self,
        signal: QueuedSignal,
        synapse_weight: f32,
        now_ns: u64,
        rng: &mut dyn Rng,
    ) -> AdmitOutcome {
        let contribution = signal.weight * synapse_weight;
        self.potential = (self.potential + contribution).min(self.max_potential);

        let (dropped, drop_reason) = self.enqueue(signal, now_ns);

        if self.dynamic {
            self.recompute_threshold();
        }

        let fired = if self.in_refractory(now_ns) {
            // Refractory always wins: it is the one bound that protects the
            // hardware, and the guard below only decides *whether* to fire
            // once firing is permitted at all.
            Vec::new()
        } else if self.evaluate(rng) || self.is_starving(now_ns) {
            self.fire(now_ns)
        } else {
            Vec::new()
        };

        AdmitOutcome {
            fired,
            dropped,
            drop_reason,
        }
    }

    /// Enqueue, applying the overflow policy if the queue is full.
    fn enqueue(
        &mut self,
        signal: QueuedSignal,
        _now_ns: u64,
    ) -> (Option<QueuedSignal>, Option<RejectReason>) {
        if self.queue.len() < self.max_queue_depth {
            self.queue.push(signal);
            return (None, None);
        }

        self.overflow_events += 1;
        self.signals_dropped += 1;

        let dropped = match self.overflow_policy {
            OverflowPolicy::DropNewest => signal,
            OverflowPolicy::DropOldest => {
                // By enqueue time, not by position. `fire` sorts the queue by
                // weight descending, so index 0 is the *heaviest* remaining
                // signal — evicting it did the opposite of what the policy
                // name promises, and under load it would systematically
                // discard the traffic the operator cared most about.
                let oldest = self
                    .queue
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, s)| (s.enqueued_at_ns, s.id))
                    .map(|(i, _)| i);
                match oldest {
                    Some(idx) => {
                        let evicted = self.queue.remove(idx);
                        self.queue.push(signal);
                        evicted
                    }
                    // Only reachable with max_queue_depth == 0, where there is
                    // nothing to evict and the arrival is the drop.
                    None => signal,
                }
            }
            OverflowPolicy::DropLowestWeight => {
                // Find the weakest queued signal. Ties drop the arrival, so a
                // flood of equal-weight signals cannot displace what is
                // already committed.
                let weakest = self
                    .queue
                    .iter()
                    .enumerate()
                    .min_by(|a, b| {
                        a.1.weight
                            .partial_cmp(&b.1.weight)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|(i, s)| (i, s.weight));

                match weakest {
                    Some((idx, weight)) if weight < signal.weight => {
                        let evicted = self.queue.remove(idx);
                        self.queue.push(signal);
                        evicted
                    }
                    _ => signal,
                }
            }
        };

        (Some(dropped), Some(RejectReason::QueueFull))
    }

    /// Drain a batch in weight order and reset potential.
    fn fire(&mut self, now_ns: u64) -> Vec<QueuedSignal> {
        // Weight descending; ties broken on identifier so the order is total
        // and reproducible.
        self.queue.sort_by(|a, b| {
            b.weight
                .partial_cmp(&a.weight)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });

        let take = self.fire_batch_size.min(self.queue.len());
        let batch: Vec<QueuedSignal> = self.queue.drain(..take).collect();

        // Potential resets once per fire, not once per signal: a batch of
        // sixteen costs the same refractory period as a batch of one, which is
        // what makes batching worth doing under load.
        self.potential = 0.0;
        self.refractory_until_ns = now_ns.saturating_add(self.refractory_period_ns);
        self.signals_fired += batch.len() as u64;
        batch
    }

    /// Evaluate the activation function.
    fn evaluate(&self, rng: &mut dyn Rng) -> bool {
        if self.queue.is_empty() {
            return false;
        }
        match self.function {
            ActivationFunction::Step => self.potential >= self.threshold,
            ActivationFunction::Sigmoid => {
                let probability = 1.0 / (1.0 + (-10.0 * (self.potential - self.threshold)).exp());
                rng.next_f32() < probability
            }
            ActivationFunction::Leaky => {
                self.potential >= self.threshold || rng.next_f32() < self.leak_rate
            }
        }
    }

    /// Recompute the threshold from current load.
    fn recompute_threshold(&mut self) {
        self.threshold = self.base_threshold * (1.0 + self.load_factor());
    }

    /// Whether the oldest queued signal has waited past the latency guard.
    ///
    /// This is what stops backpressure from becoming an indefinite hold. See
    /// [`ActivationConfig::max_queue_latency_ms`] for why an absolute
    /// threshold alone can starve a signal forever.
    #[must_use]
    pub fn is_starving(&self, now_ns: u64) -> bool {
        if self.max_queue_latency_ns == 0 {
            return false;
        }
        self.queue
            .iter()
            .any(|s| now_ns.saturating_sub(s.enqueued_at_ns) >= self.max_queue_latency_ns)
    }

    /// Whether the gate is refractory.
    #[must_use]
    pub fn in_refractory(&self, now_ns: u64) -> bool {
        now_ns < self.refractory_until_ns
    }

    /// Current queue occupancy as a fraction of the bound, in `[0, 1]`.
    #[must_use]
    pub fn load_factor(&self) -> f32 {
        #[allow(clippy::cast_precision_loss)]
        let ratio = self.queue.len() as f32 / self.max_queue_depth as f32;
        ratio.clamp(0.0, 1.0)
    }

    /// Adjust the threshold for an externally measured load.
    pub fn adjust_for_load(&mut self, load_factor: f32) {
        if self.dynamic {
            self.threshold = self.base_threshold * (1.0 + load_factor.clamp(0.0, 1.0));
        }
    }

    /// Drain a batch only if the queue is starving.
    ///
    /// Called from periodic maintenance: the guard in [`Self::admit`] can only
    /// act when another signal arrives, so a queue that goes quiet while
    /// holding a below-threshold signal needs this to release it.
    pub fn drain_if_starving(&mut self, now_ns: u64) -> Vec<QueuedSignal> {
        if self.in_refractory(now_ns) || !self.is_starving(now_ns) {
            return Vec::new();
        }
        self.fire(now_ns)
    }

    /// Drain a batch without waiting for the threshold.
    ///
    /// For shutdown, and for a node draining a backlog after its refractory
    /// period under a policy that would otherwise starve the queue.
    pub fn drain_batch(&mut self, now_ns: u64) -> Vec<QueuedSignal> {
        if self.queue.is_empty() {
            return Vec::new();
        }
        self.fire(now_ns)
    }

    /// Accumulated potential.
    #[must_use]
    pub fn potential(&self) -> f32 {
        self.potential
    }

    /// Effective threshold.
    #[must_use]
    pub fn threshold(&self) -> f32 {
        self.threshold
    }

    /// Signals waiting.
    #[must_use]
    pub fn queue_depth(&self) -> usize {
        self.queue.len()
    }

    /// Total signals processed.
    #[must_use]
    pub fn fire_count(&self) -> u64 {
        self.signals_fired
    }

    /// Total signals dropped to overflow.
    #[must_use]
    pub fn dropped_count(&self) -> u64 {
        self.signals_dropped
    }

    /// Number of times the queue was full on arrival.
    ///
    /// Regular overflow means misconfiguration: either the queue is too small
    /// for the traffic, or the dynamic threshold is off. Exposing the counter
    /// makes that visible rather than inferred.
    #[must_use]
    pub fn overflow_count(&self) -> u64 {
        self.overflow_events
    }

    /// Reset all state.
    pub fn reset(&mut self) {
        self.potential = 0.0;
        self.threshold = self.base_threshold;
        self.refractory_until_ns = 0;
        self.queue.clear();
        self.signals_fired = 0;
        self.signals_dropped = 0;
        self.overflow_events = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::SplitMix64;

    const MS: u64 = 1_000_000;

    fn cfg() -> ActivationConfig {
        ActivationConfig {
            refractory_period_ms: 0,
            dynamic_threshold: false,
            fire_batch_size: 4,
            max_queue_depth: 8,
            // Disabled so threshold behaviour can be tested in isolation;
            // the guard has its own tests below.
            max_queue_latency_ms: 0,
            ..ActivationConfig::for_class(NodeClass::Edge)
        }
    }

    fn sig(n: u64, weight: f32) -> QueuedSignal {
        QueuedSignal {
            id: SignalId::from_parts(1_700_000_000_000 + n, u128::from(n) << 64),
            origin: crate::signal::NodeId(vec![9u8; 32]),
            weight,
            delivery: DeliveryClass::BestEffort,
            enqueued_at_ns: 0,
        }
    }

    fn ack_sig(n: u64, weight: f32) -> QueuedSignal {
        QueuedSignal {
            delivery: DeliveryClass::Acknowledged,
            ..sig(n, weight)
        }
    }

    // -- config ------------------------------------------------------------

    #[test]
    fn a_config_missing_new_fields_still_parses() {
        // Upgrading NTL must not refuse to start against a config written by
        // an older build.
        let old_style = r#"
            base_threshold = 0.5
            activation_function = "step"
            refractory_period_ms = 10
            max_potential = 10.0
            dynamic_threshold = true
        "#;
        let parsed: ActivationConfig =
            toml::from_str(old_style).expect("a 0.1.0-era config must still parse");
        assert_eq!(parsed.node_class, NodeClass::Edge);
        assert!(parsed.fire_batch_size >= 1);
        assert!(
            parsed.max_queue_latency_ms > 0,
            "the starvation guard must be on by default, or an upgraded node \
             can silently hold signals forever"
        );
        parsed.validate().expect("defaults must be valid");
    }

    #[test]
    fn node_class_defaults_scale_with_capability() {
        let edge = ActivationConfig::for_class(NodeClass::Edge);
        let server = ActivationConfig::for_class(NodeClass::Server);

        assert_eq!(edge.refractory_period_ms, 10);
        assert_eq!(
            server.refractory_period_ms, 0,
            "a server-class node must not be capped at 100 fires/second"
        );
        assert!(server.fire_batch_size > edge.fire_batch_size);
        assert!(server.max_queue_depth > edge.max_queue_depth);
    }

    #[test]
    fn all_class_defaults_validate() {
        for c in [
            NodeClass::Edge,
            NodeClass::Standard,
            NodeClass::Server,
            NodeClass::Infrastructure,
        ] {
            ActivationConfig::for_class(c)
                .validate()
                .unwrap_or_else(|e| panic!("{c:?} invalid: {e}"));
        }
    }

    #[test]
    fn validate_rejects_zero_batch_size() {
        let c = ActivationConfig {
            fire_batch_size: 0,
            ..cfg()
        };
        assert!(c.validate().unwrap_err().contains("fire_batch_size"));
    }

    #[test]
    fn validate_rejects_unreachable_threshold() {
        let c = ActivationConfig {
            base_threshold: 100.0,
            max_potential: 1.0,
            ..cfg()
        };
        assert!(c.validate().is_err());
    }

    // -- firing ------------------------------------------------------------

    #[test]
    fn fires_once_potential_reaches_threshold() {
        let mut st = ActivationState::new(&cfg());
        let mut rng = SplitMix64::seeded(1);

        let out = st.admit(sig(1, 0.3), 1.0, 0, &mut rng);
        assert!(!out.did_fire(), "0.3 < 0.5 must not fire");
        assert_eq!(st.queue_depth(), 1);

        let out = st.admit(sig(2, 0.3), 1.0, 0, &mut rng);
        assert!(out.did_fire(), "0.6 >= 0.5 must fire");
        assert!((st.potential() - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn firing_drains_a_batch_not_one_signal() {
        // The 0.1.0-draft ambiguity: with potential summed across many
        // signals, processing only the threshold-crosser leaks the rest.
        let mut st = ActivationState::new(&cfg());
        let mut rng = SplitMix64::seeded(2);

        for i in 0..4 {
            st.admit(sig(i, 0.1), 1.0, 0, &mut rng);
        }
        assert_eq!(st.queue_depth(), 4, "below threshold, all should queue");

        let out = st.admit(sig(99, 0.5), 1.0, 0, &mut rng);
        assert!(out.did_fire());
        assert_eq!(
            out.fired.len(),
            4,
            "a fire must drain up to fire_batch_size, not a single signal"
        );
        assert_eq!(st.queue_depth(), 1, "the remainder must stay queued");
    }

    #[test]
    fn batch_is_ordered_by_weight_descending() {
        // A threshold above anything reachable keeps every signal queued, so
        // ordering can be checked in isolation from firing.
        let mut st = ActivationState::new(&ActivationConfig {
            base_threshold: 1_000.0,
            ..cfg()
        });
        let mut rng = SplitMix64::seeded(3);
        for (i, w) in [(1, 0.1), (2, 0.9), (3, 0.4), (4, 0.7)] {
            assert!(!st.admit(sig(i, w), 1.0, 0, &mut rng).did_fire());
        }
        let batch = st.drain_batch(0);
        let weights: Vec<f32> = batch.iter().map(|s| s.weight).collect();
        assert_eq!(
            weights,
            vec![0.9, 0.7, 0.4, 0.1],
            "the highest-weight signals must be processed first"
        );
    }

    #[test]
    fn leftover_signals_are_not_discarded() {
        // Conservation: every admitted signal is eventually either fired or
        // explicitly dropped. Nothing may vanish.
        let mut st = ActivationState::new(&ActivationConfig {
            fire_batch_size: 2,
            max_queue_depth: 16,
            ..cfg()
        });
        let mut rng = SplitMix64::seeded(4);

        const ADMITTED: usize = 20;
        let mut fired = 0;
        let mut dropped = 0;

        for i in 0..ADMITTED as u64 {
            let out = st.admit(sig(i, 0.4), 1.0, 0, &mut rng);
            fired += out.fired.len();
            if out.dropped.is_some() {
                dropped += 1;
            }
        }
        // Drain whatever remains.
        loop {
            let batch = st.drain_batch(0);
            if batch.is_empty() {
                break;
            }
            fired += batch.len();
        }

        assert_eq!(st.queue_depth(), 0, "the queue should be fully drained");
        assert_eq!(
            fired + dropped,
            ADMITTED,
            "every signal must be accounted for: {fired} fired + {dropped} \
             dropped should equal {ADMITTED} admitted"
        );
    }

    #[test]
    fn potential_is_capped() {
        let mut st = ActivationState::new(&ActivationConfig {
            base_threshold: 1_000.0,
            max_potential: 5.0,
            ..cfg()
        });
        let mut rng = SplitMix64::seeded(5);
        for i in 0..100 {
            st.admit(sig(i, 1.0), 1.0, 0, &mut rng);
        }
        assert!(st.potential() <= 5.0);
    }

    #[test]
    fn synapse_weight_modulates_contribution() {
        let mut st = ActivationState::new(&cfg());
        let mut rng = SplitMix64::seeded(6);
        let out = st.admit(sig(1, 1.0), 0.1, 0, &mut rng);
        assert!(!out.did_fire(), "1.0 * 0.1 = 0.1 < 0.5");
        assert!((st.potential() - 0.1).abs() < 1e-6);
    }

    #[test]
    fn empty_queue_never_fires() {
        let mut st = ActivationState::new(&cfg());
        let mut rng = SplitMix64::seeded(7);
        // Force potential high, then drain, then confirm no spurious fire.
        st.admit(sig(1, 1.0), 1.0, 0, &mut rng);
        assert!(st.drain_batch(0).len() <= 1);
        assert!(st.drain_batch(0).is_empty());
    }

    // -- refractory --------------------------------------------------------

    #[test]
    fn refractory_period_blocks_firing() {
        let mut st = ActivationState::new(&ActivationConfig {
            refractory_period_ms: 10,
            ..cfg()
        });
        let mut rng = SplitMix64::seeded(8);

        let out = st.admit(sig(1, 1.0), 1.0, 0, &mut rng);
        assert!(out.did_fire());
        assert!(st.in_refractory(5 * MS));

        let out = st.admit(sig(2, 1.0), 1.0, 5 * MS, &mut rng);
        assert!(
            !out.did_fire(),
            "must not fire inside the refractory period"
        );
        assert_eq!(st.queue_depth(), 1, "but the signal must still queue");

        let out = st.admit(sig(3, 1.0), 1.0, 11 * MS, &mut rng);
        assert!(out.did_fire(), "must fire once the period elapses");
    }

    #[test]
    fn zero_refractory_allows_back_to_back_fires() {
        let mut st = ActivationState::new(&ActivationConfig {
            refractory_period_ms: 0,
            ..cfg()
        });
        let mut rng = SplitMix64::seeded(9);
        for i in 0..5 {
            assert!(st.admit(sig(i, 1.0), 1.0, 0, &mut rng).did_fire());
        }
    }

    // -- queue bound and overflow -----------------------------------------

    #[test]
    fn load_factor_stays_in_unit_interval() {
        let mut st = ActivationState::new(&ActivationConfig {
            base_threshold: 1_000.0,
            max_queue_depth: 4,
            ..cfg()
        });
        let mut rng = SplitMix64::seeded(10);
        assert!((st.load_factor() - 0.0).abs() < f32::EPSILON);
        for i in 0..20 {
            st.admit(sig(i, 0.1), 1.0, 0, &mut rng);
            assert!((0.0..=1.0).contains(&st.load_factor()));
        }
        assert!((st.load_factor() - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn queue_is_bounded() {
        let mut st = ActivationState::new(&ActivationConfig {
            base_threshold: 1_000.0, // never fires
            max_queue_depth: 4,
            ..cfg()
        });
        let mut rng = SplitMix64::seeded(11);
        for i in 0..50 {
            st.admit(sig(i, 0.5), 1.0, 0, &mut rng);
        }
        assert_eq!(st.queue_depth(), 4, "the queue must respect its bound");
        assert!(st.overflow_count() > 0);
    }

    #[test]
    fn drop_lowest_weight_evicts_the_weakest() {
        let mut st = ActivationState::new(&ActivationConfig {
            base_threshold: 1_000.0,
            max_queue_depth: 3,
            overflow_policy: OverflowPolicy::DropLowestWeight,
            ..cfg()
        });
        let mut rng = SplitMix64::seeded(12);
        st.admit(sig(1, 0.9), 1.0, 0, &mut rng);
        st.admit(sig(2, 0.1), 1.0, 0, &mut rng);
        st.admit(sig(3, 0.8), 1.0, 0, &mut rng);

        let out = st.admit(sig(4, 0.5), 1.0, 0, &mut rng);
        let dropped = out.dropped.expect("queue was full");
        assert!(
            (dropped.weight - 0.1).abs() < f32::EPSILON,
            "the weakest queued signal should be evicted, not the arrival"
        );
        assert_eq!(out.drop_reason, Some(RejectReason::QueueFull));
    }

    #[test]
    fn drop_lowest_weight_rejects_a_weaker_arrival() {
        let mut st = ActivationState::new(&ActivationConfig {
            base_threshold: 1_000.0,
            max_queue_depth: 2,
            overflow_policy: OverflowPolicy::DropLowestWeight,
            ..cfg()
        });
        let mut rng = SplitMix64::seeded(13);
        st.admit(sig(1, 0.9), 1.0, 0, &mut rng);
        st.admit(sig(2, 0.8), 1.0, 0, &mut rng);

        let out = st.admit(sig(3, 0.01), 1.0, 0, &mut rng);
        let dropped = out.dropped.expect("queue was full");
        assert_eq!(
            dropped.id,
            sig(3, 0.01).id,
            "the weak arrival should be dropped"
        );
    }

    #[test]
    fn equal_weight_flood_cannot_displace_committed_signals() {
        let mut st = ActivationState::new(&ActivationConfig {
            base_threshold: 1_000.0,
            max_queue_depth: 2,
            overflow_policy: OverflowPolicy::DropLowestWeight,
            ..cfg()
        });
        let mut rng = SplitMix64::seeded(14);
        st.admit(sig(1, 0.5), 1.0, 0, &mut rng);
        st.admit(sig(2, 0.5), 1.0, 0, &mut rng);

        for i in 10..20 {
            let out = st.admit(sig(i, 0.5), 1.0, 0, &mut rng);
            assert_eq!(
                out.dropped.map(|d| d.id),
                Some(sig(i, 0.5).id),
                "on a tie the arrival is dropped, so a flood cannot evict \
                 what is already queued"
            );
        }
    }

    #[test]
    fn drop_newest_and_oldest_behave_as_named() {
        let mut rng = SplitMix64::seeded(15);

        let mut newest = ActivationState::new(&ActivationConfig {
            base_threshold: 1_000.0,
            max_queue_depth: 2,
            overflow_policy: OverflowPolicy::DropNewest,
            ..cfg()
        });
        newest.admit(sig(1, 0.1), 1.0, 0, &mut rng);
        newest.admit(sig(2, 0.1), 1.0, 0, &mut rng);
        let out = newest.admit(sig(3, 0.9), 1.0, 0, &mut rng);
        assert_eq!(out.dropped.map(|d| d.id), Some(sig(3, 0.9).id));

        let mut oldest = ActivationState::new(&ActivationConfig {
            base_threshold: 1_000.0,
            max_queue_depth: 2,
            overflow_policy: OverflowPolicy::DropOldest,
            ..cfg()
        });
        oldest.admit(sig(1, 0.1), 1.0, 0, &mut rng);
        oldest.admit(sig(2, 0.1), 1.0, 0, &mut rng);
        let out = oldest.admit(sig(3, 0.9), 1.0, 0, &mut rng);
        assert_eq!(out.dropped.map(|d| d.id), Some(sig(1, 0.1).id));
    }

    #[test]
    fn drop_oldest_evicts_by_arrival_time_not_by_position() {
        // The existing overflow test never fires (base_threshold 1000), so the
        // queue is only ever in push order and position happens to match
        // arrival order. `fire` sorts by weight descending and drains a batch,
        // so a *partly* drained queue is left in weight order — and index 0 is
        // then the heaviest remaining signal, not the oldest. Evicting it
        // inverted the policy, discarding exactly the traffic an operator
        // cares most about.
        let mut rng = SplitMix64::seeded(41);
        let mut st = ActivationState::new(&ActivationConfig {
            base_threshold: 0.5,
            fire_batch_size: 1,
            max_queue_depth: 3,
            overflow_policy: OverflowPolicy::DropOldest,
            ..cfg()
        });

        let at = |n: u64, w: f32, t: u64| QueuedSignal {
            enqueued_at_ns: t,
            ..sig(n, w)
        };

        // Build up to a fire. Ascending weights, so arrival order and weight
        // order are opposites.
        let a = at(1, 0.10, 1_000);
        let b = at(2, 0.20, 2_000);
        st.admit(a.clone(), 1.0, 1_000, &mut rng);
        st.admit(b.clone(), 1.0, 2_000, &mut rng);
        let fired = st.admit(at(3, 0.30, 3_000), 1.0, 3_000, &mut rng);
        assert_eq!(fired.fired.len(), 1, "0.10+0.20+0.30 should cross 0.5");
        assert_eq!(st.queue_depth(), 2, "a batch of 1 leaves two behind");
        // The remainder is now [b, a] — weight order, the reverse of arrival.

        // Fill the last slot without firing again.
        st.admit(at(4, 0.05, 4_000), 1.0, 4_000, &mut rng);
        assert_eq!(st.queue_depth(), 3);

        let out = st.admit(at(5, 0.05, 5_000), 1.0, 5_000, &mut rng);
        let dropped = out.dropped.expect("the queue was full, so something went");
        assert_eq!(
            dropped.id, a.id,
            "DropOldest must evict the earliest arrival; index 0 was the \
             heaviest remaining signal"
        );
    }

    #[test]
    fn acknowledged_drop_demands_a_receipt() {
        let mut st = ActivationState::new(&ActivationConfig {
            base_threshold: 1_000.0,
            max_queue_depth: 1,
            overflow_policy: OverflowPolicy::DropNewest,
            ..cfg()
        });
        let mut rng = SplitMix64::seeded(16);
        st.admit(sig(1, 0.5), 1.0, 0, &mut rng);

        let out = st.admit(ack_sig(2, 0.5), 1.0, 0, &mut rng);
        assert!(
            out.needs_receipt(),
            "overload is not an exemption from the delivery guarantee"
        );
        assert_eq!(out.drop_reason, Some(RejectReason::QueueFull));
    }

    #[test]
    fn best_effort_drop_needs_no_receipt() {
        let mut st = ActivationState::new(&ActivationConfig {
            base_threshold: 1_000.0,
            max_queue_depth: 1,
            overflow_policy: OverflowPolicy::DropNewest,
            ..cfg()
        });
        let mut rng = SplitMix64::seeded(17);
        st.admit(sig(1, 0.5), 1.0, 0, &mut rng);
        let out = st.admit(sig(2, 0.5), 1.0, 0, &mut rng);
        assert!(out.dropped.is_some());
        assert!(!out.needs_receipt());
    }

    // -- starvation guard --------------------------------------------------

    #[test]
    fn a_signal_below_threshold_eventually_fires_anyway() {
        // Regression for a real starvation bug. Contribution is
        // signal_weight * synapse_weight, but the threshold is absolute, so a
        // fresh synapse at 0.1 carrying a 0.45 signal contributes 0.045
        // against 0.5 and can never cross it. Before the guard, such a signal
        // was queued forever, the sender's receipt window expired, and the
        // model learned to avoid a path that worked.
        let mut st = ActivationState::new(&ActivationConfig {
            max_queue_latency_ms: 1_000,
            ..cfg()
        });
        let mut rng = SplitMix64::seeded(30);

        let out = st.admit(sig(1, 0.45), 0.1, 0, &mut rng);
        assert!(!out.did_fire(), "0.045 is far below the 0.5 threshold");
        assert_eq!(st.queue_depth(), 1);

        // Still inside the guard window.
        let out = st.admit(sig(2, 0.45), 0.1, 500 * MS, &mut rng);
        assert!(!out.did_fire());

        // Past it.
        let out = st.admit(sig(3, 0.45), 0.1, 1_100 * MS, &mut rng);
        assert!(
            out.did_fire(),
            "a signal must not be starved indefinitely; backpressure is a \
             delay, not a silent drop"
        );
    }

    #[test]
    fn the_guard_respects_the_refractory_period() {
        let mut st = ActivationState::new(&ActivationConfig {
            max_queue_latency_ms: 100,
            refractory_period_ms: 50,
            ..cfg()
        });
        let mut rng = SplitMix64::seeded(31);

        // Fire once to enter refractory.
        assert!(st.admit(sig(1, 1.0), 1.0, 0, &mut rng).did_fire());
        // A starving signal must still wait out the refractory period: that
        // bound is what protects the hardware.
        let out = st.admit(sig(2, 0.01), 0.01, 20 * MS, &mut rng);
        assert!(!out.did_fire(), "refractory must win over the guard");
    }

    #[test]
    fn a_zero_guard_disables_it() {
        let mut st = ActivationState::new(&ActivationConfig {
            max_queue_latency_ms: 0,
            ..cfg()
        });
        let mut rng = SplitMix64::seeded(32);
        st.admit(sig(1, 0.01), 0.01, 0, &mut rng);
        assert!(!st.is_starving(u64::MAX / 2));
        assert!(
            !st.admit(sig(2, 0.01), 0.01, u64::MAX / 2, &mut rng)
                .did_fire()
        );
    }

    #[test]
    fn every_class_default_enables_the_guard() {
        for c in [
            NodeClass::Edge,
            NodeClass::Standard,
            NodeClass::Server,
            NodeClass::Infrastructure,
        ] {
            let config = ActivationConfig::for_class(c);
            assert!(
                config.max_queue_latency_ms > 0,
                "{c:?} must not ship able to starve a signal forever"
            );
            assert!(config.max_queue_latency_ms >= config.refractory_period_ms);
        }
    }

    #[test]
    fn validate_rejects_a_guard_shorter_than_the_refractory_period() {
        let c = ActivationConfig {
            max_queue_latency_ms: 5,
            refractory_period_ms: 50,
            ..cfg()
        };
        assert!(c.validate().is_err());
    }

    // -- dynamic threshold -------------------------------------------------

    #[test]
    fn threshold_rises_with_load() {
        let mut st = ActivationState::new(&ActivationConfig {
            dynamic_threshold: true,
            ..cfg()
        });
        st.adjust_for_load(1.0);
        assert!(
            (st.threshold() - 1.0).abs() < f32::EPSILON,
            "0.5 * (1 + 1.0)"
        );
        st.adjust_for_load(0.0);
        assert!((st.threshold() - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn static_threshold_ignores_load() {
        let mut st = ActivationState::new(&ActivationConfig {
            dynamic_threshold: false,
            ..cfg()
        });
        st.adjust_for_load(1.0);
        assert!((st.threshold() - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn dynamic_threshold_provides_backpressure() {
        // Under sustained load the threshold should rise above its base,
        // making the node more selective.
        let mut st = ActivationState::new(&ActivationConfig {
            dynamic_threshold: true,
            base_threshold: 0.5,
            max_queue_depth: 10,
            fire_batch_size: 1,
            ..cfg()
        });
        let mut rng = SplitMix64::seeded(18);
        for i in 0..9 {
            st.admit(sig(i, 0.05), 1.0, 0, &mut rng);
        }
        assert!(
            st.threshold() > 0.5,
            "a loaded node must become more selective, got {}",
            st.threshold()
        );
    }

    // -- activation functions ---------------------------------------------

    #[test]
    fn leaky_passes_occasionally_below_threshold() {
        let mut st = ActivationState::new(&ActivationConfig {
            activation_function: ActivationFunction::Leaky,
            base_threshold: 100.0,
            leak_rate: 0.5,
            fire_batch_size: 1,
            ..cfg()
        });
        let mut rng = SplitMix64::seeded(19);
        let mut fires = 0;
        for i in 0..200 {
            if st.admit(sig(i, 0.001), 1.0, 0, &mut rng).did_fire() {
                fires += 1;
            }
        }
        assert!(fires > 50, "leaky should pass a fraction; got {fires}");
        assert!(fires < 200, "leaky should not pass everything");
    }

    #[test]
    fn sigmoid_is_probabilistic_around_the_threshold() {
        let mut st = ActivationState::new(&ActivationConfig {
            activation_function: ActivationFunction::Sigmoid,
            fire_batch_size: 1,
            ..cfg()
        });
        let mut rng = SplitMix64::seeded(20);
        let mut fires = 0;
        for i in 0..500 {
            if st.admit(sig(i, 0.5), 1.0, 0, &mut rng).did_fire() {
                fires += 1;
            }
        }
        assert!(
            fires > 0 && fires < 500,
            "sigmoid should be stochastic; got {fires}"
        );
    }

    // -- persistence -------------------------------------------------------

    #[test]
    fn snapshot_roundtrip_preserves_backpressure() {
        let mut st = ActivationState::new(&ActivationConfig {
            refractory_period_ms: 10,
            base_threshold: 1_000.0,
            ..cfg()
        });
        let mut rng = SplitMix64::seeded(21);
        st.admit(sig(1, 0.8), 1.0, 0, &mut rng);
        let snap = st.snapshot(1_000);

        let mut restored = ActivationState::new(&ActivationConfig {
            refractory_period_ms: 10,
            base_threshold: 1_000.0,
            ..cfg()
        });
        restored.restore(&snap);
        assert!((restored.potential() - st.potential()).abs() < 1e-6);
        assert_eq!(restored.threshold(), st.threshold());
    }

    #[test]
    fn restore_carries_the_refractory_deadline() {
        // A restart must not be a free reset of backpressure.
        let snap = ActivationSnapshot {
            potential: 0.0,
            threshold: 0.5,
            refractory_until_ns: 50 * MS,
            signals_fired: 7,
            taken_at_ns: 10 * MS,
        };
        let mut st = ActivationState::new(&cfg());
        st.restore(&snap);
        assert!(
            st.in_refractory(20 * MS),
            "the deadline must survive a restart"
        );
        assert_eq!(st.fire_count(), 7);
    }

    #[test]
    fn restore_clamps_potential_to_the_configured_max() {
        let snap = ActivationSnapshot {
            potential: 1e9,
            threshold: 0.5,
            refractory_until_ns: 0,
            signals_fired: 0,
            taken_at_ns: 0,
        };
        let mut st = ActivationState::new(&cfg());
        st.restore(&snap);
        assert!(
            st.potential() <= cfg().max_potential,
            "a corrupt or foreign snapshot must not exceed local limits"
        );
    }

    #[test]
    fn reset_clears_everything() {
        let mut st = ActivationState::new(&cfg());
        let mut rng = SplitMix64::seeded(22);
        st.admit(sig(1, 1.0), 1.0, 0, &mut rng);
        st.reset();
        assert_eq!(st.queue_depth(), 0);
        assert_eq!(st.fire_count(), 0);
        assert!((st.potential() - 0.0).abs() < f32::EPSILON);
    }
}

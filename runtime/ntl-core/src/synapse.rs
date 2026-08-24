//! Synapse types and lifecycle management for NTL.
//!
//! A synapse is a persistent, weighted connection between two NTL nodes.
//! Synapses strengthen with use and weaken with inactivity.

use serde::{Deserialize, Serialize};

use crate::signal::NodeId;

/// Unique identifier for a synapse.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SynapseId(pub String);

impl std::fmt::Display for SynapseId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Truncate by characters, not bytes. Slicing `&self.0[..8]` panics on
        // a char boundary, and an id is a `TEXT` column: a store written by
        // another implementation may hold anything. A panic in `Display` would
        // surface inside a log line, which is the worst place for one.
        write!(f, "syn:{}", self.0.chars().take(8).collect::<String>())
    }
}

/// What a signature failure did to a synapse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignatureFailureOutcome {
    /// Failures counted in the current influence window, including this one.
    pub failures_in_window: u32,
    /// Whether this failure crossed the threshold and pruned the synapse.
    pub pruned: bool,
}

/// The lifecycle state of a synapse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SynapseState {
    /// Handshake in progress.
    Forming,
    /// Actively transmitting signals.
    Active,
    /// Weight below active threshold, still connected.
    Weakening,
    /// Weight below dormancy threshold, connection idle.
    Dormant,
    /// Connection terminated, state archived.
    Pruned,
}

impl SynapseState {
    /// Whether a synapse in this state may be chosen to carry a signal.
    ///
    /// `Weakening` counts. It means "below the active threshold, **still
    /// connected**" — the connection is intact, the weight is merely low.
    ///
    /// Excluding it would be a correctness bug, not just a tuning choice.
    /// A synapse recovers weight only by carrying traffic that then succeeds
    /// ([`crate::learning`]), so a state that is both reachable by a single
    /// negative outcome *and* ineligible for traffic is a one-way trap: the
    /// synapse can never earn its way back. With `initial_weight` at 0.1 and
    /// the Active floor also at 0.1, every new synapse would fall into that
    /// trap on its first failure, and routing would ossify around whichever
    /// peers happened to succeed first — the exact failure
    /// [spec/learning-model](https://openntl.org/spec/learning-model) §4
    /// exists to prevent.
    ///
    /// `Dormant` and `Pruned` are genuinely unavailable: the connection is
    /// idle or gone, and must be re-established before it can carry anything.
    /// `Forming` has not completed its handshake.
    #[must_use]
    pub fn can_carry(self) -> bool {
        matches!(self, Self::Active | Self::Weakening)
    }

    /// Whether this state is terminal.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Pruned)
    }
}

/// The underlying transport for a synapse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Transport {
    /// QUIC — default, multiplexed, encrypted.
    #[default]
    Quic,
    /// TCP — fallback, widely supported.
    Tcp,
    /// Unix domain socket — same-machine nodes.
    Unix,
    /// Bluetooth Low Energy — proximity mesh.
    BluetoothLe,
    /// Application-defined transport.
    Custom(String),
}

/// An NTL synapse — a persistent, weighted connection between nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Synapse {
    /// Unique synapse identifier.
    pub id: SynapseId,

    /// Local node in this synapse.
    pub local_node: NodeId,

    /// Remote node in this synapse.
    pub remote_node: NodeId,

    /// Current synapse weight (0.0 - 1.0).
    pub weight: f32,

    /// Current lifecycle state.
    pub state: SynapseState,

    /// Underlying transport mechanism.
    pub transport: Transport,

    /// Timestamp when synapse was established (ns since epoch).
    pub established_at_ns: u64,

    /// Timestamp of last signal activity (ns since epoch).
    pub last_active_ns: u64,

    /// Total signals transmitted through this synapse.
    pub signals_transmitted: u64,

    /// Total signals received through this synapse.
    pub signals_received: u64,

    /// Average round-trip latency in nanoseconds.
    pub avg_latency_ns: u64,

    /// Ratio of failed signal transmissions.
    pub error_rate: f32,

    /// Maximum weight this synapse can reach.
    pub max_weight: f32,

    /// Weight decay rate per decay interval.
    pub decay_rate: f32,

    /// Weight at or above which this synapse is `Active`.
    pub active_threshold: f32,

    /// Weight below which this synapse becomes `Dormant`.
    pub dormancy_threshold: f32,

    /// Weight attenuation factor for signals passing through.
    pub attenuation_factor: f32,

    /// Signature failures counted in the current influence window.
    pub signature_failures: u32,

    /// When the current signature-failure window began (ns since epoch).
    pub failure_window_start_ns: u64,

    /// Historical affinity for signal types (type -> success count).
    pub type_affinity: std::collections::HashMap<String, u64>,
}

/// Configuration for synapse behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SynapseConfig {
    /// Initial weight for new synapses.
    pub initial_weight: f32,
    /// Maximum weight a synapse can reach.
    pub max_weight: f32,
    /// Weight decay rate per hour.
    pub decay_rate: f32,
    /// Weight at or above which a synapse is `Active`.
    ///
    /// Named here rather than hard-coded because it and `initial_weight`
    /// coincide at `0.1` by default, which is exactly the boundary
    /// [synapse-lifecycle](https://openntl.org/spec/synapse-lifecycle)
    /// warns about: a freshly formed synapse sits on it, so one rejection
    /// moves it to `Weakening`. An operator retuning one of the two must be
    /// able to see the other.
    pub active_threshold: f32,
    /// Weight threshold below which synapse becomes dormant.
    pub dormancy_threshold: f32,
    /// Hours in dormant state before pruning.
    pub prune_after_hours: u64,
    /// Maximum number of synapses per node.
    pub max_synapses: u32,
    /// Preferred transport.
    pub preferred_transport: Transport,
    /// Fallback transport.
    pub fallback_transport: Transport,
    /// Default attenuation factor.
    pub attenuation_factor: f32,
}

impl Default for SynapseConfig {
    fn default() -> Self {
        Self {
            initial_weight: 0.1,
            max_weight: 1.0,
            decay_rate: 0.01,
            active_threshold: 0.1,
            dormancy_threshold: 0.01,
            prune_after_hours: 168, // 7 days
            max_synapses: 1000,
            preferred_transport: Transport::Quic,
            fallback_transport: Transport::Tcp,
            attenuation_factor: 0.9,
        }
    }
}

impl Synapse {
    /// Create a new synapse in the Forming state.
    ///
    /// Time and randomness are injected so synapse formation is reproducible
    /// in tests and the core stays free of an ambient clock.
    #[must_use]
    pub fn new_with(
        local: NodeId,
        remote: NodeId,
        config: &SynapseConfig,
        now_ns: u64,
        rng: &mut dyn crate::rng::Rng,
    ) -> Self {
        let now = now_ns;
        let id_bits = (u128::from(rng.next_u64()) << 64) | u128::from(rng.next_u64());
        Self {
            id: SynapseId(ulid::Ulid::from_parts(now_ns / 1_000_000, id_bits).to_string()),
            local_node: local,
            remote_node: remote,
            weight: config.initial_weight,
            state: SynapseState::Forming,
            transport: config.preferred_transport.clone(),
            established_at_ns: now,
            last_active_ns: now,
            signals_transmitted: 0,
            signals_received: 0,
            avg_latency_ns: 0,
            error_rate: 0.0,
            max_weight: config.max_weight,
            decay_rate: config.decay_rate,
            active_threshold: config.active_threshold,
            dormancy_threshold: config.dormancy_threshold,
            attenuation_factor: config.attenuation_factor,
            signature_failures: 0,
            failure_window_start_ns: 0,
            type_affinity: std::collections::HashMap::new(),
        }
    }

    /// Create a new synapse using the host clock and an identity-seeded
    /// generator.
    ///
    /// Convenience for binaries and tests; core logic should prefer
    /// [`Self::new_with`].
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub fn new(local: NodeId, remote: NodeId, config: &SynapseConfig) -> Self {
        use crate::time::Clock as _;
        let now = crate::time::SystemClock.now_ns();
        let mut rng = crate::rng::SplitMix64::from_identity(&remote.0, now);
        Self::new_with(local, remote, config, now, &mut rng)
    }

    /// Strengthen the synapse after a successful signal transmission.
    pub fn strengthen(&mut self, signal_weight: f32, strengthen_factor: f32) {
        let delta = signal_weight * strengthen_factor;
        self.weight = (self.weight + delta).min(self.max_weight);
        self.update_state();
    }

    /// Weaken the synapse after a failed transmission.
    pub fn weaken_failure(&mut self) {
        self.weight *= 0.9;
        self.error_rate = (self.error_rate * 0.9) + 0.1;
        self.update_state();
    }

    /// Apply time-based weight decay.
    pub fn decay(&mut self) {
        self.weight *= 1.0 - self.decay_rate;
        self.update_state();
    }

    /// Record a successful signal transmission.
    pub fn record_transmission(&mut self, latency_ns: u64, signal_type: &str, now_ns: u64) {
        self.signals_transmitted += 1;
        self.last_active_ns = now_ns;

        // Running average latency
        if self.avg_latency_ns == 0 {
            self.avg_latency_ns = latency_ns;
        } else {
            self.avg_latency_ns = (self.avg_latency_ns * 9 + latency_ns) / 10;
        }

        // Update type affinity
        *self
            .type_affinity
            .entry(signal_type.to_string())
            .or_insert(0) += 1;

        self.error_rate *= 0.99; // Decay error rate on success
    }

    /// Record a received signal.
    pub fn record_reception(&mut self, now_ns: u64) {
        self.signals_received += 1;
        self.last_active_ns = now_ns;
    }

    /// Get the affinity score for a specific signal type.
    #[must_use]
    pub fn affinity_for(&self, signal_type: &str) -> f32 {
        let total: u64 = self.type_affinity.values().sum();
        if total == 0 {
            return 0.0;
        }
        let count = self.type_affinity.get(signal_type).copied().unwrap_or(0);
        count as f32 / total as f32
    }

    /// Mark the handshake complete, leaving `Forming`.
    ///
    /// The only way out of `Forming`, and deliberately so: a weight update is
    /// not evidence that two nodes have authenticated each other, so
    /// `update_state` leaves the state alone. The caller must have verified
    /// the peer's identity against the key that signed its handshake before
    /// calling this.
    pub fn activate(&mut self) {
        if self.state == SynapseState::Forming {
            self.state = SynapseState::Active;
        }
    }

    /// Activate a dormant synapse.
    pub fn reactivate(&mut self, config: &SynapseConfig) {
        if self.state == SynapseState::Dormant {
            self.weight = config.initial_weight;
            self.state = SynapseState::Active;
        }
    }

    /// Project to the persistent record.
    #[must_use]
    pub fn to_record(&self) -> crate::store::SynapseRecord {
        crate::store::SynapseRecord {
            id: self.id.clone(),
            peer: self.remote_node.clone(),
            weight: self.weight,
            attenuation_factor: self.attenuation_factor,
            state: self.state,
            type_affinity: self.type_affinity.clone(),
            established_at_ns: self.established_at_ns,
            last_active_ns: self.last_active_ns,
            signals_transmitted: self.signals_transmitted,
            signals_received: self.signals_received,
            avg_latency_ns: self.avg_latency_ns,
            error_rate: self.error_rate,
            signature_failures: self.signature_failures,
            failure_window_start_ns: self.failure_window_start_ns,
        }
    }

    /// Rehydrate from a persistent record.
    ///
    /// The record carries the learned state; transport and local
    /// configuration come from `config`, since neither is a property of the
    /// peer relationship.
    #[must_use]
    pub fn from_record(
        record: &crate::store::SynapseRecord,
        local: NodeId,
        config: &SynapseConfig,
    ) -> Self {
        Self {
            id: record.id.clone(),
            local_node: local,
            remote_node: record.peer.clone(),
            weight: record.weight,
            state: record.state,
            transport: config.preferred_transport.clone(),
            established_at_ns: record.established_at_ns,
            last_active_ns: record.last_active_ns,
            signals_transmitted: record.signals_transmitted,
            signals_received: record.signals_received,
            avg_latency_ns: record.avg_latency_ns,
            error_rate: record.error_rate,
            max_weight: config.max_weight,
            decay_rate: config.decay_rate,
            active_threshold: config.active_threshold,
            dormancy_threshold: config.dormancy_threshold,
            attenuation_factor: record.attenuation_factor,
            signature_failures: record.signature_failures,
            failure_window_start_ns: record.failure_window_start_ns,
            type_affinity: record.type_affinity.clone(),
        }
    }

    /// Apply a learning update from a resolved routing outcome.
    ///
    /// Returns the update that was applied, so the caller can record the
    /// magnitude against the peer's influence budget.
    pub fn apply_outcome(
        &mut self,
        outcome: crate::store::Outcome,
        signal_weight: f32,
        signal_type: &str,
        peer_influence_used: f32,
        learning: &crate::learning::LearningConfig,
    ) -> crate::learning::WeightUpdate {
        let update = crate::learning::apply_reward(
            self.weight,
            outcome,
            signal_weight,
            peer_influence_used,
            learning,
        );
        self.weight = update.after.min(self.max_weight);

        let affinity = self.affinity_for(signal_type);
        let updated = crate::learning::apply_affinity_update(affinity, outcome, learning);
        // Affinity is stored as counts; a positive outcome adds evidence.
        if updated > affinity {
            *self
                .type_affinity
                .entry(signal_type.to_string())
                .or_insert(0) += 1;
        }

        self.update_state();
        update
    }

    /// Apply the signature-failure penalty.
    pub fn apply_signature_failure(
        &mut self,
        now_ns: u64,
        learning: &crate::learning::LearningConfig,
    ) -> SignatureFailureOutcome {
        self.weight = crate::learning::apply_signature_penalty(self.weight, learning);

        // Count within one influence window, per threat-model §4. A window
        // that has elapsed starts a fresh count rather than accumulating
        // forever: the threshold is "five failures in an hour", not "five
        // failures ever", or a long-lived synapse would eventually be pruned
        // for unrelated incidents spread across months.
        let window_start = learning.influence_window_start(now_ns);
        if self.failure_window_start_ns < window_start {
            self.failure_window_start_ns = now_ns;
            self.signature_failures = 1;
        } else {
            self.signature_failures = self.signature_failures.saturating_add(1);
        }

        if self.signature_failures >= learning.signature_failure_prune_threshold {
            // Prune, and stamp `last_active_ns` so the re-formation cooldown
            // has a clock. Pruned is terminal for `update_state`, so the
            // weight can no longer move it.
            self.state = SynapseState::Pruned;
            self.last_active_ns = now_ns;
            return SignatureFailureOutcome {
                failures_in_window: self.signature_failures,
                pruned: true,
            };
        }

        self.update_state();
        SignatureFailureOutcome {
            failures_in_window: self.signature_failures,
            pruned: false,
        }
    }

    /// Weight after time-based decay, without mutating.
    #[must_use]
    pub fn decayed_weight(&self, now_ns: u64, learning: &crate::learning::LearningConfig) -> f32 {
        crate::learning::decayed_weight(self.weight, self.last_active_ns, now_ns, learning)
    }

    /// Apply time-based decay by half-life.
    ///
    /// Supersedes the old per-interval `decay()`, whose result depended on
    /// how often the sweep happened to run.
    pub fn apply_decay(&mut self, now_ns: u64, learning: &crate::learning::LearningConfig) {
        self.weight = self.decayed_weight(now_ns, learning);
        self.update_state();
    }

    /// Update state from the current weight and the configured thresholds.
    ///
    /// Two states are not weight-derived and so are left alone:
    ///
    /// - `Pruned` is terminal.
    /// - `Forming` means the handshake has not completed. A weight update is
    ///   not evidence that it has, and promoting on one would make
    ///   `Forming` skippable — the state exists precisely so that a synapse
    ///   is ineligible to carry signals until both sides have authenticated
    ///   ([synapse-lifecycle](https://openntl.org/spec/synapse-lifecycle),
    ///   "Eligibility to Carry Signals"). Only [`Self::activate`] leaves
    ///   `Forming`.
    fn update_state(&mut self) {
        if matches!(self.state, SynapseState::Pruned | SynapseState::Forming) {
            return;
        }

        if self.weight >= self.active_threshold {
            self.state = SynapseState::Active;
        } else if self.weight >= self.dormancy_threshold {
            self.state = SynapseState::Weakening;
        } else {
            self.state = SynapseState::Dormant;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> SynapseConfig {
        SynapseConfig::default()
    }

    fn test_nodes() -> (NodeId, NodeId) {
        (NodeId(vec![0u8; 32]), NodeId(vec![1u8; 32]))
    }

    #[test]
    fn a_weight_update_does_not_promote_a_forming_synapse() {
        // Forming means "handshake incomplete", and a weight change says
        // nothing about the handshake. Promoting here would make the state
        // skippable, and it is what makes a synapse ineligible to carry
        // signals before both sides have authenticated.
        let (local, remote) = test_nodes();
        let mut synapse = Synapse::new(local, remote, &test_config());
        assert_eq!(synapse.state, SynapseState::Forming);

        synapse.strengthen(1.0, 0.5);
        assert_eq!(
            synapse.state,
            SynapseState::Forming,
            "a weight update must not complete the handshake"
        );
        assert!(!synapse.state.can_carry());

        synapse.activate();
        assert_eq!(synapse.state, SynapseState::Active);
    }

    #[test]
    fn state_thresholds_follow_configuration() {
        // Previously hard-coded at 0.1/0.01, so retuning the config silently
        // did nothing.
        let (local, remote) = test_nodes();
        let config = SynapseConfig {
            active_threshold: 0.5,
            dormancy_threshold: 0.2,
            initial_weight: 0.6,
            ..test_config()
        };
        let mut synapse = Synapse::new(local, remote, &config);
        synapse.activate();
        assert_eq!(synapse.state, SynapseState::Active);

        // 0.6 * 0.9 = 0.54, still above the 0.5 active threshold.
        synapse.weaken_failure();
        assert_eq!(synapse.state, SynapseState::Active);

        // 0.54 * 0.9 = 0.486, below it. The old hard-coded 0.1 would have
        // kept this Active.
        synapse.weaken_failure();
        assert_eq!(synapse.state, SynapseState::Weakening);

        synapse.weight = 0.19;
        synapse.weaken_failure();
        assert_eq!(synapse.state, SynapseState::Dormant);
    }

    #[test]
    fn display_truncates_by_character_not_byte() {
        // Ids come from a TEXT column, so a store written by another
        // implementation may hold anything. Byte-slicing panicked mid-char,
        // inside a log line.
        let id = SynapseId("héllo wörld synapse".to_string());
        assert_eq!(format!("{id}"), "syn:héllo wö");
        assert_eq!(format!("{}", SynapseId("ab".to_string())), "syn:ab");
        assert_eq!(format!("{}", SynapseId(String::new())), "syn:");
    }

    #[test]
    fn new_synapse_starts_forming() {
        let (local, remote) = test_nodes();
        let synapse = Synapse::new(local, remote, &test_config());
        assert_eq!(synapse.state, SynapseState::Forming);
        assert!((synapse.weight - 0.1).abs() < f32::EPSILON);
    }

    #[test]
    fn strengthen_increases_weight() {
        let (local, remote) = test_nodes();
        let mut synapse = Synapse::new(local, remote, &test_config());
        synapse.state = SynapseState::Active;

        let initial = synapse.weight;
        synapse.strengthen(0.5, 0.01);
        assert!(synapse.weight > initial);
    }

    #[test]
    fn weight_respects_max() {
        let (local, remote) = test_nodes();
        let mut synapse = Synapse::new(local, remote, &test_config());
        synapse.state = SynapseState::Active;
        synapse.weight = 0.99;
        synapse.strengthen(1.0, 0.1);
        assert!(synapse.weight <= synapse.max_weight);
    }

    #[test]
    fn decay_reduces_weight() {
        let (local, remote) = test_nodes();
        let mut synapse = Synapse::new(local, remote, &test_config());
        synapse.state = SynapseState::Active;
        synapse.weight = 0.5;

        let initial = synapse.weight;
        synapse.decay();
        assert!(synapse.weight < initial);
    }

    #[test]
    fn state_transitions_on_weight() {
        let (local, remote) = test_nodes();
        let mut synapse = Synapse::new(local, remote, &test_config());
        synapse.state = SynapseState::Active;

        synapse.weight = 0.5;
        synapse.update_state();
        assert_eq!(synapse.state, SynapseState::Active);

        synapse.weight = 0.05;
        synapse.update_state();
        assert_eq!(synapse.state, SynapseState::Weakening);

        synapse.weight = 0.005;
        synapse.update_state();
        assert_eq!(synapse.state, SynapseState::Dormant);
    }

    #[test]
    fn weakening_synapses_can_still_carry_traffic() {
        // Regression: a synapse recovers weight only by carrying traffic that
        // succeeds. Making Weakening ineligible turns one bad outcome into a
        // permanent exclusion, and routing ossifies.
        assert!(SynapseState::Active.can_carry());
        assert!(
            SynapseState::Weakening.can_carry(),
            "Weakening means 'below threshold, still connected' — excluding it \
             would make weight recovery impossible"
        );
        assert!(!SynapseState::Dormant.can_carry());
        assert!(!SynapseState::Pruned.can_carry());
        assert!(!SynapseState::Forming.can_carry());
    }

    #[test]
    fn a_single_failure_does_not_permanently_exclude_a_new_synapse() {
        // initial_weight and the Active floor are both 0.1, so one negative
        // outcome drops a fresh synapse to Weakening. It must remain eligible.
        let (local, remote) = test_nodes();
        let config = test_config();
        let mut rng = crate::rng::SplitMix64::seeded(1);
        let mut synapse = Synapse::new_with(local, remote, &config, 1_000, &mut rng);
        synapse.state = SynapseState::Active;

        let learning = crate::learning::LearningConfig::default();
        synapse.apply_outcome(crate::store::Outcome::Rejected, 0.8, "data", 0.0, &learning);

        assert!(
            synapse.weight < config.initial_weight,
            "a rejection should reduce the weight"
        );
        assert!(
            synapse.state.can_carry(),
            "but the synapse must still be able to earn its weight back, \
             state was {:?} at weight {}",
            synapse.state,
            synapse.weight
        );
    }

    #[test]
    fn record_roundtrip_preserves_learned_state() {
        let (local, remote) = test_nodes();
        let mut rng = crate::rng::SplitMix64::seeded(2);
        let mut synapse = Synapse::new_with(local.clone(), remote, &test_config(), 1_000, &mut rng);
        synapse.weight = 0.42;
        synapse.type_affinity.insert("Query".into(), 9);
        synapse.state = SynapseState::Weakening;

        let record = synapse.to_record();
        let restored = Synapse::from_record(&record, local, &test_config());

        assert!((restored.weight - 0.42).abs() < f32::EPSILON);
        assert_eq!(restored.type_affinity.get("Query"), Some(&9));
        assert_eq!(restored.state, SynapseState::Weakening);
        assert_eq!(restored.id, synapse.id);
    }

    #[test]
    fn half_life_decay_replaces_per_interval_decay() {
        let (local, remote) = test_nodes();
        let mut rng = crate::rng::SplitMix64::seeded(3);
        let mut synapse = Synapse::new_with(local, remote, &test_config(), 0, &mut rng);
        synapse.weight = 0.8;
        synapse.last_active_ns = 0;
        synapse.state = SynapseState::Active;

        let learning = crate::learning::LearningConfig::default();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let half_life_ns = (learning.decay_half_life_hours as u64) * crate::time::NANOS_PER_HOUR;
        synapse.apply_decay(half_life_ns, &learning);

        assert!(
            (synapse.weight - 0.4).abs() < 0.01,
            "one half-life should halve the weight, got {}",
            synapse.weight
        );
    }

    #[test]
    fn type_affinity_tracking() {
        let (local, remote) = test_nodes();
        let mut synapse = Synapse::new(local, remote, &test_config());

        synapse.record_transmission(1000, "query", 5_000);
        synapse.record_transmission(1000, "query", 5_000);
        synapse.record_transmission(1000, "data", 5_000);

        assert!((synapse.affinity_for("query") - 0.666).abs() < 0.01);
        assert!((synapse.affinity_for("data") - 0.333).abs() < 0.01);
        assert!((synapse.affinity_for("event") - 0.0).abs() < f32::EPSILON);
    }
}

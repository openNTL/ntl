//! The online routing model.
//!
//! Implements [spec/learning-model][spec]: reward-driven Hebbian weight
//! updates, half-life decay, outbound normalisation, per-identity influence
//! caps, and stochastic exploration.
//!
//! [spec]: https://openntl.org/spec/learning-model
//!
//! The model is a contextual bandit over synapses. It is deliberately small
//! — a node with 100 synapses and 8 signal types holds under a thousand
//! parameters — so it trains on a phone and an operator can read it as a
//! table.
//!
//! Every function here is pure or takes its time and randomness as
//! arguments, so the whole model is deterministically testable.

use serde::{Deserialize, Serialize};

use crate::rng::Rng;
use crate::store::Outcome;
use crate::time::{NANOS_PER_HOUR, ns_to_hours};

/// Deployment class, selecting hyperparameter defaults.
///
/// A high-traffic node sees more evidence per unit time, so it can afford a
/// smaller learning rate, a shorter memory, and less exploration. An edge
/// node sees little traffic and must learn faster from less.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[derive(Default)]
pub enum DeploymentClass {
    /// Constrained device: phone, sensor, intermittent link.
    #[default]
    Edge,
    /// General-purpose participating node.
    FullNode,
    /// High-volume infrastructure node.
    HighTraffic,
}

/// Serde default for [`LearningConfig::signature_failure_cooldown_secs`].
///
/// One hour, matching threat-model §4's stated default.
fn default_signature_failure_cooldown_secs() -> u64 {
    3_600
}

/// Hyperparameters for the routing model.
///
/// Defaults come from [spec/learning-model][spec] §5.
///
/// [spec]: https://openntl.org/spec/learning-model
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LearningConfig {
    /// Whether weights are updated at all. A node with this off MUST report
    /// that it does not learn.
    pub enabled: bool,
    /// Learning rate `η`.
    pub learning_rate: f32,
    /// Decay half-life in hours, `t½`.
    pub decay_half_life_hours: f32,
    /// Exploration temperature `τ`.
    pub exploration_temperature: f32,
    /// Floor for `τ`. A node that stops exploring stops learning, so this
    /// MUST be above zero.
    pub min_temperature: f32,
    /// Seconds before an unresolved decision becomes [`Outcome::TimedOut`].
    pub receipt_window_secs: u64,
    /// Lower weight bound `w_min`.
    pub min_weight: f32,
    /// Upper weight bound `w_max`.
    pub max_weight: f32,
    /// Total outbound weight budget `W_max`.
    pub max_total_outbound_weight: f32,
    /// Per-identity influence cap `C` within one window.
    pub influence_cap_per_peer: f32,
    /// Influence window `T_infl`, in seconds.
    pub influence_window_secs: u64,
    /// Weight multiplier penalty on signature failure, `p`.
    pub signature_failure_penalty: f32,
    /// Signature failures within one influence window before pruning.
    pub signature_failure_prune_threshold: u32,
    /// Seconds a synapse pruned for signature failures may not be re-formed.
    ///
    /// [threat-model](https://openntl.org/spec/threat-model) §4: a node SHOULD
    /// refuse to re-form such a synapse for a cooldown period. Without one the
    /// prune costs an attacker a single handshake — it reconnects and the
    /// fresh synapse starts at `initial_weight` with no memory of why the last
    /// one died.
    #[serde(default = "default_signature_failure_cooldown_secs")]
    pub signature_failure_cooldown_secs: u64,
}

impl LearningConfig {
    /// Defaults for a deployment class.
    #[must_use]
    pub fn for_class(class: DeploymentClass) -> Self {
        let (rate, half_life, temp, min_temp, window, budget, cap) = match class {
            DeploymentClass::Edge => (0.05, 168.0, 0.15, 0.02, 30, 8.0, 0.20),
            DeploymentClass::FullNode => (0.02, 72.0, 0.10, 0.02, 15, 32.0, 0.10),
            DeploymentClass::HighTraffic => (0.01, 24.0, 0.05, 0.01, 5, 128.0, 0.05),
        };
        Self {
            enabled: true,
            learning_rate: rate,
            decay_half_life_hours: half_life,
            exploration_temperature: temp,
            min_temperature: min_temp,
            receipt_window_secs: window,
            min_weight: 0.001,
            max_weight: 1.0,
            max_total_outbound_weight: budget,
            influence_cap_per_peer: cap,
            influence_window_secs: 3_600,
            signature_failure_penalty: 0.5,
            signature_failure_prune_threshold: 5,
            signature_failure_cooldown_secs: default_signature_failure_cooldown_secs(),
        }
    }

    /// Effective temperature, never below the floor.
    #[must_use]
    pub fn effective_temperature(&self) -> f32 {
        self.exploration_temperature.max(self.min_temperature)
    }

    /// Start of the current influence window relative to `now_ns`.
    #[must_use]
    pub fn influence_window_start(&self, now_ns: u64) -> u64 {
        now_ns.saturating_sub(self.influence_window_secs.saturating_mul(1_000_000_000))
    }

    /// Deadline before which a decision must resolve or time out.
    #[must_use]
    pub fn receipt_deadline(&self, now_ns: u64) -> u64 {
        now_ns.saturating_sub(self.receipt_window_secs.saturating_mul(1_000_000_000))
    }

    /// Validate the configuration.
    ///
    /// # Errors
    /// Returns a description of the first invalid field.
    pub fn validate(&self) -> Result<(), String> {
        if !(self.learning_rate > 0.0 && self.learning_rate <= 1.0) {
            return Err(format!(
                "learning_rate must be in (0, 1], got {}",
                self.learning_rate
            ));
        }
        if self.decay_half_life_hours <= 0.0 {
            return Err(format!(
                "decay_half_life_hours must be positive, got {}",
                self.decay_half_life_hours
            ));
        }
        if self.min_temperature <= 0.0 {
            return Err(
                "min_temperature must be positive: a node that stops exploring stops learning"
                    .to_string(),
            );
        }
        if self.min_weight <= 0.0 || self.min_weight >= self.max_weight {
            return Err(format!(
                "require 0 < min_weight < max_weight, got {} and {}",
                self.min_weight, self.max_weight
            ));
        }
        if self.max_total_outbound_weight <= self.max_weight {
            return Err(format!(
                "max_total_outbound_weight ({}) must exceed max_weight ({}), \
                 or a single synapse could never reach its bound",
                self.max_total_outbound_weight, self.max_weight
            ));
        }
        if self.influence_cap_per_peer <= 0.0 {
            return Err("influence_cap_per_peer must be positive".to_string());
        }
        if !(self.signature_failure_penalty > 0.0 && self.signature_failure_penalty <= 1.0) {
            return Err(format!(
                "signature_failure_penalty must be in (0, 1], got {}",
                self.signature_failure_penalty
            ));
        }
        Ok(())
    }
}

impl Default for LearningConfig {
    fn default() -> Self {
        Self::for_class(DeploymentClass::Edge)
    }
}

// ---------------------------------------------------------------------------
// Weight updates
// ---------------------------------------------------------------------------

/// The outcome of applying one learning update.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WeightUpdate {
    /// Weight before the update.
    pub before: f32,
    /// Weight after the update.
    pub after: f32,
    /// The delta actually applied, after clamping and capping.
    pub applied_delta: f32,
    /// Whether the influence cap reduced or suppressed the update.
    pub capped: bool,
}

impl WeightUpdate {
    /// Whether the update changed anything.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        (self.after - self.before).abs() < f32::EPSILON
    }
}

/// Apply the Hebbian update for one resolved decision.
///
/// `Δw = η · r · x`, clamped to `[min_weight, max_weight]`.
///
/// `signal_weight` is the weight of the signal that was carried: a
/// low-weight signal teaches less, so influence cannot be bought cheaply by
/// flooding negligible traffic.
///
/// `peer_influence_used` is the peer's accumulated influence in the current
/// window. When the cap is exhausted, **positive** updates are suppressed
/// while negative ones still apply — an attacker must not be able to spend
/// their own budget to become immune to penalties.
#[must_use]
pub fn apply_reward(
    weight: f32,
    outcome: Outcome,
    signal_weight: f32,
    peer_influence_used: f32,
    config: &LearningConfig,
) -> WeightUpdate {
    let reward = outcome.reward();
    let raw_delta = config.learning_rate * reward * signal_weight.clamp(0.0, 1.0);

    let mut capped = false;
    let delta = if raw_delta > 0.0 && peer_influence_used >= config.influence_cap_per_peer {
        // Budget exhausted: no further help for this peer this window.
        capped = true;
        0.0
    } else if raw_delta > 0.0 {
        // Partial headroom: allow only what remains of the budget.
        let headroom = config.influence_cap_per_peer - peer_influence_used;
        if raw_delta > headroom {
            capped = true;
            headroom
        } else {
            raw_delta
        }
    } else {
        // Negative updates are never capped.
        raw_delta
    };

    let after = (weight + delta).clamp(config.min_weight, config.max_weight);
    WeightUpdate {
        before: weight,
        after,
        applied_delta: after - weight,
        capped,
    }
}

/// Apply the signature-failure penalty: `w ← max(w·(1−p), w_min)`.
///
/// Recovery is by ordinary learning only, which makes it deliberately slow —
/// a verification failure is either an attack or a serious defect.
#[must_use]
pub fn apply_signature_penalty(weight: f32, config: &LearningConfig) -> f32 {
    (weight * (1.0 - config.signature_failure_penalty)).max(config.min_weight)
}

/// Update a per-type affinity score from an outcome.
#[must_use]
pub fn apply_affinity_update(affinity: f32, outcome: Outcome, config: &LearningConfig) -> f32 {
    (affinity + config.learning_rate * outcome.reward()).clamp(0.0, 1.0)
}

/// Decay a weight from its last activity to now, by half-life.
///
/// `w(t) = w₀ · 2^(−Δt / t½)`
///
/// Callers may apply this lazily on read: the result is identical to
/// continuous application, because the formula depends only on elapsed time.
#[must_use]
pub fn decayed_weight(
    weight: f32,
    last_active_ns: u64,
    now_ns: u64,
    config: &LearningConfig,
) -> f32 {
    if now_ns <= last_active_ns {
        return weight;
    }
    let elapsed_hours = ns_to_hours(now_ns - last_active_ns);
    let exponent = -elapsed_hours / config.decay_half_life_hours;
    (weight * exponent.exp2()).max(config.min_weight)
}

/// Elapsed time after which a weight decays to `target`, in nanoseconds.
///
/// Useful for scheduling a pruning sweep rather than polling for it.
#[must_use]
pub fn time_to_decay_to(weight: f32, target: f32, config: &LearningConfig) -> Option<u64> {
    if target <= 0.0 || weight <= target {
        return None;
    }
    let halvings = (weight / target).log2();
    let hours = halvings * config.decay_half_life_hours;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ns = (f64::from(hours) * NANOS_PER_HOUR as f64) as u64;
    Some(ns)
}

/// Rescale weights so their total does not exceed `max_total_outbound_weight`.
///
/// Returns the factor applied, or `None` if the budget was not exceeded.
///
/// This divisive normalisation is what makes the model *competitive* rather
/// than merely additive. Without it every weight inflates to `max_weight`,
/// the scoring function stops discriminating, and routing degenerates to
/// flooding. It also bounds weight poisoning: an attacker who strengthens
/// their own synapse dilutes it again through the rescaling, because the
/// budget is fixed.
pub fn normalize_outbound(weights: &mut [f32], config: &LearningConfig) -> Option<f32> {
    let total: f32 = weights.iter().sum();
    if total <= config.max_total_outbound_weight || total <= 0.0 {
        return None;
    }
    let factor = config.max_total_outbound_weight / total;
    for w in weights.iter_mut() {
        *w = (*w * factor).max(config.min_weight);
    }
    Some(factor)
}

// ---------------------------------------------------------------------------
// Exploration
// ---------------------------------------------------------------------------

/// Which policy selects among scored candidates.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[derive(Default)]
pub enum ExplorationPolicy {
    /// Softmax over scores with temperature. RECOMMENDED.
    #[default]
    Softmax,
    /// Top-k, with one slot occasionally replaced by a uniform draw.
    /// Cheaper; for nodes without floating-point `exp`.
    EpsilonGreedy {
        /// Probability of substituting an exploratory pick.
        epsilon: f32,
    },
}

/// One selected candidate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Selection {
    /// Index into the scores slice that was passed in.
    pub index: usize,
    /// The score it was selected on.
    pub score: f32,
    /// Whether this pick came from exploration rather than exploitation.
    ///
    /// Journalled per decision: exploration picks carry information about
    /// paths the model currently underrates, and an operator needs to see
    /// whether the node is still exploring at all.
    pub explored: bool,
}

/// Sample up to `k` candidates from `scores` without replacement.
///
/// Selection MUST be stochastic. Deterministic top-N ossifies routing: a
/// synapse that is never selected is never rewarded, so its weight only
/// decays, so it is never selected. Startup conditions become permanent and
/// the network cannot discover a path that became good later.
///
/// Returns selections ordered best-score-first among those chosen.
pub fn sample_paths(
    scores: &[f32],
    k: usize,
    policy: ExplorationPolicy,
    config: &LearningConfig,
    rng: &mut dyn Rng,
) -> Vec<Selection> {
    if scores.is_empty() || k == 0 {
        return Vec::new();
    }
    let mut out = match policy {
        ExplorationPolicy::Softmax => {
            softmax_sample(scores, k, config.effective_temperature(), rng)
        }
        ExplorationPolicy::EpsilonGreedy { epsilon } => {
            epsilon_greedy_sample(scores, k, epsilon, rng)
        }
    };
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

/// Softmax sampling without replacement.
fn softmax_sample(scores: &[f32], k: usize, temperature: f32, rng: &mut dyn Rng) -> Vec<Selection> {
    let k = k.min(scores.len());
    let mut remaining: Vec<usize> = (0..scores.len()).collect();
    let mut chosen = Vec::with_capacity(k);

    // The highest-scoring candidate is the exploitation baseline; anything
    // else selected first is, by definition, exploration.
    let best = argmax(scores);

    for _ in 0..k {
        if remaining.is_empty() {
            break;
        }
        // Subtract the max before exponentiating, or a large score/temperature
        // ratio overflows to infinity and the distribution collapses to NaN.
        let max_score = remaining
            .iter()
            .map(|&i| scores[i])
            .fold(f32::NEG_INFINITY, f32::max);

        let weights: Vec<f32> = remaining
            .iter()
            .map(|&i| ((scores[i] - max_score) / temperature).exp())
            .collect();
        let total: f32 = weights.iter().sum();

        let pick = if total > 0.0 && total.is_finite() {
            let target = rng.next_f32() * total;
            let mut acc = 0.0;
            let mut pick = weights.len() - 1;
            for (slot, w) in weights.iter().enumerate() {
                acc += w;
                if acc >= target {
                    pick = slot;
                    break;
                }
            }
            pick
        } else {
            // Degenerate distribution — fall back to a uniform draw rather
            // than biasing toward index 0.
            #[allow(clippy::cast_possible_truncation)]
            let n = remaining.len() as u64;
            rng.next_below(n).unwrap_or(0) as usize
        };

        let index = remaining.swap_remove(pick);
        chosen.push(Selection {
            index,
            score: scores[index],
            explored: Some(index) != best,
        });
    }
    chosen
}

/// Top-k with an occasional uniform substitution.
fn epsilon_greedy_sample(
    scores: &[f32],
    k: usize,
    epsilon: f32,
    rng: &mut dyn Rng,
) -> Vec<Selection> {
    let k = k.min(scores.len());
    let mut order: Vec<usize> = (0..scores.len()).collect();
    order.sort_by(|&a, &b| {
        scores[b]
            .partial_cmp(&scores[a])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.cmp(&b))
    });

    let mut chosen: Vec<usize> = order[..k].to_vec();
    let mut explored_index = None;

    if rng.next_f32() < epsilon && order.len() > k {
        let pool = &order[k..];
        #[allow(clippy::cast_possible_truncation)]
        let n = pool.len() as u64;
        if let Some(pick) = rng.next_below(n) {
            let swapped_in = pool[pick as usize];
            // Replace the weakest of the exploitation picks.
            chosen[k - 1] = swapped_in;
            explored_index = Some(swapped_in);
        }
    }

    chosen
        .into_iter()
        .map(|index| Selection {
            index,
            score: scores[index],
            explored: Some(index) == explored_index,
        })
        .collect()
}

/// Index of the maximum score, or `None` when empty.
fn argmax(scores: &[f32]) -> Option<usize> {
    scores
        .iter()
        .enumerate()
        .fold(None, |acc: Option<(usize, f32)>, (i, &s)| match acc {
            Some((_, best)) if best >= s => acc,
            _ => Some((i, s)),
        })
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::{FixedRng, SplitMix64};
    use crate::time::{NANOS_PER_HOUR, NANOS_PER_SEC};

    fn cfg() -> LearningConfig {
        LearningConfig::for_class(DeploymentClass::Edge)
    }

    // -- config ------------------------------------------------------------

    #[test]
    fn class_defaults_are_ordered_sensibly() {
        let edge = LearningConfig::for_class(DeploymentClass::Edge);
        let full = LearningConfig::for_class(DeploymentClass::FullNode);
        let high = LearningConfig::for_class(DeploymentClass::HighTraffic);

        // More traffic means more evidence per unit time: learn slower,
        // forget faster, explore less.
        assert!(edge.learning_rate > full.learning_rate);
        assert!(full.learning_rate > high.learning_rate);
        assert!(edge.decay_half_life_hours > high.decay_half_life_hours);
        assert!(edge.exploration_temperature > high.exploration_temperature);
        assert!(edge.max_total_outbound_weight < high.max_total_outbound_weight);
        assert!(edge.influence_cap_per_peer > high.influence_cap_per_peer);
    }

    #[test]
    fn all_class_defaults_validate() {
        for class in [
            DeploymentClass::Edge,
            DeploymentClass::FullNode,
            DeploymentClass::HighTraffic,
        ] {
            LearningConfig::for_class(class)
                .validate()
                .unwrap_or_else(|e| panic!("{class:?} defaults invalid: {e}"));
        }
    }

    #[test]
    fn validate_rejects_zero_temperature_floor() {
        let c = LearningConfig {
            min_temperature: 0.0,
            ..cfg()
        };
        assert!(c.validate().unwrap_err().contains("min_temperature"));
    }

    #[test]
    fn validate_rejects_budget_below_single_max() {
        let c = LearningConfig {
            max_total_outbound_weight: 0.5,
            max_weight: 1.0,
            ..cfg()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn temperature_never_falls_below_floor() {
        let c = LearningConfig {
            exploration_temperature: 0.0,
            min_temperature: 0.02,
            ..cfg()
        };
        assert!((c.effective_temperature() - 0.02).abs() < f32::EPSILON);
    }

    // -- reward updates ----------------------------------------------------

    #[test]
    fn delivered_strengthens_and_timeout_weakens() {
        let c = cfg();
        let up = apply_reward(0.5, Outcome::Delivered, 0.8, 0.0, &c);
        assert!(up.after > 0.5);
        assert!((up.applied_delta - 0.05 * 0.8).abs() < 1e-6);

        let down = apply_reward(0.5, Outcome::TimedOut, 0.8, 0.0, &c);
        assert!(down.after < 0.5);
    }

    #[test]
    fn pending_never_moves_weight() {
        let up = apply_reward(0.5, Outcome::Pending, 1.0, 0.0, &cfg());
        assert!(
            up.is_noop(),
            "an unresolved decision must not move weights in either direction"
        );
    }

    #[test]
    fn rejection_penalised_harder_than_timeout() {
        let c = cfg();
        let t = apply_reward(0.9, Outcome::TimedOut, 1.0, 0.0, &c);
        let r = apply_reward(0.9, Outcome::Rejected, 1.0, 0.0, &c);
        assert!(
            r.after < t.after,
            "definite rejection must cost more than ambiguous silence, \
             or the model becomes brittle on lossy links"
        );
    }

    #[test]
    fn signature_failure_is_the_strongest_penalty() {
        let c = cfg();
        let outcomes = [
            Outcome::TimedOut,
            Outcome::Rejected,
            Outcome::TransportFailure,
        ];
        let sig = apply_reward(0.9, Outcome::SignatureFailure, 1.0, 0.0, &c).after;
        for o in outcomes {
            assert!(sig <= apply_reward(0.9, o, 1.0, 0.0, &c).after);
        }
    }

    #[test]
    fn low_weight_signals_teach_less() {
        let c = cfg();
        let big = apply_reward(0.5, Outcome::Delivered, 1.0, 0.0, &c);
        let small = apply_reward(0.5, Outcome::Delivered, 0.01, 0.0, &c);
        assert!(
            big.applied_delta > small.applied_delta * 10.0,
            "flooding negligible signals must not buy influence cheaply"
        );
    }

    #[test]
    fn weight_respects_bounds() {
        let c = cfg();
        let high = apply_reward(c.max_weight, Outcome::Delivered, 1.0, 0.0, &c);
        assert!(high.after <= c.max_weight);
        let low = apply_reward(c.min_weight, Outcome::SignatureFailure, 1.0, 0.0, &c);
        assert!(low.after >= c.min_weight);
    }

    // -- influence caps ----------------------------------------------------

    #[test]
    fn exhausted_budget_suppresses_positive_updates() {
        let c = cfg();
        let up = apply_reward(0.5, Outcome::Delivered, 1.0, c.influence_cap_per_peer, &c);
        assert!(up.capped);
        assert!(
            up.is_noop(),
            "a peer past its cap must not gain more weight"
        );
    }

    #[test]
    fn partial_budget_allows_only_the_headroom() {
        let c = cfg();
        let used = c.influence_cap_per_peer - 0.01;
        let up = apply_reward(0.5, Outcome::Delivered, 1.0, used, &c);
        assert!(up.capped);
        assert!((up.applied_delta - 0.01).abs() < 1e-6);
    }

    #[test]
    fn cap_is_asymmetric_negatives_always_apply() {
        let c = cfg();
        let up = apply_reward(
            0.5,
            Outcome::Rejected,
            1.0,
            c.influence_cap_per_peer * 10.0,
            &c,
        );
        assert!(
            up.after < 0.5,
            "an attacker must not be able to spend their own budget to \
             become immune to penalties"
        );
        assert!(!up.capped);
    }

    // -- decay -------------------------------------------------------------

    #[test]
    fn one_half_life_halves_the_weight() {
        let c = cfg();
        let t0 = 1_000 * NANOS_PER_HOUR;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let half_life_ns = (c.decay_half_life_hours as u64) * NANOS_PER_HOUR;
        let w = decayed_weight(0.8, t0, t0 + half_life_ns, &c);
        assert!((w - 0.4).abs() < 0.01, "expected ~0.4, got {w}");
    }

    #[test]
    fn two_half_lives_quarter_the_weight() {
        let c = cfg();
        let t0 = 0;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let two = 2 * (c.decay_half_life_hours as u64) * NANOS_PER_HOUR;
        let w = decayed_weight(0.8, t0, two, &c);
        assert!((w - 0.2).abs() < 0.01, "expected ~0.2, got {w}");
    }

    #[test]
    fn decay_is_path_independent() {
        // Lazy application must equal continuous application. Applying decay
        // in two steps must match applying it in one.
        let c = cfg();
        let step = 24 * NANOS_PER_HOUR;
        let one_shot = decayed_weight(0.8, 0, 2 * step, &c);
        let two_step = decayed_weight(decayed_weight(0.8, 0, step, &c), step, 2 * step, &c);
        assert!(
            (one_shot - two_step).abs() < 1e-5,
            "decay must not depend on how often the sweep runs: {one_shot} vs {two_step}"
        );
    }

    #[test]
    fn decay_never_goes_below_min_weight() {
        let c = cfg();
        let w = decayed_weight(0.8, 0, 10_000 * NANOS_PER_HOUR, &c);
        assert!(w >= c.min_weight);
    }

    #[test]
    fn decay_ignores_clock_going_backwards() {
        let c = cfg();
        assert!((decayed_weight(0.8, 5_000, 1_000, &c) - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn time_to_decay_matches_decay() {
        let c = cfg();
        let ns = time_to_decay_to(0.8, 0.1, &c).unwrap();
        let w = decayed_weight(0.8, 0, ns, &c);
        assert!((w - 0.1).abs() < 0.01, "expected ~0.1, got {w}");
        assert_eq!(time_to_decay_to(0.1, 0.8, &c), None);
    }

    // -- normalisation -----------------------------------------------------

    #[test]
    fn normalisation_is_a_noop_under_budget() {
        let c = cfg();
        let mut w = vec![0.1, 0.2, 0.3];
        assert_eq!(normalize_outbound(&mut w, &c), None);
        assert_eq!(w, vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn normalisation_caps_the_total() {
        let c = cfg();
        let mut w = vec![1.0; 20]; // total 20 > budget 8
        let factor = normalize_outbound(&mut w, &c).expect("should rescale");
        assert!((factor - 8.0 / 20.0).abs() < 1e-6);
        let total: f32 = w.iter().sum();
        assert!(
            (total - c.max_total_outbound_weight).abs() < 0.01,
            "total should be at budget, got {total}"
        );
    }

    #[test]
    fn normalisation_preserves_relative_order() {
        let c = cfg();
        let mut w = vec![4.0, 2.0, 1.0, 8.0];
        normalize_outbound(&mut w, &c).expect("should rescale");
        assert!(w[3] > w[0] && w[0] > w[1] && w[1] > w[2]);
    }

    #[test]
    fn saturation_is_prevented_over_many_updates() {
        // The failure this exists to prevent: repeated positive updates
        // inflating every weight to max, after which scoring cannot
        // discriminate.
        let c = LearningConfig {
            influence_cap_per_peer: f32::MAX, // isolate normalisation
            ..cfg()
        };
        let mut weights = vec![0.4_f32; 20];
        for _ in 0..500 {
            for w in &mut weights {
                *w = apply_reward(*w, Outcome::Delivered, 1.0, 0.0, &c).after;
            }
            normalize_outbound(&mut weights, &c);
        }
        let total: f32 = weights.iter().sum();
        assert!(
            total <= c.max_total_outbound_weight + 0.01,
            "total weight must stay bounded, got {total}"
        );
        assert!(
            weights.iter().all(|&w| w < c.max_weight),
            "no synapse should saturate at max_weight"
        );
    }

    // -- exploration -------------------------------------------------------

    #[test]
    fn sampling_returns_at_most_k_distinct_indices() {
        let mut rng = SplitMix64::seeded(1);
        let scores = vec![0.9, 0.7, 0.5, 0.3, 0.1];
        let picks = sample_paths(&scores, 3, ExplorationPolicy::Softmax, &cfg(), &mut rng);
        assert_eq!(picks.len(), 3);
        let mut idx: Vec<usize> = picks.iter().map(|s| s.index).collect();
        idx.sort_unstable();
        idx.dedup();
        assert_eq!(idx.len(), 3, "sampling must be without replacement");
    }

    #[test]
    fn sampling_handles_degenerate_inputs() {
        let mut rng = SplitMix64::seeded(2);
        let c = cfg();
        assert!(sample_paths(&[], 3, ExplorationPolicy::Softmax, &c, &mut rng).is_empty());
        assert!(sample_paths(&[0.5], 0, ExplorationPolicy::Softmax, &c, &mut rng).is_empty());
        // k larger than the candidate set must not panic.
        let picks = sample_paths(&[0.5, 0.4], 10, ExplorationPolicy::Softmax, &c, &mut rng);
        assert_eq!(picks.len(), 2);
    }

    #[test]
    fn softmax_favours_high_scores_but_still_probes_low_ones() {
        let mut rng = SplitMix64::seeded(4);
        let c = cfg();
        let scores = vec![0.62, 0.55, 0.11];
        let mut counts = [0u32; 3];
        for _ in 0..10_000 {
            for pick in sample_paths(&scores, 1, ExplorationPolicy::Softmax, &c, &mut rng) {
                counts[pick.index] += 1;
            }
        }
        assert!(counts[0] > counts[1], "the best path must dominate");
        assert!(counts[1] > counts[2]);
        assert!(
            counts[2] > 0,
            "the weakest path must still be probed sometimes — this is what \
             prevents ossified routing"
        );
    }

    #[test]
    fn low_temperature_approaches_greedy() {
        let c = LearningConfig {
            exploration_temperature: 0.001,
            min_temperature: 0.0005,
            ..cfg()
        };
        let mut rng = SplitMix64::seeded(5);
        let scores = vec![0.9, 0.5, 0.1];
        let mut best = 0;
        for _ in 0..500 {
            if sample_paths(&scores, 1, ExplorationPolicy::Softmax, &c, &mut rng)[0].index == 0 {
                best += 1;
            }
        }
        assert!(
            best > 480,
            "low temperature should be near-greedy, got {best}/500"
        );
    }

    #[test]
    fn high_temperature_approaches_uniform() {
        let c = LearningConfig {
            exploration_temperature: 100.0,
            ..cfg()
        };
        let mut rng = SplitMix64::seeded(6);
        let scores = vec![0.9, 0.5, 0.1];
        let mut counts = [0u32; 3];
        for _ in 0..9_000 {
            counts[sample_paths(&scores, 1, ExplorationPolicy::Softmax, &c, &mut rng)[0].index] +=
                1;
        }
        for (i, &c) in counts.iter().enumerate() {
            assert!(
                (2_400..3_600).contains(&c),
                "index {i} drawn {c} times; high temperature should be ~uniform"
            );
        }
    }

    #[test]
    fn extreme_score_ratio_does_not_produce_nan() {
        // Without subtracting the max before exp(), this overflows to inf
        // and the distribution collapses to NaN.
        let c = LearningConfig {
            exploration_temperature: 0.001,
            min_temperature: 0.0001,
            ..cfg()
        };
        let mut rng = SplitMix64::seeded(8);
        let scores = vec![1000.0, 0.0, -1000.0];
        let picks = sample_paths(&scores, 2, ExplorationPolicy::Softmax, &c, &mut rng);
        assert_eq!(picks.len(), 2);
        assert!(picks.iter().all(|p| p.score.is_finite()));
    }

    #[test]
    fn best_score_pick_is_not_marked_explored() {
        let mut rng = FixedRng::from_f32s(&[0.0]);
        let picks = sample_paths(&[0.9, 0.1], 1, ExplorationPolicy::Softmax, &cfg(), &mut rng);
        assert_eq!(picks[0].index, 0);
        assert!(
            !picks[0].explored,
            "selecting the top-scoring path is exploitation, not exploration"
        );
    }

    #[test]
    fn non_best_pick_is_marked_explored() {
        let mut rng = SplitMix64::seeded(12);
        let scores = vec![0.9, 0.5, 0.1];
        let mut saw_explored = false;
        for _ in 0..2_000 {
            for p in sample_paths(&scores, 1, ExplorationPolicy::Softmax, &cfg(), &mut rng) {
                if p.index != 0 {
                    assert!(p.explored, "a non-best pick must be flagged as exploration");
                    saw_explored = true;
                }
            }
        }
        assert!(saw_explored, "expected at least one exploratory pick");
    }

    #[test]
    fn results_are_ordered_by_score_descending() {
        let mut rng = SplitMix64::seeded(13);
        let scores = vec![0.1, 0.9, 0.5, 0.7];
        for _ in 0..200 {
            let picks = sample_paths(&scores, 3, ExplorationPolicy::Softmax, &cfg(), &mut rng);
            for w in picks.windows(2) {
                assert!(w[0].score >= w[1].score);
            }
        }
    }

    #[test]
    fn epsilon_greedy_takes_top_k_when_not_exploring() {
        // epsilon = 0 means pure exploitation.
        let mut rng = SplitMix64::seeded(14);
        let scores = vec![0.1, 0.9, 0.5, 0.7];
        let picks = sample_paths(
            &scores,
            2,
            ExplorationPolicy::EpsilonGreedy { epsilon: 0.0 },
            &cfg(),
            &mut rng,
        );
        let idx: Vec<usize> = picks.iter().map(|s| s.index).collect();
        assert_eq!(idx, vec![1, 3], "should take the two best");
        assert!(picks.iter().all(|p| !p.explored));
    }

    #[test]
    fn epsilon_greedy_explores_when_it_should() {
        // epsilon = 1 means always substitute one exploratory pick.
        let mut rng = SplitMix64::seeded(15);
        let scores = vec![0.1, 0.9, 0.5, 0.7];
        let picks = sample_paths(
            &scores,
            2,
            ExplorationPolicy::EpsilonGreedy { epsilon: 1.0 },
            &cfg(),
            &mut rng,
        );
        assert_eq!(picks.len(), 2);
        assert_eq!(
            picks.iter().filter(|p| p.explored).count(),
            1,
            "exactly one slot should be exploratory"
        );
    }

    #[test]
    fn ossification_is_prevented_over_a_long_run() {
        // The end-to-end property: a path that starts weak must be able to
        // overtake one that starts strong, if it actually delivers.
        let c = cfg();
        let mut rng = SplitMix64::seeded(21);
        // index 0 starts strong but always fails; index 1 starts weak but
        // always delivers.
        let mut weights = [0.9_f32, 0.05_f32];

        for _ in 0..400 {
            let picks = sample_paths(&weights, 1, ExplorationPolicy::Softmax, &c, &mut rng);
            let i = picks[0].index;
            let outcome = if i == 0 {
                Outcome::Rejected
            } else {
                Outcome::Delivered
            };
            weights[i] = apply_reward(weights[i], outcome, 1.0, 0.0, &c).after;
        }

        assert!(
            weights[1] > weights[0],
            "the delivering path must overtake the failing one: {weights:?} — \
             if it cannot, routing has ossified"
        );
    }

    #[test]
    fn receipt_deadline_and_influence_window_are_in_the_past() {
        let c = cfg();
        let now = 10_000 * NANOS_PER_SEC;
        assert!(c.receipt_deadline(now) < now);
        assert!(c.influence_window_start(now) < now);
        // Must not underflow near the epoch.
        assert_eq!(c.receipt_deadline(5), 0);
    }
}

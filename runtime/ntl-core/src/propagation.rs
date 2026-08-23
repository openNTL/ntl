//! Propagation engine for NTL signal routing.
//!
//! Determines how signals move through the synapse topology based on
//! relevance, weight, and activation patterns.

use serde::{Deserialize, Serialize};

use crate::signal::NodeId;
use crate::synapse::Synapse;

/// Propagation scope — determines how a signal routes through the network.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PropagationScope {
    /// Propagate to all active synapses. Use sparingly.
    Flood {
        /// Maximum hops for flood propagation.
        ///
        /// Enforced by [`check_propagable`], and separate from the signal's
        /// TTL: this is a bound the *emitter* sets on how far its flood may
        /// spread, whereas TTL is the general loop guard. Flood is the one
        /// scope that ignores `max_fanout`, so depth is the only thing
        /// bounding total fan-out — a flood of depth `d` over nodes of degree
        /// `f` touches on the order of `f^d` synapses.
        max_hops: u16,
    },
    /// Propagate to highest-scoring synapses (default).
    Weighted {
        /// Minimum synapse weight to consider.
        min_synapse_weight: f32,
    },
    /// Directed toward a specific destination node.
    Targeted {
        /// The target node.
        destination: NodeId,
    },
    /// Follow the gradient of type affinity.
    Gradient {
        /// Signal type to follow affinity for.
        signal_type: String,
    },
}

impl Default for PropagationScope {
    fn default() -> Self {
        Self::Weighted {
            min_synapse_weight: 0.0,
        }
    }
}

/// Configuration for the propagation engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PropagationConfig {
    /// Default propagation strategy.
    pub default_strategy: PropagationScope,
    /// Default TTL for signals.
    pub default_ttl: u16,
    /// Minimum signal weight to propagate.
    pub min_propagation_weight: f32,
    /// Default attenuation factor.
    pub attenuation_factor: f32,
    /// Maximum synapses to propagate to per hop.
    pub max_fanout: usize,
    /// Deduplication cache duration in seconds.
    ///
    /// MUST be at least the longest sender retry budget on the network: a
    /// shorter window means a late retry is processed as a new signal, which
    /// silently breaks idempotent handling.
    pub dedup_cache_seconds: u64,
    /// Scoring weights.
    pub scoring: ScoringWeights,
    /// How candidates are selected from their scores.
    pub exploration: crate::learning::ExplorationPolicy,
}

impl Default for PropagationConfig {
    fn default() -> Self {
        Self {
            default_strategy: PropagationScope::default(),
            default_ttl: 10,
            min_propagation_weight: 0.01,
            attenuation_factor: 0.9,
            max_fanout: 5,
            dedup_cache_seconds: 300,
            scoring: ScoringWeights::default(),
            exploration: crate::learning::ExplorationPolicy::Softmax,
        }
    }
}

impl PropagationConfig {
    /// Validate the configuration.
    ///
    /// # Errors
    /// Returns a description of the first invalid field.
    pub fn validate(&self, retry_deadline_secs: u64) -> Result<(), String> {
        if self.max_fanout == 0 {
            return Err("max_fanout must be at least 1".to_string());
        }
        if self.default_ttl == 0 {
            return Err("default_ttl must be at least 1".to_string());
        }
        if !(0.0..1.0).contains(&self.min_propagation_weight) {
            return Err(format!(
                "min_propagation_weight must be in [0, 1), got {}",
                self.min_propagation_weight
            ));
        }
        if !(0.0..=1.0).contains(&self.attenuation_factor) {
            return Err(format!(
                "attenuation_factor must be in [0, 1], got {}",
                self.attenuation_factor
            ));
        }
        if self.dedup_cache_seconds < retry_deadline_secs {
            return Err(format!(
                "dedup_cache_seconds ({}) must be at least the sender retry \
                 deadline ({retry_deadline_secs}s), or a late retry is \
                 processed as a new signal",
                self.dedup_cache_seconds
            ));
        }
        Ok(())
    }
}

/// Weights for the synapse scoring function.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoringWeights {
    /// Weight factor in scoring (default: 0.4).
    pub weight_factor: f32,
    /// Latency factor in scoring (default: 0.2).
    pub latency_factor: f32,
    /// Type affinity factor in scoring (default: 0.3).
    pub affinity_factor: f32,
    /// Recency factor in scoring (default: 0.1).
    pub recency_factor: f32,
}

impl Default for ScoringWeights {
    fn default() -> Self {
        Self {
            weight_factor: 0.4,
            latency_factor: 0.2,
            affinity_factor: 0.3,
            recency_factor: 0.1,
        }
    }
}

impl ScoringWeights {
    /// Sum of the factors, which should be 1.
    #[must_use]
    pub fn sum(&self) -> f32 {
        self.weight_factor + self.latency_factor + self.affinity_factor + self.recency_factor
    }

    /// Whether the factors sum to 1 within tolerance.
    ///
    /// Factors that do not sum to 1 rescale every score uniformly, which
    /// leaves ranking intact but changes what an exploration temperature
    /// means — the same `τ` becomes more or less greedy.
    #[must_use]
    pub fn is_normalized(&self) -> bool {
        (self.sum() - 1.0).abs() < 1e-3
    }
}

/// Score a synapse for signal propagation.
///
/// Higher scores indicate better candidates for carrying a signal.
#[must_use]
pub fn score_synapse(
    synapse: &Synapse,
    signal_type: &str,
    now_ns: u64,
    weights: &ScoringWeights,
) -> f32 {
    let weight_score = synapse.weight;

    let latency_score = if synapse.avg_latency_ns == 0 {
        1.0
    } else {
        1.0 / (1.0 + (synapse.avg_latency_ns as f32 / 1_000_000.0)) // Normalize to ms
    };

    let affinity_score = synapse.affinity_for(signal_type);

    let hours_since_active = if now_ns > synapse.last_active_ns {
        (now_ns - synapse.last_active_ns) as f32 / 3_600_000_000_000.0
    } else {
        0.0
    };
    let recency_score = 1.0 / (1.0 + hours_since_active);

    (weight_score * weights.weight_factor)
        + (latency_score * weights.latency_factor)
        + (affinity_score * weights.affinity_factor)
        + (recency_score * weights.recency_factor)
}

/// One synapse chosen to carry a signal.
#[derive(Debug, Clone, Copy)]
pub struct Chosen<'a> {
    /// The synapse.
    pub synapse: &'a Synapse,
    /// The score it was selected on.
    pub score: f32,
    /// Whether this came from exploration rather than exploitation.
    ///
    /// Journalled per decision so outcomes can be attributed, and so an
    /// operator can see whether the node is still exploring at all.
    pub explored: bool,
}

/// Select synapses to carry a signal.
///
/// Selection is **stochastic**, not deterministic top-N. Greedy selection
/// ossifies routing: a synapse that is never chosen is never rewarded, so its
/// weight only decays, so it is never chosen. Startup conditions become
/// permanent and the network cannot discover a path that became good later.
/// See [spec/propagation-rules][spec] and [`crate::learning::sample_paths`].
///
/// [spec]: https://openntl.org/spec/propagation-rules
///
/// `Targeted` scope with a direct synapse short-circuits without exploring:
/// exploration serves routing under uncertainty, and there is none here.
///
/// Returns choices ordered best-score-first.
// Eight parameters, all load-bearing: the topology, the scope, the type key,
// the arrival exclusion, two configs, and the injected clock and RNG. Bundling
// them into a struct would add a type used exactly once and hide which inputs
// the decision actually depends on.
#[allow(clippy::too_many_arguments)]
pub fn select_synapses<'a>(
    synapses: &'a [Synapse],
    scope: &PropagationScope,
    signal_type: &str,
    arrival_synapse: Option<&str>,
    config: &PropagationConfig,
    learning: &crate::learning::LearningConfig,
    now_ns: u64,
    rng: &mut dyn crate::rng::Rng,
) -> Vec<Chosen<'a>> {
    let eligible: Vec<&Synapse> = synapses
        .iter()
        .filter(|s| {
            // Never propagate back down the synapse the signal arrived on.
            if let Some(arrival_id) = arrival_synapse {
                if s.id.0 == arrival_id {
                    return false;
                }
            }
            // `can_carry`, not `== Active`: a Weakening synapse is still
            // connected, and must stay eligible or it can never earn its
            // weight back. See SynapseState::can_carry.
            s.state.can_carry()
        })
        .filter(|s| match scope {
            // Flood and Gradient consider every active synapse; Targeted
            // routes through whatever it has. Only Weighted applies a floor.
            PropagationScope::Flood { .. }
            | PropagationScope::Gradient { .. }
            | PropagationScope::Targeted { .. } => true,
            PropagationScope::Weighted { min_synapse_weight } => s.weight >= *min_synapse_weight,
        })
        .collect();

    if eligible.is_empty() {
        return Vec::new();
    }

    // A direct synapse to the destination is not a routing guess.
    if let PropagationScope::Targeted { destination } = scope {
        if let Some(direct) = eligible.iter().find(|s| &s.remote_node == destination) {
            return vec![Chosen {
                synapse: direct,
                score: score_synapse(direct, signal_type, now_ns, &config.scoring),
                explored: false,
            }];
        }
    }

    let scores: Vec<f32> = eligible
        .iter()
        .map(|s| score_synapse(s, signal_type, now_ns, &config.scoring))
        .collect();

    // Flood deliberately ignores fanout: reaching everything is the point.
    // Targeted without a direct synapse commits to one best guess.
    let k = match scope {
        PropagationScope::Flood { .. } => eligible.len(),
        PropagationScope::Targeted { .. } => 1,
        _ => config.max_fanout,
    };

    if matches!(scope, PropagationScope::Flood { .. }) {
        // Flooding selects everything, so sampling would only shuffle it.
        let mut all: Vec<Chosen<'a>> = eligible
            .iter()
            .zip(&scores)
            .map(|(s, &score)| Chosen {
                synapse: s,
                score,
                explored: false,
            })
            .collect();
        all.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        return all;
    }

    crate::learning::sample_paths(&scores, k, config.exploration, learning, rng)
        .into_iter()
        .map(|sel| Chosen {
            synapse: eligible[sel.index],
            score: sel.score,
            explored: sel.explored,
        })
        .collect()
}

/// Whether a signal may be propagated onward, and why not if not.
///
/// Encodes Propagation Rules 1-3 plus the acknowledged-delivery exception:
/// silent absorption below `min_propagation_weight` does not apply to an
/// acknowledged signal, whose sender must always learn the outcome.
///
/// Returns `Ok(())` when the signal may propagate.
///
/// # Errors
/// Returns the [`crate::delivery::RejectReason`] that blocks propagation.
pub fn check_propagable(
    signal: &crate::signal::Signal,
    local_node: &NodeId,
    config: &PropagationConfig,
) -> Result<(), crate::delivery::RejectReason> {
    use crate::delivery::RejectReason;

    if signal.ttl == 0 {
        return Err(RejectReason::TtlExhausted);
    }
    if signal.has_visited(local_node) {
        // Rule 2: our own id in the trace means a cycle.
        return Err(RejectReason::NoRoute);
    }
    if signal.weight < config.min_propagation_weight {
        return Err(RejectReason::BelowThreshold);
    }
    Ok(())
}

/// Whether a flood has reached the depth its emitter asked for.
///
/// Kept out of [`check_propagable`], which gates *acceptance* as well as
/// propagation. Refusing there made the outer ring of a flood reject the
/// signal instead of delivering it, so `max_hops: 3` reached two rings — the
/// bound belongs on the forwarding decision, not on whether this node may
/// process what it was sent.
///
/// The field previously existed and was read by nothing, so a flood ran to
/// TTL. Flood is also the one scope that ignores `max_fanout`, which makes
/// depth the sole bound on total fan-out: a flood of depth `d` across nodes of
/// degree `f` touches on the order of `f^d` synapses.
///
/// Trace length is the hop count — [`crate::signal::Signal::hop`] appends to it
/// on each forward.
#[must_use]
pub fn flood_depth_reached(signal: &crate::signal::Signal) -> bool {
    match &signal.scope {
        PropagationScope::Flood { max_hops } => signal.trace.len() >= usize::from(*max_hops),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::learning::{ExplorationPolicy, LearningConfig};
    use crate::rng::SplitMix64;
    use crate::signal::NodeId;
    use crate::synapse::{Synapse, SynapseConfig, SynapseState};

    const NOW: u64 = 1_700_000_000 * 1_000_000_000;

    fn make_synapse(id: &str, weight: f32, remote: u8) -> Synapse {
        let local = NodeId(vec![0u8; 32]);
        let mut rng = SplitMix64::seeded(u64::from(remote));
        let mut s = Synapse::new_with(
            local,
            NodeId(vec![remote; 32]),
            &SynapseConfig::default(),
            NOW,
            &mut rng,
        );
        s.id = crate::synapse::SynapseId(id.to_string());
        s.weight = weight;
        s.state = SynapseState::Active;
        s.last_active_ns = NOW;
        s
    }

    fn select<'a>(
        synapses: &'a [Synapse],
        scope: &PropagationScope,
        arrival: Option<&str>,
        config: &PropagationConfig,
        seed: u64,
    ) -> Vec<Chosen<'a>> {
        let mut rng = SplitMix64::seeded(seed);
        select_synapses(
            synapses,
            scope,
            "data",
            arrival,
            config,
            &LearningConfig::default(),
            NOW,
            &mut rng,
        )
    }

    #[test]
    fn flood_max_hops_bounds_forwarding_not_acceptance() {
        // Two bugs in one test. The field was declared and read by nothing, so
        // a flood ran to TTL; and the first fix put the bound in
        // `check_propagable`, which also gates *acceptance* — so the outermost
        // ring refused the flood instead of delivering it, and `max_hops: 2`
        // reached one ring.
        let config = PropagationConfig::default();
        let local = NodeId(vec![0u8; 32]);

        let mut signal = crate::signal::Signal::data("flood")
            .with_weight(0.9)
            .with_scope(PropagationScope::Flood { max_hops: 2 })
            .build_unsigned(NodeId(vec![7u8; 32]));

        // Zero hops: accept, and forward.
        assert!(check_propagable(&signal, &local, &config).is_ok());
        assert!(!flood_depth_reached(&signal));

        // One hop: accept, and still forward.
        signal.trace.push(NodeId(vec![1u8; 32]));
        assert!(check_propagable(&signal, &local, &config).is_ok());
        assert!(!flood_depth_reached(&signal));

        // Two hops: the emitter's depth is spent. The node must still *accept*
        // it — it was sent here, and refusing would mean the last ring of a
        // flood never sees it — but must not forward it further.
        signal.trace.push(NodeId(vec![2u8; 32]));
        assert!(
            check_propagable(&signal, &local, &config).is_ok(),
            "reaching max_hops must not make the signal unacceptable"
        );
        assert!(
            flood_depth_reached(&signal),
            "a flood must stop forwarding at max_hops even with TTL to spare"
        );
        assert!(signal.ttl > 0, "the point is that TTL was not the bound");

        // Other scopes are unaffected.
        let mut weighted = crate::signal::Signal::data("weighted")
            .with_weight(0.9)
            .build_unsigned(NodeId(vec![7u8; 32]));
        weighted.trace = vec![NodeId(vec![1u8; 32]), NodeId(vec![2u8; 32])];
        assert!(check_propagable(&weighted, &local, &config).is_ok());
        assert!(!flood_depth_reached(&weighted));
    }

    // -- scoring -----------------------------------------------------------

    #[test]
    fn default_scoring_weights_are_normalized() {
        assert!(
            ScoringWeights::default().is_normalized(),
            "factors must sum to 1, or the exploration temperature changes meaning"
        );
    }

    #[test]
    fn higher_weight_scores_higher() {
        let w = ScoringWeights::default();
        let strong = make_synapse("strong", 0.9, 1);
        let weak = make_synapse("weak", 0.1, 2);
        assert!(score_synapse(&strong, "data", NOW, &w) > score_synapse(&weak, "data", NOW, &w));
    }

    #[test]
    fn type_affinity_raises_the_score() {
        let w = ScoringWeights::default();
        let plain = make_synapse("plain", 0.5, 1);
        let mut affine = make_synapse("affine", 0.5, 2);
        affine.type_affinity.insert("data".to_string(), 10);
        assert!(
            score_synapse(&affine, "data", NOW, &w) > score_synapse(&plain, "data", NOW, &w),
            "a synapse with history for this type should be preferred"
        );
    }

    #[test]
    fn staleness_lowers_the_score() {
        let w = ScoringWeights::default();
        let fresh = make_synapse("fresh", 0.5, 1);
        let mut stale = make_synapse("stale", 0.5, 2);
        stale.last_active_ns = NOW - 100 * crate::time::NANOS_PER_HOUR;
        assert!(score_synapse(&fresh, "data", NOW, &w) > score_synapse(&stale, "data", NOW, &w));
    }

    // -- selection ---------------------------------------------------------

    #[test]
    fn weighted_selection_respects_fanout() {
        let synapses: Vec<Synapse> = (0..10)
            .map(|i| make_synapse(&format!("syn-{i:03}"), 0.5, i + 1))
            .collect();
        let config = PropagationConfig {
            max_fanout: 3,
            ..Default::default()
        };
        let picked = select(&synapses, &PropagationScope::default(), None, &config, 1);
        assert_eq!(picked.len(), 3);
    }

    #[test]
    fn selection_excludes_the_arrival_synapse() {
        let synapses = vec![
            make_synapse("arrival", 0.9, 1),
            make_synapse("other-1", 0.5, 2),
            make_synapse("other-2", 0.3, 3),
        ];
        let config = PropagationConfig::default();
        for seed in 0..50 {
            let picked = select(
                &synapses,
                &PropagationScope::default(),
                Some("arrival"),
                &config,
                seed,
            );
            assert!(
                picked.iter().all(|c| c.synapse.id.0 != "arrival"),
                "a signal must never be sent back the way it came"
            );
        }
    }

    #[test]
    fn selection_excludes_inactive_synapses() {
        let mut dormant = make_synapse("dormant", 0.9, 1);
        dormant.state = SynapseState::Dormant;
        let synapses = vec![dormant, make_synapse("active", 0.2, 2)];
        let picked = select(
            &synapses,
            &PropagationScope::default(),
            None,
            &PropagationConfig::default(),
            2,
        );
        assert!(picked.iter().all(|c| c.synapse.id.0 == "active"));
    }

    #[test]
    fn weighted_scope_applies_its_weight_floor() {
        let synapses = vec![
            make_synapse("heavy", 0.9, 1),
            make_synapse("light", 0.05, 2),
        ];
        let scope = PropagationScope::Weighted {
            min_synapse_weight: 0.5,
        };
        for seed in 0..30 {
            let picked = select(&synapses, &scope, None, &PropagationConfig::default(), seed);
            assert!(
                picked.iter().all(|c| c.synapse.weight >= 0.5),
                "exploration widens the choice among eligible paths; it does \
                 not override eligibility"
            );
        }
    }

    #[test]
    fn empty_topology_selects_nothing() {
        assert!(
            select(
                &[],
                &PropagationScope::default(),
                None,
                &PropagationConfig::default(),
                1
            )
            .is_empty()
        );
    }

    // -- exploration -------------------------------------------------------

    #[test]
    fn selection_is_stochastic_not_greedy() {
        // The anti-ossification property: over many draws the weakest
        // synapse must sometimes be chosen.
        let synapses = vec![
            make_synapse("best", 0.9, 1),
            make_synapse("mid", 0.5, 2),
            make_synapse("worst", 0.05, 3),
        ];
        let config = PropagationConfig {
            max_fanout: 1,
            ..Default::default()
        };
        let mut rng = SplitMix64::seeded(7);
        let mut picked_worst = 0;
        for _ in 0..3_000 {
            let picked = select_synapses(
                &synapses,
                &PropagationScope::Weighted {
                    min_synapse_weight: 0.0,
                },
                "data",
                None,
                &config,
                &LearningConfig::default(),
                NOW,
                &mut rng,
            );
            if picked[0].synapse.id.0 == "worst" {
                picked_worst += 1;
            }
        }
        assert!(
            picked_worst > 0,
            "greedy selection would starve the weak path forever, and its \
             weight would only decay — this is ossification"
        );
    }

    #[test]
    fn best_synapse_still_dominates() {
        let synapses = vec![make_synapse("best", 0.9, 1), make_synapse("worst", 0.05, 2)];
        let config = PropagationConfig {
            max_fanout: 1,
            ..Default::default()
        };
        let mut rng = SplitMix64::seeded(8);
        let mut best = 0;
        for _ in 0..1_000 {
            let picked = select_synapses(
                &synapses,
                &PropagationScope::default(),
                "data",
                None,
                &config,
                &LearningConfig::default(),
                NOW,
                &mut rng,
            );
            if picked[0].synapse.id.0 == "best" {
                best += 1;
            }
        }
        assert!(
            best > 500,
            "exploration must not swamp exploitation; got {best}/1000"
        );
    }

    #[test]
    fn epsilon_greedy_is_supported() {
        let synapses: Vec<Synapse> = (0..5)
            .map(|i| make_synapse(&format!("s{i}"), 0.1 * f32::from(i + 1), i + 1))
            .collect();
        let config = PropagationConfig {
            max_fanout: 2,
            exploration: ExplorationPolicy::EpsilonGreedy { epsilon: 0.5 },
            ..Default::default()
        };
        let picked = select(&synapses, &PropagationScope::default(), None, &config, 9);
        assert_eq!(picked.len(), 2);
    }

    #[test]
    fn results_are_ordered_by_score_descending() {
        let synapses: Vec<Synapse> = (0..6)
            .map(|i| make_synapse(&format!("s{i}"), 0.15 * f32::from(i + 1), i + 1))
            .collect();
        let config = PropagationConfig {
            max_fanout: 4,
            ..Default::default()
        };
        for seed in 0..20 {
            let picked = select(&synapses, &PropagationScope::default(), None, &config, seed);
            for w in picked.windows(2) {
                assert!(w[0].score >= w[1].score);
            }
        }
    }

    // -- scopes ------------------------------------------------------------

    #[test]
    fn flood_selects_every_active_synapse() {
        let synapses: Vec<Synapse> = (0..7)
            .map(|i| make_synapse(&format!("s{i}"), 0.3, i + 1))
            .collect();
        let config = PropagationConfig {
            max_fanout: 2, // deliberately smaller than the topology
            ..Default::default()
        };
        let picked = select(
            &synapses,
            &PropagationScope::Flood { max_hops: 3 },
            None,
            &config,
            3,
        );
        assert_eq!(picked.len(), 7, "flood must ignore the fanout limit");
    }

    #[test]
    fn targeted_prefers_a_direct_synapse_and_does_not_explore() {
        let synapses = vec![
            make_synapse("indirect-strong", 0.99, 1),
            make_synapse("direct", 0.05, 42),
        ];
        let scope = PropagationScope::Targeted {
            destination: NodeId(vec![42u8; 32]),
        };
        for seed in 0..50 {
            let picked = select(&synapses, &scope, None, &PropagationConfig::default(), seed);
            assert_eq!(picked.len(), 1);
            assert_eq!(
                picked[0].synapse.id.0, "direct",
                "a direct synapse is not a routing guess, even against a \
                 much stronger indirect path"
            );
            assert!(
                !picked[0].explored,
                "there is no uncertainty to explore here"
            );
        }
    }

    #[test]
    fn targeted_without_a_direct_synapse_picks_one_path() {
        let synapses = vec![make_synapse("a", 0.7, 1), make_synapse("b", 0.5, 2)];
        let scope = PropagationScope::Targeted {
            destination: NodeId(vec![99u8; 32]),
        };
        let picked = select(&synapses, &scope, None, &PropagationConfig::default(), 4);
        assert_eq!(
            picked.len(),
            1,
            "targeted routing commits to one best guess"
        );
    }

    // -- propagation rules -------------------------------------------------

    #[test]
    fn ttl_zero_blocks_propagation() {
        use crate::delivery::RejectReason;
        let me = NodeId(vec![1u8; 32]);
        let mut sig = crate::signal::Signal::data("t")
            .with_ttl(1)
            .build_unsigned(NodeId(vec![9u8; 32]));
        sig.ttl = 0;
        assert_eq!(
            check_propagable(&sig, &me, &PropagationConfig::default()),
            Err(RejectReason::TtlExhausted)
        );
    }

    #[test]
    fn own_id_in_trace_blocks_propagation() {
        use crate::delivery::RejectReason;
        let me = NodeId(vec![1u8; 32]);
        let mut sig = crate::signal::Signal::data("t").build_unsigned(NodeId(vec![9u8; 32]));
        sig.hop(me.clone());
        assert_eq!(
            check_propagable(&sig, &me, &PropagationConfig::default()),
            Err(RejectReason::NoRoute),
            "loop prevention: a node must not forward a signal it already saw"
        );
    }

    #[test]
    fn weight_below_floor_blocks_propagation() {
        use crate::delivery::RejectReason;
        let me = NodeId(vec![1u8; 32]);
        let mut sig = crate::signal::Signal::data("t").build_unsigned(NodeId(vec![9u8; 32]));
        sig.weight = 0.001;
        assert_eq!(
            check_propagable(&sig, &me, &PropagationConfig::default()),
            Err(RejectReason::BelowThreshold)
        );
    }

    #[test]
    fn healthy_signal_propagates() {
        let me = NodeId(vec![1u8; 32]);
        let sig = crate::signal::Signal::data("t")
            .with_weight(0.5)
            .build_unsigned(NodeId(vec![9u8; 32]));
        assert!(check_propagable(&sig, &me, &PropagationConfig::default()).is_ok());
    }

    // -- config validation -------------------------------------------------

    #[test]
    fn default_config_validates_against_default_retry_budget() {
        let retry = crate::delivery::RetryPolicy::default();
        PropagationConfig::default()
            .validate(retry.required_dedup_secs())
            .expect("defaults must be self-consistent");
    }

    #[test]
    fn dedup_window_shorter_than_retry_budget_is_rejected() {
        let c = PropagationConfig {
            dedup_cache_seconds: 10,
            ..Default::default()
        };
        let err = c.validate(300).unwrap_err();
        assert!(
            err.contains("dedup_cache_seconds"),
            "a dedup window shorter than the retry budget silently breaks \
             idempotent handling: {err}"
        );
    }

    #[test]
    fn zero_fanout_is_rejected() {
        let c = PropagationConfig {
            max_fanout: 0,
            ..Default::default()
        };
        assert!(c.validate(300).is_err());
    }
}

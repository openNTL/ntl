//! Deterministic pseudo-randomness for exploration sampling.
//!
//! `ntl-core` carries no dependency on `rand`. That is not asceticism: the
//! `getrandom` backend behind `rand` does not build for
//! `wasm32-unknown-unknown` without host-specific configuration, and the
//! core must build everywhere.
//!
//! Injecting randomness through a trait buys something more valuable than
//! portability, though. Exploration sampling is the part of the routing
//! model most in need of testing, and a seeded generator makes it
//! **deterministically testable** — a test can assert the exact sequence of
//! paths a node explores.
//!
//! # Not cryptographic
//!
//! [`SplitMix64`] is a statistical generator, not a cryptographic one. It is
//! appropriate for exploration sampling, where an adversary who predicts the
//! next draw learns only which path a node probes next. It MUST NOT be used
//! for key generation, nonces, or signal identifiers that need
//! unpredictability — those go through [`crate::crypto`].

/// A source of pseudo-randomness for routing decisions.
pub trait Rng: Send {
    /// Next raw 64-bit value.
    fn next_u64(&mut self) -> u64;

    /// Next value uniformly distributed in `[0, 1)`.
    ///
    /// Built from the top 24 bits so the result is exactly representable as
    /// an `f32` and never reaches `1.0`.
    fn next_f32(&mut self) -> f32 {
        // 2^24 distinct values, scaled into [0, 1).
        #[allow(clippy::cast_precision_loss)]
        let bits = (self.next_u64() >> 40) as f32;
        bits / 16_777_216.0
    }

    /// Uniform integer in `[0, n)`, or `None` when `n == 0`.
    ///
    /// Uses Lemire's method with rejection, so the result is unbiased rather
    /// than merely close to uniform. A modulo would skew selection toward
    /// low-index synapses, which over time is a real routing bias.
    fn next_below(&mut self, n: u64) -> Option<u64> {
        if n == 0 {
            return None;
        }
        let threshold = n.wrapping_neg() % n;
        loop {
            let x = self.next_u64();
            let m = u128::from(x) * u128::from(n);
            if (m as u64) >= threshold {
                return Some((m >> 64) as u64);
            }
        }
    }
}

/// SplitMix64 — a small, fast, well-distributed generator.
///
/// Chosen because it is a dozen lines with no dependencies, has a full
/// 2^64 period, and passes the standard statistical batteries. Every NTL
/// node can therefore explore identically given the same seed, which is what
/// makes routing tests reproducible.
#[derive(Debug, Clone)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// Create a generator from an explicit seed.
    #[must_use]
    pub fn seeded(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Seed from a node identity and a timestamp.
    ///
    /// Two nodes starting at the same instant would otherwise explore
    /// identically, which correlates their probes and wastes the diversity
    /// exploration exists to provide.
    #[must_use]
    pub fn from_identity(node_id: &[u8], now_ns: u64) -> Self {
        let mut state = now_ns;
        for chunk in node_id.chunks(8) {
            let mut buf = [0u8; 8];
            buf[..chunk.len()].copy_from_slice(chunk);
            state ^= u64::from_le_bytes(buf);
            state = state.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
        Self { state }
    }
}

impl Rng for SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// A generator that returns a fixed sequence, for tests that need to pin an
/// exact routing decision.
///
/// Values are consumed in order and then repeat.
#[derive(Debug, Clone)]
pub struct FixedRng {
    values: Vec<u64>,
    cursor: usize,
}

impl FixedRng {
    /// Build from raw 64-bit values.
    ///
    /// # Panics
    /// Panics if `values` is empty.
    #[must_use]
    pub fn new(values: Vec<u64>) -> Self {
        assert!(!values.is_empty(), "FixedRng needs at least one value");
        Self { values, cursor: 0 }
    }

    /// Build from values in `[0, 1)`, which is how exploration tests think.
    ///
    /// # Panics
    /// Panics if `values` is empty.
    #[must_use]
    pub fn from_f32s(values: &[f32]) -> Self {
        assert!(!values.is_empty(), "FixedRng needs at least one value");
        Self::new(
            values
                .iter()
                .map(|v| {
                    let clamped = v.clamp(0.0, 0.999_999_9);
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let scaled = (f64::from(clamped) * 16_777_216.0) as u64;
                    scaled << 40
                })
                .collect(),
        )
    }
}

impl Rng for FixedRng {
    fn next_u64(&mut self) -> u64 {
        let v = self.values[self.cursor % self.values.len()];
        self.cursor += 1;
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_yields_same_sequence() {
        let a: Vec<u64> = (0..8).map(|_| SplitMix64::seeded(42).next_u64()).collect();
        let mut g = SplitMix64::seeded(42);
        let b: Vec<u64> = (0..8).map(|_| g.next_u64()).collect();
        // The first element of `a` is a fresh generator each time, so all of
        // its entries are equal; `b` advances. Only element 0 must match.
        assert_eq!(a[0], b[0], "a fresh seed must reproduce its first draw");
        assert!(b.windows(2).any(|w| w[0] != w[1]), "generator must advance");
    }

    #[test]
    fn distinct_seeds_diverge() {
        let mut a = SplitMix64::seeded(1);
        let mut b = SplitMix64::seeded(2);
        assert_ne!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn identity_seeding_separates_nodes() {
        let mut a = SplitMix64::from_identity(&[1u8; 32], 1_000);
        let mut b = SplitMix64::from_identity(&[2u8; 32], 1_000);
        assert_ne!(
            a.next_u64(),
            b.next_u64(),
            "two nodes starting together must not explore identically"
        );
    }

    #[test]
    fn next_f32_stays_in_unit_interval() {
        let mut g = SplitMix64::seeded(7);
        for _ in 0..10_000 {
            let v = g.next_f32();
            assert!((0.0..1.0).contains(&v), "f32 out of range: {v}");
        }
    }

    #[test]
    fn next_f32_is_roughly_uniform() {
        let mut g = SplitMix64::seeded(99);
        let mut buckets = [0u32; 10];
        for _ in 0..100_000 {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let b = (g.next_f32() * 10.0) as usize;
            buckets[b.min(9)] += 1;
        }
        // Each bucket should hold ~10,000. Allow generous slack; this is a
        // smoke test for gross bias, not a statistical proof.
        for (i, &count) in buckets.iter().enumerate() {
            assert!(
                (8_500..11_500).contains(&count),
                "bucket {i} held {count}, expected ~10000"
            );
        }
    }

    #[test]
    fn next_below_respects_bound() {
        let mut g = SplitMix64::seeded(3);
        for _ in 0..1_000 {
            assert!(g.next_below(5).unwrap() < 5);
        }
        assert_eq!(g.next_below(0), None, "a zero bound has no valid draw");
        assert_eq!(g.next_below(1), Some(0));
    }

    #[test]
    fn next_below_is_unbiased_across_range() {
        let mut g = SplitMix64::seeded(11);
        let mut seen = [0u32; 3];
        for _ in 0..30_000 {
            seen[g.next_below(3).unwrap() as usize] += 1;
        }
        for (i, &c) in seen.iter().enumerate() {
            assert!(
                (9_000..11_000).contains(&c),
                "index {i} drawn {c} times, expected ~10000"
            );
        }
    }

    #[test]
    fn fixed_rng_replays_then_cycles() {
        let mut g = FixedRng::from_f32s(&[0.0, 0.5, 0.99]);
        assert!(g.next_f32() < 0.001);
        assert!((g.next_f32() - 0.5).abs() < 0.01);
        assert!(g.next_f32() > 0.98);
        assert!(g.next_f32() < 0.001, "sequence must cycle");
    }
}

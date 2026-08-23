//! A cryptographically secure [`Rng`] for the node runtime.
//!
//! `ntl-core` deliberately has no `rand` dependency — `getrandom` does not
//! build for `wasm32-unknown-unknown` without host configuration — so it ships
//! only [`ntl_core::rng::SplitMix64`], a statistical generator, and takes its
//! randomness through a trait. `rng.rs` says plainly what that generator is
//! not for: "It MUST NOT be used for key generation, nonces, or **signal
//! identifiers that need unpredictability**."
//!
//! Signal identifiers need unpredictability, and until this type existed the
//! node's default generator supplied them.
//!
//! # Why that mattered
//!
//! The default was `SplitMix64::from_identity(&identity.0, now)`. Two
//! properties combined badly:
//!
//! - **The seed is public.** A node's identity appears in the `origin` field of
//!   every signal it emits. Only the start-up timestamp was unknown.
//! - **SplitMix64 is invertible.** Every step of its output function is a
//!   bijection on 64 bits, so one output recovers the state. A ULID carries 80
//!   bits of randomness, which spans a whole `next_u64()` draw — so a *single*
//!   observed signal identifier recovered the state, and the timestamp never
//!   had to be guessed at all.
//!
//! An attacker who can see one signal could therefore enumerate every
//! identifier the node would go on to emit. Identifiers are the network's
//! deduplication key, and identities are free, so the attack is: peer with the
//! nodes the victim routes through, emit cheap signals carrying the victim's
//! *next* identifiers, and every relay then treats the victim's real signal as
//! a duplicate and drops it. No receipt comes back, the decision times out,
//! and the victim applies a **negative** weight update to an honest peer —
//! which the influence cap does not bound, because
//! [threat-model](https://openntl.org/spec/threat-model) §1 requires negative
//! updates to always apply. Repeated, it collapses the victim's weights on
//! every honest path at no cost to the attacker.
//!
//! # Why one generator rather than two
//!
//! Exploration sampling is happy with a statistical generator and benefits
//! from being seedable in tests. It would be reasonable to keep `SplitMix64`
//! for sampling and use this only for identifiers. The node exposes a single
//! `Rng`, though, and splitting it would mean a second injection point whose
//! only job is to be got right — so the runtime uses this for both. Sampling a
//! path costs one `next_u64`; ChaCha-class generators produce those at
//! gigabytes per second, and the node does far more expensive work per signal.
//!
//! Tests that need reproducible exploration still inject `SplitMix64`
//! directly.

use ntl_core::rng::Rng;
use rand::RngCore as _;
use rand_chacha::ChaCha12Rng;
use rand_chacha::rand_core::SeedableRng as _;

/// A ChaCha12-backed [`Rng`], seeded from the operating system.
///
/// ChaCha12 rather than the OS generator on every call: the syscall per draw
/// would show up in the propagation path, and a ChaCha stream seeded once from
/// the OS is not predictable from its output.
pub struct OsBackedRng {
    inner: ChaCha12Rng,
}

impl OsBackedRng {
    /// Seed from the operating system's entropy source.
    ///
    /// # Panics
    /// Panics if the OS entropy source is unavailable. That is the right
    /// response: a node whose randomness is not random cannot safely emit
    /// signals, and continuing with a fallback would hide it.
    #[must_use]
    pub fn from_os() -> Self {
        let mut seed = <ChaCha12Rng as SeedableRngShim>::Seed::default();
        rand::rngs::OsRng.fill_bytes(seed.as_mut());
        Self {
            inner: ChaCha12Rng::from_seed(seed),
        }
    }
}

/// Local alias so the seed type is nameable without importing `rand_core`
/// publicly.
trait SeedableRngShim {
    type Seed: Default + AsMut<[u8]>;
}

impl SeedableRngShim for ChaCha12Rng {
    type Seed = [u8; 32];
}

impl Rng for OsBackedRng {
    fn next_u64(&mut self) -> u64 {
        self.inner.next_u64()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_generators_do_not_agree() {
        // Seeded from the OS, so two instances must diverge immediately. A
        // deterministic default seed is exactly the bug this type exists to
        // remove.
        let mut a = OsBackedRng::from_os();
        let mut b = OsBackedRng::from_os();
        let left: Vec<u64> = (0..4).map(|_| a.next_u64()).collect();
        let right: Vec<u64> = (0..4).map(|_| b.next_u64()).collect();
        assert_ne!(left, right);
    }

    #[test]
    fn output_is_not_trivially_degenerate() {
        // Not a randomness test — just a guard against a wiring mistake that
        // returns a constant or zero, which a seeded generator would still
        // pass a "two instances differ" check on if only one were broken.
        let mut rng = OsBackedRng::from_os();
        let draws: Vec<u64> = (0..16).map(|_| rng.next_u64()).collect();
        assert!(draws.iter().any(|&v| v != 0));
        assert!(
            draws.windows(2).any(|w| w[0] != w[1]),
            "consecutive draws must not all be equal"
        );
    }

    #[test]
    fn next_below_stays_in_range() {
        let mut rng = OsBackedRng::from_os();
        for _ in 0..256 {
            let v = rng.next_below(7).expect("7 is non-zero");
            assert!(v < 7);
        }
        assert_eq!(rng.next_below(0), None);
    }
}

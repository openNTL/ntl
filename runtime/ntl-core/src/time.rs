//! Time as an injected dependency.
//!
//! Core logic never calls [`std::time::SystemTime::now`] directly. Two
//! reasons, in order of importance:
//!
//! 1. **Testability.** Decay half-lives, refractory periods, dedup TTLs, and
//!    receipt windows are all time-dependent. Injecting the clock lets a test
//!    advance a week instantly and assert the exact decayed weight, instead
//!    of sleeping or accepting a loose tolerance.
//! 2. **Portability.** `SystemTime::now()` panics at runtime on
//!    `wasm32-unknown-unknown`. Code that takes `now_ns` as a parameter has
//!    nothing to panic about.
//!
//! Timestamps are nanoseconds since the Unix epoch throughout, matching the
//! signal wire format.

/// A source of wall-clock time.
pub trait Clock: Send + Sync {
    /// Nanoseconds since the Unix epoch.
    fn now_ns(&self) -> u64;
}

/// Clock backed by the host operating system.
///
/// Not available on `wasm32-unknown-unknown`, where there is no such clock to
/// back it — a caller there must supply its own [`Clock`].
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

#[cfg(not(target_arch = "wasm32"))]
impl Clock for SystemClock {
    fn now_ns(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
    }
}

/// A clock a test drives by hand.
#[derive(Debug, Default)]
pub struct ManualClock {
    now_ns: std::sync::atomic::AtomicU64,
}

impl ManualClock {
    /// Start at a given instant.
    #[must_use]
    pub fn starting_at(now_ns: u64) -> Self {
        Self {
            now_ns: std::sync::atomic::AtomicU64::new(now_ns),
        }
    }

    /// Move the clock forward.
    pub fn advance_ns(&self, delta_ns: u64) {
        self.now_ns
            .fetch_add(delta_ns, std::sync::atomic::Ordering::SeqCst);
    }

    /// Move the clock forward by whole seconds.
    pub fn advance_secs(&self, secs: u64) {
        self.advance_ns(secs.saturating_mul(NANOS_PER_SEC));
    }

    /// Move the clock forward by whole hours.
    pub fn advance_hours(&self, hours: u64) {
        self.advance_secs(hours.saturating_mul(3_600));
    }
}

impl Clock for ManualClock {
    fn now_ns(&self) -> u64 {
        self.now_ns.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Nanoseconds in a second.
pub const NANOS_PER_SEC: u64 = 1_000_000_000;
/// Nanoseconds in an hour.
pub const NANOS_PER_HOUR: u64 = 3_600 * NANOS_PER_SEC;

/// Convert a nanosecond duration to hours as a float, for decay maths.
#[must_use]
pub fn ns_to_hours(ns: u64) -> f32 {
    #[allow(clippy::cast_precision_loss)]
    let hours = ns as f64 / NANOS_PER_HOUR as f64;
    #[allow(clippy::cast_possible_truncation)]
    let out = hours as f32;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_advances() {
        let c = ManualClock::starting_at(1_000);
        assert_eq!(c.now_ns(), 1_000);
        c.advance_ns(500);
        assert_eq!(c.now_ns(), 1_500);
        c.advance_secs(1);
        assert_eq!(c.now_ns(), 1_500 + NANOS_PER_SEC);
    }

    #[test]
    fn manual_clock_advances_hours() {
        let c = ManualClock::starting_at(0);
        c.advance_hours(24);
        assert_eq!(c.now_ns(), 24 * NANOS_PER_HOUR);
        assert!((ns_to_hours(c.now_ns()) - 24.0).abs() < 1e-3);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn system_clock_is_after_2020() {
        // 2020-01-01 in nanoseconds — a sanity check that the clock is real.
        assert!(SystemClock.now_ns() > 1_577_836_800_000_000_000);
    }
}

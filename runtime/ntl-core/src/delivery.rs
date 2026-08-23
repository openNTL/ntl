//! Delivery classes and receipts.
//!
//! Implements [spec/delivery-semantics][spec].
//!
//! [spec]: https://openntl.org/spec/delivery-semantics
//!
//! NTL previously offered one behaviour: emit and hope. A signal absorbed
//! below `min_propagation_weight` left the sender none the wiser. That is
//! fine for telemetry and disqualifying for anything with a side effect, so
//! there are now two classes and an application can tell which it has.

use serde::{Deserialize, Serialize};

use crate::signal::SignalId;

/// How hard a node must try, and whether failure is reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeliveryClass {
    /// No guarantee. May be silently absorbed. The default.
    BestEffort,
    /// At-least-once, or the sender learns it failed. Never silent.
    Acknowledged,
}

impl Default for DeliveryClass {
    fn default() -> Self {
        Self::BestEffort
    }
}

impl DeliveryClass {
    /// Wire representation (the `del` body field).
    #[must_use]
    pub fn to_wire(self) -> u8 {
        match self {
            Self::BestEffort => 0,
            Self::Acknowledged => 1,
        }
    }

    /// Parse from the wire representation.
    #[must_use]
    pub fn from_wire(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::BestEffort),
            1 => Some(Self::Acknowledged),
            _ => None,
        }
    }

    /// Whether a node must report failure rather than dropping silently.
    #[must_use]
    pub fn requires_receipt(self) -> bool {
        matches!(self, Self::Acknowledged)
    }
}

/// Why an acknowledged signal could not be delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// Weight fell below `min_propagation_weight`.
    BelowThreshold,
    /// TTL reached zero at a node that could not handle the signal.
    TtlExhausted,
    /// No candidate synapse passed scope filtering.
    NoRoute,
    /// The activation queue was full and overflow selected this signal.
    QueueFull,
    /// This node cannot handle the type and cannot forward it.
    UnsupportedType,
    /// Policy declined the signal.
    Refused,
}

impl RejectReason {
    /// Whether retrying could plausibly succeed.
    ///
    /// Retrying a terminal rejection is pure waste — the sender must stop.
    #[must_use]
    pub fn is_transient(self) -> bool {
        match self {
            Self::NoRoute | Self::QueueFull | Self::BelowThreshold => true,
            Self::TtlExhausted | Self::UnsupportedType | Self::Refused => false,
        }
    }

    /// Stable string for the receipt payload's `reason` field.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BelowThreshold => "below_threshold",
            Self::TtlExhausted => "ttl_exhausted",
            Self::NoRoute => "no_route",
            Self::QueueFull => "queue_full",
            Self::UnsupportedType => "unsupported_type",
            Self::Refused => "refused",
        }
    }
}

/// The outcome a receipt reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReceiptStatus {
    /// A node handled the signal.
    Delivered,
    /// A node determined it could not be delivered.
    Rejected(RejectReason),
}

/// The payload of a `Receipt` signal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Receipt {
    /// The signal being acknowledged.
    pub correlation_id: SignalId,
    /// What happened.
    pub status: ReceiptStatus,
    /// Hops taken to reach the handler.
    pub hops: u16,
}

impl Receipt {
    /// A positive receipt.
    #[must_use]
    pub fn delivered(correlation_id: SignalId, hops: u16) -> Self {
        Self {
            correlation_id,
            status: ReceiptStatus::Delivered,
            hops,
        }
    }

    /// A negative receipt.
    #[must_use]
    pub fn rejected(correlation_id: SignalId, reason: RejectReason, hops: u16) -> Self {
        Self {
            correlation_id,
            status: ReceiptStatus::Rejected(reason),
            hops,
        }
    }

    /// Whether this receipt reports success.
    #[must_use]
    pub fn is_delivered(&self) -> bool {
        matches!(self.status, ReceiptStatus::Delivered)
    }

    /// The outcome this receipt resolves a journalled decision to.
    #[must_use]
    pub fn outcome(&self) -> crate::store::Outcome {
        match self.status {
            ReceiptStatus::Delivered => crate::store::Outcome::Delivered,
            ReceiptStatus::Rejected(_) => crate::store::Outcome::Rejected,
        }
    }
}

/// Sender-side retry policy.
///
/// The **sender** retries; intermediate nodes do not. Hop-by-hop retry
/// multiplies traffic combinatorially and makes idempotency the whole
/// network's problem rather than the endpoints'.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// First backoff interval, in milliseconds.
    pub base_ms: u64,
    /// Ceiling on any single backoff, in milliseconds.
    pub cap_ms: u64,
    /// Maximum attempts, including the first.
    pub max_attempts: u32,
    /// Overall deadline, in seconds.
    pub total_deadline_secs: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            base_ms: 1_000,
            cap_ms: 30_000,
            max_attempts: 5,
            total_deadline_secs: 300,
        }
    }
}

impl RetryPolicy {
    /// Backoff before `attempt`, with full jitter.
    ///
    /// Full jitter — a uniform draw from `[0, backoff)` rather than the
    /// backoff itself — is what stops many senders that failed together from
    /// retrying together and re-creating the overload.
    ///
    /// Attempt numbering starts at 1; attempt 1 has no preceding delay.
    #[must_use]
    pub fn backoff_ms(&self, attempt: u32, rng: &mut dyn crate::rng::Rng) -> u64 {
        if attempt <= 1 {
            return 0;
        }
        let exp = (attempt - 2).min(31);
        let ceiling = self.base_ms.saturating_mul(1_u64 << exp).min(self.cap_ms);
        rng.next_below(ceiling.max(1)).unwrap_or(0)
    }

    /// Whether another attempt is permitted.
    ///
    /// A terminal rejection stops retrying regardless of budget.
    #[must_use]
    pub fn should_retry(
        &self,
        attempts_made: u32,
        elapsed_secs: u64,
        last_reason: Option<RejectReason>,
    ) -> bool {
        if let Some(reason) = last_reason {
            if !reason.is_transient() {
                return false;
            }
        }
        attempts_made < self.max_attempts && elapsed_secs < self.total_deadline_secs
    }

    /// Minimum deduplication retention this policy implies.
    ///
    /// A dedup window shorter than the retry budget means a late retry is
    /// processed as a new signal — the one misconfiguration that silently
    /// breaks idempotent handling.
    #[must_use]
    pub fn required_dedup_secs(&self) -> u64 {
        self.total_deadline_secs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::SplitMix64;

    #[test]
    fn best_effort_is_the_default() {
        assert_eq!(DeliveryClass::default(), DeliveryClass::BestEffort);
        assert!(!DeliveryClass::default().requires_receipt());
    }

    #[test]
    fn acknowledged_requires_a_receipt() {
        assert!(DeliveryClass::Acknowledged.requires_receipt());
    }

    #[test]
    fn delivery_class_wire_roundtrip() {
        for c in [DeliveryClass::BestEffort, DeliveryClass::Acknowledged] {
            assert_eq!(DeliveryClass::from_wire(c.to_wire()), Some(c));
        }
        assert_eq!(DeliveryClass::from_wire(2), None, "reserved values must not parse");
    }

    #[test]
    fn transient_and_terminal_reasons_are_separated() {
        assert!(RejectReason::NoRoute.is_transient());
        assert!(RejectReason::QueueFull.is_transient());
        assert!(!RejectReason::UnsupportedType.is_transient());
        assert!(!RejectReason::Refused.is_transient());
    }

    #[test]
    fn receipt_maps_to_outcome() {
        let id = SignalId::from_parts(1, 2);
        assert_eq!(
            Receipt::delivered(id, 3).outcome(),
            crate::store::Outcome::Delivered
        );
        assert_eq!(
            Receipt::rejected(id, RejectReason::NoRoute, 3).outcome(),
            crate::store::Outcome::Rejected
        );
    }

    #[test]
    fn first_attempt_has_no_delay() {
        let mut rng = SplitMix64::seeded(1);
        assert_eq!(RetryPolicy::default().backoff_ms(1, &mut rng), 0);
    }

    #[test]
    fn backoff_grows_and_respects_the_cap() {
        let p = RetryPolicy::default();
        let mut rng = SplitMix64::seeded(2);
        for attempt in 2..12 {
            let mut max_seen = 0;
            for _ in 0..200 {
                max_seen = max_seen.max(p.backoff_ms(attempt, &mut rng));
            }
            assert!(max_seen <= p.cap_ms, "attempt {attempt} exceeded the cap");
        }
    }

    #[test]
    fn backoff_is_jittered_not_fixed() {
        let p = RetryPolicy::default();
        let mut rng = SplitMix64::seeded(3);
        let draws: Vec<u64> = (0..50).map(|_| p.backoff_ms(4, &mut rng)).collect();
        assert!(
            draws.windows(2).any(|w| w[0] != w[1]),
            "full jitter must vary, or senders that failed together retry together"
        );
    }

    #[test]
    fn backoff_does_not_overflow_on_large_attempts() {
        let p = RetryPolicy::default();
        let mut rng = SplitMix64::seeded(4);
        assert!(p.backoff_ms(u32::MAX, &mut rng) <= p.cap_ms);
    }

    #[test]
    fn retry_stops_at_attempt_budget() {
        let p = RetryPolicy::default();
        assert!(p.should_retry(1, 0, None));
        assert!(!p.should_retry(p.max_attempts, 0, None));
    }

    #[test]
    fn retry_stops_at_deadline() {
        let p = RetryPolicy::default();
        assert!(!p.should_retry(1, p.total_deadline_secs, None));
    }

    #[test]
    fn retry_stops_immediately_on_terminal_rejection() {
        let p = RetryPolicy::default();
        assert!(
            !p.should_retry(1, 0, Some(RejectReason::UnsupportedType)),
            "retrying a terminal rejection is pure waste"
        );
        assert!(p.should_retry(1, 0, Some(RejectReason::NoRoute)));
    }

    #[test]
    fn dedup_window_must_cover_the_retry_budget() {
        let p = RetryPolicy::default();
        assert_eq!(p.required_dedup_secs(), p.total_deadline_secs);
    }
}

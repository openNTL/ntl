//! Signal types and construction for NTL.
//!
//! A signal is the fundamental data unit in NTL, replacing the concept of
//! a "request" or "message" from traditional protocols.

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::propagation::PropagationScope;

/// Unique identifier for a signal, based on ULID for lexicographic
/// time-ordering.
///
/// Serialises as the 16 raw bytes the wire format specifies, not as the
/// 26-character Crockford base32 string — the string form is for humans and
/// logs, and costs 10 extra bytes per signal on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SignalId(Ulid);

impl Serialize for SignalId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0.to_bytes())
    }
}

impl<'de> Deserialize<'de> for SignalId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = SignalId;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a 16-byte ULID")
            }

            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<SignalId, E> {
                let bytes: [u8; 16] = v.try_into().map_err(|_| {
                    E::invalid_length(v.len(), &"exactly 16 bytes")
                })?;
                Ok(SignalId::from_bytes(bytes))
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<SignalId, A::Error> {
                let mut bytes = [0u8; 16];
                for (i, slot) in bytes.iter_mut().enumerate() {
                    *slot = seq
                        .next_element()?
                        .ok_or_else(|| serde::de::Error::invalid_length(i, &"exactly 16 bytes"))?;
                }
                Ok(SignalId::from_bytes(bytes))
            }

            // JSON has no byte type, so accept the string form there too.
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<SignalId, E> {
                Ulid::from_string(v)
                    .map(SignalId)
                    .map_err(|e| E::custom(format!("invalid ULID {v:?}: {e}")))
            }
        }

        deserializer.deserialize_bytes(Visitor)
    }
}

impl SignalId {
    /// Build an identifier from an explicit millisecond timestamp and
    /// randomness.
    ///
    /// Time and randomness are injected rather than read from the ambient
    /// environment: `ulid`'s own constructor pulls in `getrandom`, which does
    /// not build for `wasm32-unknown-unknown`, and an injected clock makes
    /// ULID time-ordering directly testable.
    #[must_use]
    pub fn from_parts(timestamp_ms: u64, randomness: u128) -> Self {
        Self(Ulid::from_parts(timestamp_ms, randomness))
    }

    /// Generate an identifier from a clock and a randomness source.
    pub fn generate(clock: &dyn crate::time::Clock, rng: &mut dyn crate::rng::Rng) -> Self {
        let ms = clock.now_ns() / 1_000_000;
        let randomness = (u128::from(rng.next_u64()) << 64) | u128::from(rng.next_u64());
        Self::from_parts(ms, randomness)
    }

    /// Generate an identifier using the host clock.
    ///
    /// Convenience for binaries; core logic should prefer
    /// [`Self::generate`] so its time source stays injectable.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn generate_now(rng: &mut dyn crate::rng::Rng) -> Self {
        Self::generate(&crate::time::SystemClock, rng)
    }

    /// The timestamp component, in milliseconds since the Unix epoch.
    #[must_use]
    pub fn timestamp_ms(&self) -> u64 {
        self.0.timestamp_ms()
    }

    /// The 16-byte wire representation.
    #[must_use]
    pub fn to_bytes(self) -> [u8; 16] {
        self.0.to_bytes()
    }

    /// Parse from the 16-byte wire representation.
    #[must_use]
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(Ulid::from_bytes(bytes))
    }
}

impl std::fmt::Display for SignalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The type of a signal, determining how it's routed and processed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SignalType {
    /// Carries a data payload.
    Data,
    /// Requests data from the network.
    Query,
    /// Notifies of a state change.
    Event,
    /// Requests an action.
    Command,
    /// Maintains synapse liveness.
    Heartbeat,
    /// Announces node capability.
    Discovery,
    /// Reports the outcome of an acknowledged signal.
    ///
    /// Named `Ack` in 0.1.0-draft. The wire value is unchanged; the type now
    /// carries a structured outcome, because a bare confirmation cannot say
    /// *why* a signal failed.
    Receipt,
    /// Application-defined signal type.
    Custom(String),
}

impl SignalType {
    /// Convert to the wire format type byte.
    #[must_use]
    pub fn to_type_byte(&self) -> u8 {
        match self {
            Self::Data => 0,
            Self::Query => 1,
            Self::Event => 2,
            Self::Command => 3,
            Self::Heartbeat => 4,
            Self::Discovery => 5,
            Self::Receipt => 6,
            Self::Custom(_) => 15,
        }
    }

    /// Whether this type may itself request acknowledgement.
    ///
    /// A receipt is never acknowledged, so the protocol cannot recurse.
    #[must_use]
    pub fn can_be_acknowledged(&self) -> bool {
        !matches!(self, Self::Receipt)
    }

    /// Parse from wire format type byte.
    ///
    /// Returns `None` for byte 15 (`Custom`): the discriminant alone does not
    /// carry the application-defined name, so the caller must supply it.
    #[must_use]
    pub fn from_type_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Data),
            1 => Some(Self::Query),
            2 => Some(Self::Event),
            3 => Some(Self::Command),
            4 => Some(Self::Heartbeat),
            5 => Some(Self::Discovery),
            6 => Some(Self::Receipt),
            _ => None,
        }
    }
}

/// Encoding format for signal payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Encoding {
    /// CBOR — default, compact binary, self-describing.
    Cbor = 0,
    /// Protocol Buffers — when schema is shared.
    Protobuf = 1,
    /// Raw unstructured bytes.
    Raw = 2,
}

/// A node identifier, derived from the node's public key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub Vec<u8>);

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hex: String = self.0.iter().take(8).map(|b| format!("{b:02x}")).collect();
        write!(f, "ntl:{hex}...")
    }
}

/// An NTL signal — the fundamental data unit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    /// Unique signal identifier (ULID).
    pub id: SignalId,

    /// Signal type classification.
    pub signal_type: SignalType,

    /// Protocol version.
    pub version: u8,

    /// Emitting node identity.
    pub origin: NodeId,

    /// Cryptographic signature over the signal body.
    pub signature: Vec<u8>,

    /// Emission timestamp in nanoseconds since Unix epoch.
    pub timestamp: u64,

    /// The data payload.
    pub payload: serde_json::Value,

    /// Payload encoding format.
    pub encoding: Encoding,

    /// Signal weight / priority (0.0 - 1.0).
    pub weight: f32,

    /// Time-to-live in hops.
    pub ttl: u16,

    /// Propagation scope.
    pub scope: PropagationScope,

    /// Links to a related signal for request-response patterns.
    pub correlation_id: Option<SignalId>,

    /// Ordered list of nodes this signal has traversed.
    pub trace: Vec<NodeId>,

    /// Searchable tags.
    pub tags: Vec<String>,

    /// Delivery class.
    ///
    /// Part of the signed body, so an intermediate node cannot downgrade an
    /// acknowledged signal to best-effort.
    #[serde(default)]
    pub delivery: crate::delivery::DeliveryClass,
}

/// Builder for constructing signals with a fluent API.
pub struct SignalBuilder {
    signal_type: SignalType,
    topic: String,
    payload: serde_json::Value,
    weight: f32,
    ttl: u16,
    scope: PropagationScope,
    correlation_id: Option<SignalId>,
    tags: Vec<String>,
    delivery: crate::delivery::DeliveryClass,
}

impl Signal {
    /// Create a Data signal builder.
    #[must_use]
    pub fn data(topic: &str) -> SignalBuilder {
        SignalBuilder::new(SignalType::Data, topic)
    }

    /// Create a Query signal builder.
    #[must_use]
    pub fn query(topic: &str) -> SignalBuilder {
        SignalBuilder::new(SignalType::Query, topic)
    }

    /// Create an Event signal builder.
    #[must_use]
    pub fn event(topic: &str) -> SignalBuilder {
        SignalBuilder::new(SignalType::Event, topic)
    }

    /// Create a Command signal builder.
    #[must_use]
    pub fn command(topic: &str) -> SignalBuilder {
        SignalBuilder::new(SignalType::Command, topic)
    }

    /// Create a Discovery signal builder.
    #[must_use]
    pub fn discovery() -> SignalBuilder {
        SignalBuilder::new(SignalType::Discovery, "discovery")
    }

    /// Create a Heartbeat signal builder.
    #[must_use]
    pub fn heartbeat() -> SignalBuilder {
        SignalBuilder::new(SignalType::Heartbeat, "heartbeat")
    }

    /// Create a Receipt signal builder acknowledging `correlation_id`.
    ///
    /// Receipts always route `Targeted` toward the acknowledged signal's
    /// origin, and are themselves never acknowledged.
    #[must_use]
    pub fn receipt(receipt: &crate::delivery::Receipt, origin: NodeId) -> SignalBuilder {
        SignalBuilder::new(SignalType::Receipt, "receipt")
            .with_correlation(receipt.correlation_id)
            .with_scope(PropagationScope::Targeted { destination: origin })
            .with_payload(
                serde_json::to_value(receipt).unwrap_or(serde_json::Value::Null),
            )
    }

    /// Validate this signal according to the NTL specification.
    pub fn validate(&self) -> crate::Result<()> {
        if !(0.0..=1.0).contains(&self.weight) {
            return Err(crate::Error::InvalidSignal(format!(
                "weight {} out of range [0.0, 1.0]",
                self.weight
            )));
        }

        if self.ttl == 0 {
            return Err(crate::Error::TtlExpired(self.id.to_string()));
        }

        if self.signature.is_empty() {
            return Err(crate::Error::InvalidSignal(
                "missing signature".to_string(),
            ));
        }

        Ok(())
    }

    /// Check if this signal has exceeded its TTL.
    #[must_use]
    pub fn is_expired(&self) -> bool {
        self.ttl == 0
    }

    /// Decrement TTL and add a node to the trace.
    pub fn hop(&mut self, node_id: NodeId) {
        self.ttl = self.ttl.saturating_sub(1);
        if self.trace.len() < 64 {
            self.trace.push(node_id);
        }
    }

    /// Attenuate the signal weight by a factor.
    pub fn attenuate(&mut self, factor: f32) {
        self.weight *= factor;
        self.weight = self.weight.clamp(0.0, 1.0);
    }

    /// Check if a node ID appears in the trace (loop detection).
    #[must_use]
    pub fn has_visited(&self, node_id: &NodeId) -> bool {
        self.trace.contains(node_id)
    }

    /// Get the wire format size estimate in bytes.
    #[must_use]
    pub fn estimated_size(&self) -> usize {
        // Header (8) + approximate CBOR body
        8 + 16 // id
          + self.origin.0.len()
          + self.signature.len()
          + 8  // timestamp
          + 4  // weight
          + 2  // ttl
          + self.payload.to_string().len()
          + self.trace.len() * 32
          + self.tags.iter().map(String::len).sum::<usize>()
    }

    /// Maximum allowed signal size in bytes.
    pub const MAX_SIZE: usize = 1_048_576; // 1 MiB

    /// Whether this signal requires its failures to be reported.
    #[must_use]
    pub fn requires_receipt(&self) -> bool {
        self.delivery.requires_receipt()
    }

    /// Attenuate the weight for a hop, respecting the acknowledged floor.
    ///
    /// For an acknowledged signal the result is clamped at
    /// `min_propagation_weight` while TTL remains: without the clamp a path
    /// of more than a few hops guarantees a `below_threshold` rejection
    /// regardless of routing quality, which would make the class unusable at
    /// any distance.
    pub fn attenuate_for_hop(&mut self, factor: f32, min_propagation_weight: f32) {
        let attenuated = self.weight * factor;
        self.weight = if self.requires_receipt() && self.ttl > 0 {
            attenuated.max(min_propagation_weight)
        } else {
            attenuated
        }
        .clamp(0.0, 1.0);
    }

    /// Encode to CBOR for the wire.
    ///
    /// # Errors
    /// Returns [`crate::Error::Serialization`] if encoding fails, or
    /// [`crate::Error::InvalidSignal`] if the result exceeds
    /// [`Self::MAX_SIZE`].
    pub fn encode(&self) -> crate::Result<Vec<u8>> {
        let bytes = serde_cbor::to_vec(self)
            .map_err(|e| crate::Error::Serialization(e.to_string()))?;
        if bytes.len() > Self::MAX_SIZE {
            return Err(crate::Error::InvalidSignal(format!(
                "encoded signal is {} bytes, exceeding the {} byte maximum",
                bytes.len(),
                Self::MAX_SIZE
            )));
        }
        Ok(bytes)
    }

    /// Decode from CBOR.
    ///
    /// The size bound is checked *before* decoding: validation is ordered
    /// cheapest-first so an attacker cannot impose expensive work with
    /// malformed traffic.
    ///
    /// # Errors
    /// Returns [`crate::Error::InvalidSignal`] if the input exceeds
    /// [`Self::MAX_SIZE`], or [`crate::Error::Serialization`] if decoding
    /// fails.
    pub fn decode(bytes: &[u8]) -> crate::Result<Self> {
        if bytes.len() > Self::MAX_SIZE {
            return Err(crate::Error::InvalidSignal(format!(
                "signal is {} bytes, exceeding the {} byte maximum",
                bytes.len(),
                Self::MAX_SIZE
            )));
        }
        serde_cbor::from_slice(bytes).map_err(|e| crate::Error::Serialization(e.to_string()))
    }

    /// The bytes a signature covers: everything except the signature itself.
    ///
    /// # Errors
    /// Returns [`crate::Error::Serialization`] if encoding fails.
    pub fn signing_bytes(&self) -> crate::Result<Vec<u8>> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        // The trace grows as the signal travels, so it cannot be covered by
        // an origin signature that must still verify downstream.
        unsigned.trace = Vec::new();
        serde_cbor::to_vec(&unsigned).map_err(|e| crate::Error::Serialization(e.to_string()))
    }
}

impl SignalBuilder {
    fn new(signal_type: SignalType, topic: &str) -> Self {
        Self {
            signal_type,
            topic: topic.to_string(),
            payload: serde_json::Value::Null,
            weight: 0.5,
            ttl: 10,
            scope: PropagationScope::default(),
            correlation_id: None,
            tags: Vec::new(),
            delivery: crate::delivery::DeliveryClass::BestEffort,
        }
    }

    /// Set the signal payload.
    #[must_use]
    pub fn with_payload(mut self, payload: serde_json::Value) -> Self {
        self.payload = payload;
        self
    }

    /// Set the signal weight (0.0 - 1.0).
    #[must_use]
    pub fn with_weight(mut self, weight: f32) -> Self {
        self.weight = weight.clamp(0.0, 1.0);
        self
    }

    /// Set the time-to-live in hops.
    #[must_use]
    pub fn with_ttl(mut self, ttl: u16) -> Self {
        self.ttl = ttl;
        self
    }

    /// Set the propagation scope.
    #[must_use]
    pub fn with_scope(mut self, scope: PropagationScope) -> Self {
        self.scope = scope;
        self
    }

    /// Set a correlation ID for request-response patterns.
    #[must_use]
    pub fn with_correlation(mut self, id: SignalId) -> Self {
        self.correlation_id = Some(id);
        self
    }

    /// Set searchable tags.
    ///
    /// Tags are visible to every node on the path. Do not put sensitive data
    /// in them.
    #[must_use]
    pub fn with_tags(mut self, tags: Vec<&str>) -> Self {
        self.tags = tags.into_iter().map(String::from).collect();
        self
    }

    /// Set the delivery class.
    #[must_use]
    pub fn with_delivery(mut self, delivery: crate::delivery::DeliveryClass) -> Self {
        self.delivery = delivery;
        self
    }

    /// Request at-least-once delivery with failure reporting.
    ///
    /// Has no effect on a `Receipt`, which is never itself acknowledged.
    #[must_use]
    pub fn acknowledged(mut self) -> Self {
        if self.signal_type.can_be_acknowledged() {
            self.delivery = crate::delivery::DeliveryClass::Acknowledged;
        }
        self
    }

    /// Build an unsigned signal with an explicit clock and randomness
    /// source.
    ///
    /// The signal still needs signing before emission; see
    /// [`crate::node::Node::emit`].
    #[must_use]
    pub fn build_unsigned_with(
        self,
        origin: NodeId,
        clock: &dyn crate::time::Clock,
        rng: &mut dyn crate::rng::Rng,
    ) -> Signal {
        let now = clock.now_ns();
        let id = SignalId::generate(clock, rng);

        let mut tags = self.tags;
        if !self.topic.is_empty() {
            tags.insert(0, self.topic);
        }

        Signal {
            id,
            signal_type: self.signal_type,
            version: 1,
            origin,
            signature: Vec::new(), // Unsigned — must be signed before emission
            timestamp: now,
            payload: self.payload,
            encoding: Encoding::Cbor,
            weight: self.weight,
            ttl: self.ttl,
            scope: self.scope,
            correlation_id: self.correlation_id,
            trace: Vec::new(),
            tags,
            delivery: self.delivery,
        }
    }

    /// Build an unsigned signal using the host clock and a seeded generator.
    ///
    /// Convenience for tests and binaries. Core logic should prefer
    /// [`Self::build_unsigned_with`] so time and randomness stay injectable.
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub fn build_unsigned(self, origin: NodeId) -> Signal {
        use crate::time::Clock as _;
        let clock = crate::time::SystemClock;
        let mut rng = crate::rng::SplitMix64::from_identity(&origin.0, clock.now_ns());
        self.build_unsigned_with(origin, &clock, &mut rng)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::rng::SplitMix64;
    use crate::time::ManualClock;

    #[test]
    fn signal_id_is_unique() {
        let clock = ManualClock::starting_at(1_700_000_000 * 1_000_000_000);
        let mut rng = SplitMix64::seeded(1);
        let a = SignalId::generate(&clock, &mut rng);
        let b = SignalId::generate(&clock, &mut rng);
        assert_ne!(
            a, b,
            "two ids from the same millisecond must still differ in randomness"
        );
    }

    #[test]
    fn signal_id_is_time_ordered() {
        // No sleeping: the injected clock makes ULID ordering directly
        // assertable.
        let clock = ManualClock::starting_at(1_700_000_000 * 1_000_000_000);
        let mut rng = SplitMix64::seeded(2);
        let a = SignalId::generate(&clock, &mut rng);
        clock.advance_ns(5_000_000);
        let b = SignalId::generate(&clock, &mut rng);
        assert!(b.timestamp_ms() > a.timestamp_ms());
        assert!(
            b.to_string() > a.to_string(),
            "ULIDs must sort lexicographically by emission time"
        );
    }

    #[test]
    fn signal_id_bytes_roundtrip() {
        let id = SignalId::from_parts(1_700_000_000_123, 42);
        assert_eq!(SignalId::from_bytes(id.to_bytes()), id);
    }

    #[test]
    fn signal_builder_defaults() {
        let origin = NodeId(vec![0u8; 32]);
        let signal = Signal::data("test").build_unsigned(origin);

        assert_eq!(signal.signal_type, SignalType::Data);
        assert!((signal.weight - 0.5).abs() < f32::EPSILON);
        assert_eq!(signal.ttl, 10);
        assert!(signal.tags.contains(&"test".to_string()));
    }

    #[test]
    fn signal_weight_clamping() {
        let origin = NodeId(vec![0u8; 32]);
        let signal = Signal::data("test")
            .with_weight(1.5)
            .build_unsigned(origin);
        assert!((signal.weight - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn signal_hop_decrements_ttl() {
        let origin = NodeId(vec![0u8; 32]);
        let mut signal = Signal::data("test")
            .with_ttl(5)
            .build_unsigned(origin);

        let hop_node = NodeId(vec![1u8; 32]);
        signal.hop(hop_node.clone());

        assert_eq!(signal.ttl, 4);
        assert!(signal.has_visited(&hop_node));
    }

    #[test]
    fn signal_attenuation() {
        let origin = NodeId(vec![0u8; 32]);
        let mut signal = Signal::data("test")
            .with_weight(1.0)
            .build_unsigned(origin);

        signal.attenuate(0.9);
        assert!((signal.weight - 0.9).abs() < f32::EPSILON);

        signal.attenuate(0.9);
        assert!((signal.weight - 0.81).abs() < 0.001);
    }

    #[test]
    fn signal_validation_rejects_invalid_weight() {
        let origin = NodeId(vec![0u8; 32]);
        let mut signal = Signal::data("test").build_unsigned(origin);
        signal.weight = 1.5;
        signal.signature = vec![1]; // Non-empty to pass sig check
        assert!(signal.validate().is_err());
    }

    #[test]
    fn signal_type_byte_roundtrip() {
        let types: Vec<SignalType> = vec![
            SignalType::Data,
            SignalType::Query,
            SignalType::Event,
            SignalType::Command,
            SignalType::Heartbeat,
            SignalType::Discovery,
            SignalType::Receipt,
        ];
        for t in types {
            let byte = t.to_type_byte();
            let parsed = SignalType::from_type_byte(byte).expect("known type must parse");
            assert_eq!(t, parsed);
        }
    }

    #[test]
    fn signal_loop_detection() {
        let origin = NodeId(vec![0u8; 32]);
        let mut signal = Signal::data("test").build_unsigned(origin.clone());

        let node_a = NodeId(vec![1u8; 32]);
        let node_b = NodeId(vec![2u8; 32]);

        signal.hop(node_a.clone());
        signal.hop(node_b.clone());

        assert!(signal.has_visited(&node_a));
        assert!(signal.has_visited(&node_b));
        assert!(!signal.has_visited(&origin));
    }
}

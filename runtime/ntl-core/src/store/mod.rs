//! Pluggable persistence for NTL nodes.
//!
//! NTL does not depend on any particular database. Everything a node needs
//! to remember — synapse weights, topology knowledge, the deduplication
//! cache, activation state, and the learning journal — is reached through
//! the [`NodeStore`] trait. Backends live in their own crates:
//!
//! | Crate | Backend | Deployment class |
//! |---|---|---|
//! | `ntl-store-sqlite` | `SQLite` | Edge nodes (default) |
//! | `ntl-store-postgres` | `PostgreSQL` | Full nodes |
//! | — | Graph or KV stores | Ecosystem |
//!
//! The normative contract is [spec/storage-interface][spec]. This module is
//! the Rust expression of it; the two are meant to be read together.
//!
//! [spec]: https://openntl.org/spec/storage-interface
//!
//! # Why the trait is synchronous
//!
//! `ntl-core` makes no runtime assumptions — it carries no async executor and
//! must build for `wasm32-unknown-unknown`. A synchronous trait keeps that
//! promise and matches the backends that matter most: `SQLite`'s API is
//! blocking, and an edge node has no thread pool to spare. Backends whose
//! driver is async (`PostgreSQL`) wrap their calls in a blocking pool; see
//! `ntl-store-postgres` for the pattern.
//!
//! Every method takes `&self` rather than `&mut self` so a store can be
//! shared as `Arc<dyn NodeStore>` across the propagation, activation, and
//! learning paths. Implementations serialise internally.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::signal::{NodeId, SignalId, SignalType};
use crate::synapse::{SynapseId, SynapseState};

pub mod conformance;
mod memory;
pub use memory::MemoryStore;

/// Errors a storage backend can report.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The backend could not be reached or opened.
    #[error("storage backend unavailable: {0}")]
    Unavailable(String),

    /// A schema migration failed.
    #[error("migration failed at version {version}: {reason}")]
    Migration {
        /// Schema version being applied.
        version: u32,
        /// Why it failed.
        reason: String,
    },

    /// A stored record could not be decoded.
    #[error("corrupt record in {table}: {reason}")]
    Corrupt {
        /// Logical table the record came from.
        table: String,
        /// Why decoding failed.
        reason: String,
    },

    /// The write could not be durably committed.
    #[error("write failed: {0}")]
    Write(String),

    /// The operation is not supported by this backend.
    ///
    /// Backends MAY return this for optional capabilities — see
    /// [`NodeStore::signal_history_enabled`].
    #[error("operation not supported by this backend: {0}")]
    Unsupported(&'static str),
}

/// Result alias for storage operations.
pub type StoreResult<T> = std::result::Result<T, StoreError>;

/// How durably a backend persists writes.
///
/// This is a promise the backend makes to the runtime, not a request from
/// it. An edge node on flash may legitimately choose [`Self::BestEffort`]
/// to spare write cycles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Durability {
    /// Writes survive process and power loss before the call returns.
    Durable,
    /// Writes survive process loss, but a window may be lost to power loss.
    BestEffort,
    /// Nothing survives process exit. Valid only for tests and ephemeral
    /// edge nodes that re-bootstrap from scratch.
    Memory,
}

// ---------------------------------------------------------------------------
// Synapse records
// ---------------------------------------------------------------------------

/// The persistent projection of a synapse.
///
/// This is deliberately not [`crate::synapse::Synapse`]: the live struct
/// carries transport handles and derived counters, while this record is the
/// subset that MUST survive a restart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SynapseRecord {
    /// Synapse identifier.
    pub id: SynapseId,
    /// The peer on the far end.
    pub peer: NodeId,
    /// Learned weight in `[0, 1]`.
    pub weight: f32,
    /// Weight attenuation applied to signals crossing this synapse.
    pub attenuation_factor: f32,
    /// Lifecycle state.
    pub state: SynapseState,
    /// Per-signal-type success counts, keyed by signal type name.
    pub type_affinity: HashMap<String, u64>,
    /// Nanoseconds since the Unix epoch when the synapse was established.
    pub established_at_ns: u64,
    /// Nanoseconds since the Unix epoch of the last signal activity.
    pub last_active_ns: u64,
    /// Signals successfully transmitted.
    pub signals_transmitted: u64,
    /// Signals received.
    pub signals_received: u64,
    /// Exponentially weighted mean round-trip latency, in nanoseconds.
    pub avg_latency_ns: u64,
    /// Exponentially weighted transmission failure ratio in `[0, 1]`.
    pub error_rate: f32,
}

/// Filter for [`NodeStore::list_synapses`].
#[derive(Debug, Clone, Default)]
pub struct SynapseFilter {
    /// Only synapses in one of these states. Empty means any state.
    pub states: Vec<SynapseState>,
    /// Only synapses with weight at or above this value.
    pub min_weight: Option<f32>,
    /// Only synapses idle since before this timestamp — used by the decay
    /// and pruning sweeps.
    pub last_active_before_ns: Option<u64>,
    /// Cap on returned rows. `None` means no cap.
    pub limit: Option<usize>,
}

impl SynapseFilter {
    /// Filter matching only synapses in the `Active` state.
    ///
    /// For routing candidates prefer [`Self::eligible`]: a `Weakening`
    /// synapse is still connected and must remain able to earn its weight
    /// back.
    #[must_use]
    pub fn active() -> Self {
        Self {
            states: vec![SynapseState::Active],
            ..Self::default()
        }
    }

    /// Filter matching every synapse that may carry a signal.
    ///
    /// Includes `Weakening` as well as `Active` — see
    /// [`SynapseState::can_carry`] for why excluding it would make weight
    /// recovery impossible.
    #[must_use]
    pub fn eligible() -> Self {
        Self {
            states: vec![SynapseState::Active, SynapseState::Weakening],
            ..Self::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Topology records
// ---------------------------------------------------------------------------

/// What a node remembers about a peer it is not necessarily connected to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerRecord {
    /// Peer identity.
    pub id: NodeId,
    /// Dialable addresses, most recently useful first.
    pub addresses: Vec<String>,
    /// Region hint used for affinity routing, if the peer advertised one.
    pub region: Option<String>,
    /// Signal types the peer advertised handling.
    pub advertised_types: Vec<String>,
    /// Nanoseconds since the Unix epoch when the peer was last seen alive.
    pub last_seen_ns: u64,
    /// How the node learned about this peer.
    pub source: PeerSource,
}

/// Provenance of a topology entry.
///
/// Provenance is load-bearing for the eclipse-attack mitigations in the
/// threat model: a node MUST NOT let discovery-learned peers evict
/// configured ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerSource {
    /// Present in node configuration.
    Configured,
    /// A configured bootstrap node.
    Bootstrap,
    /// Learned from a Discovery signal.
    Discovered,
    /// Observed as the origin of a received signal.
    Observed,
}

// ---------------------------------------------------------------------------
// Activation state
// ---------------------------------------------------------------------------

/// A point-in-time snapshot of the activation gate.
///
/// Persisting this stops a restart from being a free reset of backpressure:
/// a node that is being flooded should come back still throttled.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ActivationSnapshot {
    /// Accumulated potential.
    pub potential: f32,
    /// Effective threshold, after any load adjustment.
    pub threshold: f32,
    /// Nanoseconds since the Unix epoch until which the node is refractory.
    pub refractory_until_ns: u64,
    /// Total fire count, for observability.
    pub signals_fired: u64,
    /// When this snapshot was taken.
    pub taken_at_ns: u64,
}

// ---------------------------------------------------------------------------
// Learning journal
// ---------------------------------------------------------------------------

/// Opaque handle to a journal entry, used to resolve its outcome later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct JournalId(pub u64);

/// The observed outcome of a routing decision.
///
/// This is the reward signal. Without it the routing model has no gradient
/// — see [spec/learning-model](https://openntl.org/spec/learning-model).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    /// No outcome observed yet. Carries no reward.
    Pending,
    /// A positive receipt arrived: the signal reached a node that handled it.
    Delivered,
    /// A negative receipt arrived: the path is known not to lead anywhere.
    Rejected,
    /// No receipt arrived within the aggregation window.
    TimedOut,
    /// The transmission itself failed at the transport layer.
    TransportFailure,
    /// The peer presented a signature that did not verify.
    SignatureFailure,
}

impl Outcome {
    /// The reward this outcome contributes, in `[-1, 1]`.
    ///
    /// [`Self::Pending`] yields `0.0` — an unresolved decision must not move
    /// weights in either direction.
    #[must_use]
    pub fn reward(self) -> f32 {
        match self {
            Self::Delivered => 1.0,
            Self::Pending => 0.0,
            Self::TimedOut => -0.25,
            Self::Rejected => -0.5,
            Self::TransportFailure => -0.5,
            Self::SignatureFailure => -1.0,
        }
    }

    /// Whether this outcome is terminal, i.e. no longer awaiting a receipt.
    #[must_use]
    pub fn is_resolved(self) -> bool {
        !matches!(self, Self::Pending)
    }
}

/// One routing decision and what came of it — the unit of training data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    /// Assigned by the store on append.
    pub id: Option<JournalId>,
    /// The signal whose routing this records.
    pub signal: SignalId,
    /// Signal type, kept denormalised so training can group by it without
    /// retaining signal bodies.
    pub signal_type: SignalType,
    /// The synapse chosen.
    pub synapse: SynapseId,
    /// The peer on the far end at decision time.
    pub peer: NodeId,
    /// Score the propagation engine assigned this path.
    ///
    /// Records *why* the path was chosen. Distinct from `signal_weight`:
    /// the score is a composite of weight, affinity, latency, and recency,
    /// whereas `signal_weight` is the carried signal's own priority.
    pub score: f32,
    /// Weight of the signal that was carried.
    ///
    /// The reward rule scales by this (`Δw = η · r · x`), so a low-weight
    /// signal teaches less — which is what stops influence being bought
    /// cheaply by flooding negligible traffic. It must be recorded at
    /// decision time, because by the time a receipt arrives the signal is
    /// gone.
    pub signal_weight: f32,
    /// Whether this choice came from exploration rather than exploitation.
    ///
    /// Exploration decisions are the ones that carry information about paths
    /// the model currently underrates, so they are worth distinguishing.
    pub explored: bool,
    /// When the decision was made.
    pub decided_at_ns: u64,
    /// What came of it.
    pub outcome: Outcome,
    /// When the outcome was observed, if it has been.
    pub resolved_at_ns: Option<u64>,
}

// ---------------------------------------------------------------------------
// The trait
// ---------------------------------------------------------------------------

/// Everything an NTL node persists.
///
/// See the module documentation for why this is synchronous and takes
/// `&self`. Implementations MUST be safe to call concurrently.
pub trait NodeStore: Send + Sync {
    // -- lifecycle ---------------------------------------------------------

    /// Apply any outstanding schema migrations.
    ///
    /// MUST be idempotent: calling it on an up-to-date store is a no-op.
    ///
    /// # Errors
    /// Returns [`StoreError::Migration`] if a migration cannot be applied.
    fn migrate(&self) -> StoreResult<()>;

    /// The durability guarantee this backend provides.
    fn durability(&self) -> Durability;

    /// Force any buffered writes to their durable home.
    ///
    /// # Errors
    /// Returns [`StoreError::Write`] if the flush fails.
    fn flush(&self) -> StoreResult<()>;

    // -- synapses ----------------------------------------------------------

    /// Insert or replace a synapse record.
    ///
    /// # Errors
    /// Returns [`StoreError::Write`] if the record cannot be persisted.
    fn put_synapse(&self, record: &SynapseRecord) -> StoreResult<()>;

    /// Fetch a synapse by identifier.
    ///
    /// # Errors
    /// Returns [`StoreError::Corrupt`] if the stored row cannot be decoded.
    fn get_synapse(&self, id: &SynapseId) -> StoreResult<Option<SynapseRecord>>;

    /// Fetch the synapse to a given peer, if one exists.
    ///
    /// # Errors
    /// Returns [`StoreError::Corrupt`] if the stored row cannot be decoded.
    fn synapse_for_peer(&self, peer: &NodeId) -> StoreResult<Option<SynapseRecord>>;

    /// List synapses matching a filter, ordered by weight descending.
    ///
    /// # Errors
    /// Returns [`StoreError::Corrupt`] if a stored row cannot be decoded.
    fn list_synapses(&self, filter: &SynapseFilter) -> StoreResult<Vec<SynapseRecord>>;

    /// Remove a synapse record.
    ///
    /// Removing an absent synapse is not an error.
    ///
    /// # Errors
    /// Returns [`StoreError::Write`] if the deletion fails.
    fn delete_synapse(&self, id: &SynapseId) -> StoreResult<()>;

    // -- topology ----------------------------------------------------------

    /// Insert or replace a peer record.
    ///
    /// # Errors
    /// Returns [`StoreError::Write`] if the record cannot be persisted.
    fn put_peer(&self, record: &PeerRecord) -> StoreResult<()>;

    /// Fetch a peer record.
    ///
    /// # Errors
    /// Returns [`StoreError::Corrupt`] if the stored row cannot be decoded.
    fn get_peer(&self, id: &NodeId) -> StoreResult<Option<PeerRecord>>;

    /// List known peers, most recently seen first.
    ///
    /// When `region` is `Some`, only peers advertising that region are
    /// returned.
    ///
    /// # Errors
    /// Returns [`StoreError::Corrupt`] if a stored row cannot be decoded.
    fn list_peers(&self, region: Option<&str>, limit: usize) -> StoreResult<Vec<PeerRecord>>;

    /// Count known peers by provenance.
    ///
    /// The eclipse-attack mitigation needs this to enforce a floor on
    /// configured peers.
    ///
    /// # Errors
    /// Returns [`StoreError::Unavailable`] if the backend cannot be read.
    fn count_peers(&self, source: PeerSource) -> StoreResult<u64>;

    // -- deduplication -----------------------------------------------------

    /// Record that a signal has been seen, returning whether it had been
    /// seen already.
    ///
    /// This MUST be atomic: a concurrent caller must not also observe
    /// `false`. Loop prevention (Propagation Rule 4) depends on it.
    ///
    /// `ttl_secs` sets how long the entry is retained.
    ///
    /// # Errors
    /// Returns [`StoreError::Write`] if the entry cannot be recorded.
    fn check_and_set_seen(&self, id: &SignalId, now_ns: u64, ttl_secs: u64) -> StoreResult<bool>;

    /// Whether a signal is in the deduplication cache, without inserting it.
    ///
    /// # Errors
    /// Returns [`StoreError::Unavailable`] if the backend cannot be read.
    fn has_seen(&self, id: &SignalId, now_ns: u64) -> StoreResult<bool>;

    /// Drop expired deduplication entries, returning how many were removed.
    ///
    /// # Errors
    /// Returns [`StoreError::Write`] if the purge fails.
    fn purge_expired_seen(&self, now_ns: u64) -> StoreResult<u64>;

    // -- activation --------------------------------------------------------

    /// Persist the activation snapshot, replacing any previous one.
    ///
    /// # Errors
    /// Returns [`StoreError::Write`] if the snapshot cannot be persisted.
    fn save_activation(&self, snapshot: &ActivationSnapshot) -> StoreResult<()>;

    /// Load the most recent activation snapshot.
    ///
    /// # Errors
    /// Returns [`StoreError::Corrupt`] if the stored snapshot cannot be
    /// decoded.
    fn load_activation(&self) -> StoreResult<Option<ActivationSnapshot>>;

    // -- learning journal --------------------------------------------------

    /// Append a routing decision, returning its assigned identifier.
    ///
    /// # Errors
    /// Returns [`StoreError::Write`] if the entry cannot be appended.
    fn append_decision(&self, entry: &JournalEntry) -> StoreResult<JournalId>;

    /// Attach an observed outcome to a previously recorded decision.
    ///
    /// Resolving an already-resolved entry MUST NOT overwrite it: the first
    /// receipt wins. This makes receipt handling idempotent, which the
    /// at-least-once delivery class requires.
    ///
    /// Returns the entry as it now stands, or `None` if the identifier is
    /// unknown — an unknown identifier is not an error, since the journal
    /// may have been trimmed.
    ///
    /// # Errors
    /// Returns [`StoreError::Write`] if the update fails.
    fn resolve_decision(
        &self,
        id: JournalId,
        outcome: Outcome,
        now_ns: u64,
    ) -> StoreResult<Option<JournalEntry>>;

    /// Find the decision awaiting a receipt for a given signal and peer.
    ///
    /// Receipts name the signal they acknowledge, not the journal entry, so
    /// the runtime needs this to route a receipt back to its decision.
    ///
    /// # Errors
    /// Returns [`StoreError::Corrupt`] if a stored row cannot be decoded.
    fn pending_decision_for(
        &self,
        signal: &SignalId,
        peer: &NodeId,
    ) -> StoreResult<Option<JournalEntry>>;

    /// Decisions still pending whose deadline has passed.
    ///
    /// The learning loop calls this to convert silence into
    /// [`Outcome::TimedOut`] — the step that makes failure observable rather
    /// than silent.
    ///
    /// # Errors
    /// Returns [`StoreError::Corrupt`] if a stored row cannot be decoded.
    fn expired_decisions(&self, deadline_ns: u64, limit: usize) -> StoreResult<Vec<JournalEntry>>;

    /// Most recent resolved decisions, newest first, optionally for one
    /// signal type.
    ///
    /// # Errors
    /// Returns [`StoreError::Corrupt`] if a stored row cannot be decoded.
    fn recent_decisions(
        &self,
        signal_type: Option<&SignalType>,
        limit: usize,
    ) -> StoreResult<Vec<JournalEntry>>;

    /// Drop journal entries older than a cutoff, returning how many went.
    ///
    /// # Errors
    /// Returns [`StoreError::Write`] if the trim fails.
    fn trim_journal(&self, older_than_ns: u64) -> StoreResult<u64>;

    // -- influence accounting ---------------------------------------------

    /// Record a peer's attempt to move its own weight, and return the total
    /// it has accumulated in the window starting at `window_start_ns`.
    ///
    /// The caller compares the return value against the configured
    /// per-identity cap. Keeping the accounting in the store — rather than
    /// in memory — is what stops an attacker from resetting their budget by
    /// forcing a restart.
    ///
    /// # Errors
    /// Returns [`StoreError::Write`] if the entry cannot be recorded.
    fn record_influence(
        &self,
        peer: &NodeId,
        magnitude: f32,
        now_ns: u64,
        window_start_ns: u64,
    ) -> StoreResult<f32>;

    /// Total influence a peer has accumulated since `window_start_ns`.
    ///
    /// # Errors
    /// Returns [`StoreError::Unavailable`] if the backend cannot be read.
    fn influence_since(&self, peer: &NodeId, window_start_ns: u64) -> StoreResult<f32>;

    /// Drop influence records older than a cutoff, returning how many went.
    ///
    /// # Errors
    /// Returns [`StoreError::Write`] if the purge fails.
    fn purge_influence(&self, older_than_ns: u64) -> StoreResult<u64>;

    // -- node metadata -----------------------------------------------------

    /// Store an opaque metadata value — node identity, schema notes, and
    /// similar single-valued state.
    ///
    /// # Errors
    /// Returns [`StoreError::Write`] if the value cannot be persisted.
    fn put_meta(&self, key: &str, value: &[u8]) -> StoreResult<()>;

    /// Read an opaque metadata value.
    ///
    /// # Errors
    /// Returns [`StoreError::Unavailable`] if the backend cannot be read.
    fn get_meta(&self, key: &str) -> StoreResult<Option<Vec<u8>>>;

    // -- optional: signal history -----------------------------------------

    /// Whether this backend retains signal bodies for replay and debugging.
    ///
    /// Retention is optional and off by default: signal bodies are the most
    /// privacy-sensitive thing a node touches, and an edge node has no room
    /// for them.
    fn signal_history_enabled(&self) -> bool {
        false
    }

    /// Retain a signal body for later replay.
    ///
    /// Backends that do not retain history MUST return
    /// [`StoreError::Unsupported`] rather than silently discarding.
    ///
    /// # Errors
    /// Returns [`StoreError::Unsupported`] when history is disabled, or
    /// [`StoreError::Write`] if the write fails.
    fn put_signal_history(&self, _id: &SignalId, _body: &[u8], _now_ns: u64) -> StoreResult<()> {
        Err(StoreError::Unsupported("signal history"))
    }

    /// Read back a retained signal body.
    ///
    /// # Errors
    /// Returns [`StoreError::Unsupported`] when history is disabled.
    fn get_signal_history(&self, _id: &SignalId) -> StoreResult<Option<Vec<u8>>> {
        Err(StoreError::Unsupported("signal history"))
    }
}

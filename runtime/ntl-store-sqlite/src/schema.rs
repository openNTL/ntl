//! Embedded schema migrations.
//!
//! Migrations are compiled in rather than shipped as files: zero-config
//! startup on an edge device cannot depend on an operator running a separate
//! tool, or on files being present next to the binary.

/// One migration step.
pub struct Migration {
    /// Target schema version.
    pub version: u32,
    /// What it does, for logs and diagnostics.
    pub description: &'static str,
    /// The statements to run, as one script.
    pub sql: &'static str,
}

/// The schema version this build understands.
pub const CURRENT_VERSION: u32 = 2;

/// All migrations, in order.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "initial schema",
        sql: r"
-- Synapses: the learned model. Losing these discards everything the node
-- learned, so they are unconditionally persisted.
CREATE TABLE IF NOT EXISTS synapses (
    id                  TEXT PRIMARY KEY,
    peer                BLOB NOT NULL,
    weight              REAL NOT NULL,
    attenuation_factor  REAL NOT NULL,
    state               TEXT NOT NULL,
    type_affinity       TEXT NOT NULL,   -- JSON object: type -> count
    established_at_ns   INTEGER NOT NULL,
    last_active_ns      INTEGER NOT NULL,
    signals_transmitted INTEGER NOT NULL,
    signals_received    INTEGER NOT NULL,
    avg_latency_ns      INTEGER NOT NULL,
    error_rate          REAL NOT NULL
);
-- Listing is ordered by weight descending on every routing decision.
CREATE INDEX IF NOT EXISTS idx_synapses_weight ON synapses(weight DESC);
-- One synapse per peer, which also makes synapse_for_peer an index lookup.
CREATE UNIQUE INDEX IF NOT EXISTS idx_synapses_peer ON synapses(peer);
CREATE INDEX IF NOT EXISTS idx_synapses_state ON synapses(state);
CREATE INDEX IF NOT EXISTS idx_synapses_last_active ON synapses(last_active_ns);

-- Topology knowledge. `source` is load-bearing: discovery-learned peers must
-- not evict configured ones (threat-model, eclipse attacks).
CREATE TABLE IF NOT EXISTS peers (
    id               BLOB PRIMARY KEY,
    addresses        TEXT NOT NULL,      -- JSON array
    region           TEXT,
    advertised_types TEXT NOT NULL,      -- JSON array
    last_seen_ns     INTEGER NOT NULL,
    source           TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_peers_region ON peers(region);
CREATE INDEX IF NOT EXISTS idx_peers_last_seen ON peers(last_seen_ns DESC);
CREATE INDEX IF NOT EXISTS idx_peers_source ON peers(source);

-- Deduplication cache. The primary key is what makes check-and-set atomic:
-- INSERT ... ON CONFLICT decides presence and inserts in one statement.
CREATE TABLE IF NOT EXISTS seen_signals (
    id         BLOB PRIMARY KEY,
    expires_ns INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_seen_expires ON seen_signals(expires_ns);

-- Activation snapshot. A single row: snapshots replace, never accumulate.
CREATE TABLE IF NOT EXISTS activation (
    singleton           INTEGER PRIMARY KEY CHECK (singleton = 0),
    potential           REAL NOT NULL,
    threshold           REAL NOT NULL,
    refractory_until_ns INTEGER NOT NULL,
    signals_fired       INTEGER NOT NULL,
    taken_at_ns         INTEGER NOT NULL
);

-- The learning journal: one routing decision and its observed outcome.
CREATE TABLE IF NOT EXISTS journal (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    signal         BLOB NOT NULL,
    signal_type    TEXT NOT NULL,
    synapse        TEXT NOT NULL,
    peer           BLOB NOT NULL,
    score          REAL NOT NULL,
    signal_weight  REAL NOT NULL,
    explored       INTEGER NOT NULL,
    decided_at_ns  INTEGER NOT NULL,
    outcome        TEXT NOT NULL,
    resolved_at_ns INTEGER
);
-- Routing a receipt back to its decision.
CREATE INDEX IF NOT EXISTS idx_journal_signal_peer ON journal(signal, peer);
-- The timeout sweep scans unresolved decisions by age.
CREATE INDEX IF NOT EXISTS idx_journal_pending ON journal(outcome, decided_at_ns);
CREATE INDEX IF NOT EXISTS idx_journal_decided ON journal(decided_at_ns DESC);

-- Influence accounting, persisted so a forced restart cannot reset an
-- attacker's budget (threat-model §1).
CREATE TABLE IF NOT EXISTS influence (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    peer      BLOB NOT NULL,
    magnitude REAL NOT NULL,
    at_ns     INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_influence_peer_at ON influence(peer, at_ns);

-- Node identity and other single-valued state.
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value BLOB NOT NULL
);

-- Optional signal bodies for replay. Off unless explicitly enabled.
CREATE TABLE IF NOT EXISTS signal_history (
    id       BLOB PRIMARY KEY,
    body     BLOB NOT NULL,
    added_ns INTEGER NOT NULL
);
",
    },
    Migration {
        version: 2,
        description: "signature-failure window on synapses",
        sql: r"
-- threat-model §4 requires pruning a synapse that accumulates
-- `signature_failure_prune_threshold` failures within one influence window.
-- A count over a window needs the count and the window's start persisted, or a
-- restart is a free amnesty for an attacker mid-way to the threshold.
ALTER TABLE synapses ADD COLUMN signature_failures INTEGER NOT NULL DEFAULT 0;
ALTER TABLE synapses ADD COLUMN failure_window_start_ns INTEGER NOT NULL DEFAULT 0;
",
    },
];

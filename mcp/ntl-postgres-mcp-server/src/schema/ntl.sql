-- openNTL PostgreSQL schema.
--
-- This is the reference schema for a Postgres-backed NTL node. It mirrors the
-- SQLite schema in `runtime/ntl-store-sqlite/src/schema.rs`, and is the schema
-- the Rust `ntl-store-postgres` backend will implement against.
--
-- Applied by the `ntl_init_schema` tool. Every statement is idempotent, so
-- re-running it is safe.
--
-- Notes on the Postgres-specific choices, since they are not all obvious:
--
--  * Timestamps are BIGINT holding nanoseconds since the Unix epoch. NTL
--    timestamps are u64 and Postgres BIGINT is signed, so a value above
--    2^63-1 must be saturated by the writer, not wrapped — wrapping would
--    make a far-future timestamp sort before everything.
--
--  * Weights are REAL (float4), matching the f32 the routing model actually
--    uses. Widening to double precision would imply precision the model does
--    not have.
--
--  * type_affinity is JSONB rather than TEXT, so affinity is queryable
--    server-side. That is what lets the MCP server answer "which peers are
--    good for Query signals" without pulling every row.
--
--  * Tables live in their own schema (default `ntl`) rather than `public`, so
--    NTL state is separable from whatever else the database holds.

-- {{SCHEMA}} is substituted by the server with a validated, quoted identifier
-- (see safety.ts quoteIdent). It is NOT a bind parameter: Postgres cannot bind
-- identifiers, so this is the one place a name reaches SQL by interpolation,
-- and it is why quoteIdent rejects anything but a plain identifier.
CREATE SCHEMA IF NOT EXISTS {{SCHEMA}};

SET LOCAL search_path TO {{SCHEMA}};

-- Schema version, replacing SQLite's PRAGMA user_version.
CREATE TABLE IF NOT EXISTS schema_version (
    singleton   BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    version     INTEGER NOT NULL,
    applied_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Synapses: the learned model. Losing these discards everything the node
-- learned, so they are unconditionally persisted.
CREATE TABLE IF NOT EXISTS synapses (
    id                  TEXT PRIMARY KEY,
    peer                BYTEA NOT NULL,
    weight              REAL NOT NULL CHECK (weight >= 0 AND weight <= 1),
    attenuation_factor  REAL NOT NULL CHECK (attenuation_factor >= 0 AND attenuation_factor <= 1),
    state               TEXT NOT NULL CHECK (state IN ('forming','active','weakening','dormant','pruned')),
    type_affinity       JSONB NOT NULL DEFAULT '{}'::jsonb,
    established_at_ns   BIGINT NOT NULL,
    last_active_ns      BIGINT NOT NULL,
    signals_transmitted BIGINT NOT NULL DEFAULT 0,
    signals_received    BIGINT NOT NULL DEFAULT 0,
    avg_latency_ns      BIGINT NOT NULL DEFAULT 0,
    error_rate          REAL NOT NULL DEFAULT 0 CHECK (error_rate >= 0 AND error_rate <= 1),
    -- threat-model §4: a synapse accumulating
    -- `signature_failure_prune_threshold` failures within one influence
    -- window must be pruned. A count over a window needs both the count and
    -- the window's start persisted, or a restart is a free amnesty for an
    -- attacker part-way to the threshold.
    signature_failures      INTEGER NOT NULL DEFAULT 0 CHECK (signature_failures >= 0),
    failure_window_start_ns BIGINT NOT NULL DEFAULT 0
);

-- Routing lists synapses by weight descending on every decision.
CREATE INDEX IF NOT EXISTS idx_synapses_weight ON synapses (weight DESC, id ASC);
-- One synapse per peer, which also makes lookup-by-peer an index hit.
CREATE UNIQUE INDEX IF NOT EXISTS idx_synapses_peer ON synapses (peer);
CREATE INDEX IF NOT EXISTS idx_synapses_state ON synapses (state);
CREATE INDEX IF NOT EXISTS idx_synapses_last_active ON synapses (last_active_ns);
-- Affinity questions are answered server-side.
CREATE INDEX IF NOT EXISTS idx_synapses_affinity ON synapses USING gin (type_affinity);

-- Topology knowledge. `source` is load-bearing, not informational:
-- discovery-learned peers must not evict configured ones. See
-- https://openntl.org/spec/threat-model section 3.
CREATE TABLE IF NOT EXISTS peers (
    id               BYTEA PRIMARY KEY,
    addresses        JSONB NOT NULL DEFAULT '[]'::jsonb,
    region           TEXT,
    advertised_types JSONB NOT NULL DEFAULT '[]'::jsonb,
    last_seen_ns     BIGINT NOT NULL,
    source           TEXT NOT NULL CHECK (source IN ('configured','bootstrap','discovered','observed'))
);

CREATE INDEX IF NOT EXISTS idx_peers_region ON peers (region);
CREATE INDEX IF NOT EXISTS idx_peers_last_seen ON peers (last_seen_ns DESC);
CREATE INDEX IF NOT EXISTS idx_peers_source ON peers (source);

-- Deduplication cache. The primary key is what makes check-and-set atomic:
-- INSERT ... ON CONFLICT decides presence and inserts in one statement. A
-- SELECT followed by an INSERT would leave a race, and that race is duplicate
-- propagation -- the exact failure Propagation Rule 4 exists to prevent.
CREATE TABLE IF NOT EXISTS seen_signals (
    id         BYTEA PRIMARY KEY,
    expires_ns BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_seen_expires ON seen_signals (expires_ns);

-- Activation snapshot. A single row: snapshots replace, never accumulate.
CREATE TABLE IF NOT EXISTS activation (
    singleton           BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    potential           REAL NOT NULL,
    threshold           REAL NOT NULL,
    refractory_until_ns BIGINT NOT NULL,
    signals_fired       BIGINT NOT NULL,
    taken_at_ns         BIGINT NOT NULL
);

-- The learning journal: one routing decision and its observed outcome. This is
-- the training data.
CREATE TABLE IF NOT EXISTS journal (
    id             BIGSERIAL PRIMARY KEY,
    signal         BYTEA NOT NULL,
    signal_type    TEXT NOT NULL,
    synapse        TEXT NOT NULL,
    peer           BYTEA NOT NULL,
    score          REAL NOT NULL,
    signal_weight  REAL NOT NULL,
    explored       BOOLEAN NOT NULL DEFAULT FALSE,
    decided_at_ns  BIGINT NOT NULL,
    outcome        TEXT NOT NULL CHECK (outcome IN (
                       'pending','delivered','rejected','timed_out',
                       'transport_failure','signature_failure')),
    resolved_at_ns BIGINT
);

-- Routing a receipt back to its decision.
CREATE INDEX IF NOT EXISTS idx_journal_signal_peer ON journal (signal, peer);
-- The timeout sweep scans unresolved decisions by age. Partial index, because
-- resolved rows are the overwhelming majority and never match.
CREATE INDEX IF NOT EXISTS idx_journal_pending ON journal (decided_at_ns)
    WHERE outcome = 'pending';
CREATE INDEX IF NOT EXISTS idx_journal_decided ON journal (decided_at_ns DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_journal_type ON journal (signal_type, decided_at_ns DESC);

-- Influence accounting, persisted so a forced restart cannot reset an
-- attacker's budget. See https://openntl.org/spec/threat-model section 1.
CREATE TABLE IF NOT EXISTS influence (
    id        BIGSERIAL PRIMARY KEY,
    peer      BYTEA NOT NULL,
    magnitude REAL NOT NULL CHECK (magnitude >= 0),
    at_ns     BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_influence_peer_at ON influence (peer, at_ns);

-- Node identity and other single-valued state.
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value BYTEA NOT NULL
);

-- Optional signal bodies for replay. Off unless explicitly enabled: payloads
-- are the most privacy-sensitive data a node handles.
CREATE TABLE IF NOT EXISTS signal_history (
    id       BYTEA PRIMARY KEY,
    body     BYTEA NOT NULL,
    added_ns BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_signal_history_added ON signal_history (added_ns);

INSERT INTO schema_version (singleton, version)
VALUES (TRUE, 1)
ON CONFLICT (singleton) DO UPDATE SET version = 1, applied_at = now();

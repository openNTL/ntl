//! SQLite storage backend for NTL — the zero-config default.
//!
//! One file, no server, no configuration. This is the edge story rather than
//! a compromise on it: WAL mode lets the propagation path read synapse
//! weights while the learning loop writes them, and the whole database is a
//! single file you can copy.
//!
//! Implements [`NodeStore`] and is verified against
//! [`ntl_core::store::conformance`], the same suite every other backend runs.
//!
//! ```no_run
//! use ntl_store_sqlite::SqliteStore;
//! use ntl_core::NodeStore;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let store = SqliteStore::open("~/.ntl/node.db")?;
//! store.migrate()?;
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

mod schema;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ntl_core::signal::{NodeId, SignalId, SignalType};
use ntl_core::store::{
    ActivationSnapshot, Durability, JournalEntry, JournalId, NodeStore, Outcome, PeerRecord,
    PeerSource, StoreError, StoreResult, SynapseFilter, SynapseRecord,
};
use ntl_core::synapse::{SynapseId, SynapseState};
use rusqlite::{params, Connection, OptionalExtension};

pub use schema::CURRENT_VERSION;

/// How aggressively SQLite flushes to disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Synchronous {
    /// `synchronous = FULL`. Survives power loss; heaviest write load.
    Full,
    /// `synchronous = NORMAL`. The default: a narrow power-loss window in
    /// exchange for materially fewer write cycles, which matters on the
    /// flash storage edge devices use.
    #[default]
    Normal,
    /// `synchronous = OFF`. Fast and unsafe; tests only.
    Off,
}

impl Synchronous {
    fn as_pragma(self) -> &'static str {
        match self {
            Self::Full => "FULL",
            Self::Normal => "NORMAL",
            Self::Off => "OFF",
        }
    }
}

/// Configuration for [`SqliteStore`].
#[derive(Debug, Clone)]
pub struct SqliteConfig {
    /// Where the database lives.
    pub path: PathBuf,
    /// Flush behaviour.
    pub synchronous: Synchronous,
    /// How long to wait on a locked database, in milliseconds.
    pub busy_timeout_ms: u32,
    /// Whether to retain signal bodies for replay.
    ///
    /// Off by default: payloads are the most privacy-sensitive data a node
    /// handles, and edge nodes have no room for them.
    pub retain_signal_history: bool,
}

impl SqliteConfig {
    /// Configuration for a database at `path`, with defaults elsewhere.
    #[must_use]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            synchronous: Synchronous::Normal,
            busy_timeout_ms: 5_000,
            retain_signal_history: false,
        }
    }
}

/// SQLite-backed [`NodeStore`].
///
/// The connection is held behind a mutex, so the store satisfies the trait's
/// requirement to serialise internally rather than pushing locking onto
/// callers.
pub struct SqliteStore {
    conn: Mutex<Connection>,
    config: SqliteConfig,
    in_memory: bool,
}

impl SqliteStore {
    /// Open (or create) a database at `path`.
    ///
    /// A leading `~/` is expanded, and parent directories are created — `ntl
    /// init` should not fail because `~/.ntl` does not exist yet.
    ///
    /// # Errors
    /// Returns [`StoreError::Unavailable`] if the database cannot be opened.
    pub fn open(path: impl AsRef<str>) -> StoreResult<Self> {
        Self::with_config(SqliteConfig::at(expand_home(path.as_ref())))
    }

    /// Open with explicit configuration.
    ///
    /// # Errors
    /// Returns [`StoreError::Unavailable`] if the database cannot be opened
    /// or its pragmas cannot be applied.
    pub fn with_config(config: SqliteConfig) -> StoreResult<Self> {
        if let Some(parent) = config.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    StoreError::Unavailable(format!(
                        "cannot create {}: {e}",
                        parent.display()
                    ))
                })?;
            }
        }

        let conn = Connection::open(&config.path).map_err(|e| {
            StoreError::Unavailable(format!("cannot open {}: {e}", config.path.display()))
        })?;

        let store = Self {
            conn: Mutex::new(conn),
            config,
            in_memory: false,
        };
        store.apply_pragmas()?;
        Ok(store)
    }

    /// Open a private in-memory database, for tests.
    ///
    /// # Errors
    /// Returns [`StoreError::Unavailable`] if the database cannot be created.
    pub fn in_memory() -> StoreResult<Self> {
        let conn = Connection::open_in_memory()
            .map_err(|e| StoreError::Unavailable(format!("cannot open in-memory db: {e}")))?;
        let store = Self {
            conn: Mutex::new(conn),
            config: SqliteConfig {
                // OFF is safe here: there is no disk to lose.
                synchronous: Synchronous::Off,
                ..SqliteConfig::at(":memory:")
            },
            in_memory: true,
        };
        store.apply_pragmas()?;
        Ok(store)
    }

    /// As [`Self::in_memory`], retaining signal bodies.
    ///
    /// # Errors
    /// Returns [`StoreError::Unavailable`] if the database cannot be created.
    pub fn in_memory_with_history() -> StoreResult<Self> {
        let mut store = Self::in_memory()?;
        store.config.retain_signal_history = true;
        Ok(store)
    }

    /// The path this store is backed by.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.config.path
    }

    /// The schema version currently applied.
    ///
    /// # Errors
    /// Returns [`StoreError::Unavailable`] if the database cannot be read.
    pub fn schema_version(&self) -> StoreResult<u32> {
        let conn = self.lock()?;
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|e| StoreError::Unavailable(e.to_string()))?;
        Ok(version)
    }

    fn apply_pragmas(&self) -> StoreResult<()> {
        let conn = self.lock()?;
        // WAL keeps readers from blocking on the learning loop's writes. It is
        // unavailable for in-memory databases, which have no journal file.
        if !self.in_memory {
            conn.pragma_update(None, "journal_mode", "WAL")
                .map_err(|e| StoreError::Unavailable(format!("cannot enable WAL: {e}")))?;
        }
        conn.pragma_update(None, "synchronous", self.config.synchronous.as_pragma())
            .map_err(|e| StoreError::Unavailable(e.to_string()))?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| StoreError::Unavailable(e.to_string()))?;
        conn.busy_timeout(std::time::Duration::from_millis(u64::from(
            self.config.busy_timeout_ms,
        )))
        .map_err(|e| StoreError::Unavailable(e.to_string()))?;
        Ok(())
    }

    fn lock(&self) -> StoreResult<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| StoreError::Unavailable("connection lock poisoned".to_string()))
    }
}

// ---------------------------------------------------------------------------
// Encoding helpers
// ---------------------------------------------------------------------------

fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

fn write_err(e: rusqlite::Error) -> StoreError {
    StoreError::Write(e.to_string())
}

fn read_err(e: rusqlite::Error) -> StoreError {
    StoreError::Unavailable(e.to_string())
}

fn corrupt(table: &str, reason: impl std::fmt::Display) -> StoreError {
    StoreError::Corrupt {
        table: table.to_string(),
        reason: reason.to_string(),
    }
}

/// SQLite integers are signed; timestamps are `u64`. Saturating rather than
/// wrapping keeps a far-future timestamp ordering as far-future.
#[allow(clippy::cast_possible_wrap)]
fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

#[allow(clippy::cast_sign_loss)]
fn from_i64(v: i64) -> u64 {
    if v < 0 { 0 } else { v as u64 }
}

fn state_to_str(state: SynapseState) -> &'static str {
    match state {
        SynapseState::Forming => "forming",
        SynapseState::Active => "active",
        SynapseState::Weakening => "weakening",
        SynapseState::Dormant => "dormant",
        SynapseState::Pruned => "pruned",
    }
}

fn state_from_str(s: &str) -> StoreResult<SynapseState> {
    match s {
        "forming" => Ok(SynapseState::Forming),
        "active" => Ok(SynapseState::Active),
        "weakening" => Ok(SynapseState::Weakening),
        "dormant" => Ok(SynapseState::Dormant),
        "pruned" => Ok(SynapseState::Pruned),
        other => Err(corrupt("synapses", format!("unknown state {other:?}"))),
    }
}

fn source_to_str(source: PeerSource) -> &'static str {
    match source {
        PeerSource::Configured => "configured",
        PeerSource::Bootstrap => "bootstrap",
        PeerSource::Discovered => "discovered",
        PeerSource::Observed => "observed",
    }
}

fn source_from_str(s: &str) -> StoreResult<PeerSource> {
    match s {
        "configured" => Ok(PeerSource::Configured),
        "bootstrap" => Ok(PeerSource::Bootstrap),
        "discovered" => Ok(PeerSource::Discovered),
        "observed" => Ok(PeerSource::Observed),
        other => Err(corrupt("peers", format!("unknown source {other:?}"))),
    }
}

fn outcome_to_str(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Pending => "pending",
        Outcome::Delivered => "delivered",
        Outcome::Rejected => "rejected",
        Outcome::TimedOut => "timed_out",
        Outcome::TransportFailure => "transport_failure",
        Outcome::SignatureFailure => "signature_failure",
    }
}

fn outcome_from_str(s: &str) -> StoreResult<Outcome> {
    match s {
        "pending" => Ok(Outcome::Pending),
        "delivered" => Ok(Outcome::Delivered),
        "rejected" => Ok(Outcome::Rejected),
        "timed_out" => Ok(Outcome::TimedOut),
        "transport_failure" => Ok(Outcome::TransportFailure),
        "signature_failure" => Ok(Outcome::SignatureFailure),
        other => Err(corrupt("journal", format!("unknown outcome {other:?}"))),
    }
}

/// Signal types round-trip by name so `Custom` variants survive.
fn signal_type_to_str(t: &SignalType) -> String {
    match t {
        SignalType::Custom(name) => format!("custom:{name}"),
        other => format!("{other:?}"),
    }
}

fn signal_type_from_str(s: &str) -> StoreResult<SignalType> {
    if let Some(name) = s.strip_prefix("custom:") {
        return Ok(SignalType::Custom(name.to_string()));
    }
    match s {
        "Data" => Ok(SignalType::Data),
        "Query" => Ok(SignalType::Query),
        "Event" => Ok(SignalType::Event),
        "Command" => Ok(SignalType::Command),
        "Heartbeat" => Ok(SignalType::Heartbeat),
        "Discovery" => Ok(SignalType::Discovery),
        "Receipt" => Ok(SignalType::Receipt),
        other => Err(corrupt("journal", format!("unknown signal type {other:?}"))),
    }
}

fn json_map(raw: &str) -> StoreResult<HashMap<String, u64>> {
    serde_json::from_str(raw).map_err(|e| corrupt("synapses", format!("type_affinity: {e}")))
}

fn json_vec(raw: &str, table: &str) -> StoreResult<Vec<String>> {
    serde_json::from_str(raw).map_err(|e| corrupt(table, e))
}

fn signal_id_from_blob(blob: &[u8]) -> StoreResult<SignalId> {
    let bytes: [u8; 16] = blob
        .try_into()
        .map_err(|_| corrupt("signal id", format!("expected 16 bytes, got {}", blob.len())))?;
    Ok(SignalId::from_bytes(bytes))
}

fn row_to_synapse(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoreResult<SynapseRecord>> {
    let id: String = row.get(0)?;
    let peer: Vec<u8> = row.get(1)?;
    let weight: f64 = row.get(2)?;
    let attenuation: f64 = row.get(3)?;
    let state: String = row.get(4)?;
    let affinity: String = row.get(5)?;
    let established: i64 = row.get(6)?;
    let last_active: i64 = row.get(7)?;
    let transmitted: i64 = row.get(8)?;
    let received: i64 = row.get(9)?;
    let latency: i64 = row.get(10)?;
    let error_rate: f64 = row.get(11)?;

    Ok((|| {
        Ok(SynapseRecord {
            id: SynapseId(id),
            peer: NodeId(peer),
            #[allow(clippy::cast_possible_truncation)]
            weight: weight as f32,
            #[allow(clippy::cast_possible_truncation)]
            attenuation_factor: attenuation as f32,
            state: state_from_str(&state)?,
            type_affinity: json_map(&affinity)?,
            established_at_ns: from_i64(established),
            last_active_ns: from_i64(last_active),
            signals_transmitted: from_i64(transmitted),
            signals_received: from_i64(received),
            avg_latency_ns: from_i64(latency),
            #[allow(clippy::cast_possible_truncation)]
            error_rate: error_rate as f32,
        })
    })())
}

fn row_to_journal(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoreResult<JournalEntry>> {
    let id: i64 = row.get(0)?;
    let signal: Vec<u8> = row.get(1)?;
    let signal_type: String = row.get(2)?;
    let synapse: String = row.get(3)?;
    let peer: Vec<u8> = row.get(4)?;
    let score: f64 = row.get(5)?;
    let explored: i64 = row.get(6)?;
    let decided: i64 = row.get(7)?;
    let outcome: String = row.get(8)?;
    let resolved: Option<i64> = row.get(9)?;

    Ok((|| {
        Ok(JournalEntry {
            id: Some(JournalId(from_i64(id))),
            signal: signal_id_from_blob(&signal)?,
            signal_type: signal_type_from_str(&signal_type)?,
            synapse: SynapseId(synapse),
            peer: NodeId(peer),
            #[allow(clippy::cast_possible_truncation)]
            score: score as f32,
            explored: explored != 0,
            decided_at_ns: from_i64(decided),
            outcome: outcome_from_str(&outcome)?,
            resolved_at_ns: resolved.map(from_i64),
        })
    })())
}

const SYNAPSE_COLUMNS: &str = "id, peer, weight, attenuation_factor, state, type_affinity, \
     established_at_ns, last_active_ns, signals_transmitted, signals_received, \
     avg_latency_ns, error_rate";

const JOURNAL_COLUMNS: &str = "id, signal, signal_type, synapse, peer, score, explored, \
     decided_at_ns, outcome, resolved_at_ns";

// ---------------------------------------------------------------------------
// NodeStore
// ---------------------------------------------------------------------------

impl NodeStore for SqliteStore {
    fn migrate(&self) -> StoreResult<()> {
        let mut conn = self.lock()?;
        let current: u32 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|e| StoreError::Migration {
                version: 0,
                reason: e.to_string(),
            })?;

        // Refuse a database written by a newer build rather than guessing.
        if current > schema::CURRENT_VERSION {
            return Err(StoreError::Migration {
                version: current,
                reason: format!(
                    "database is at schema version {current}, but this build \
                     understands only {}. Upgrade NTL rather than \
                     downgrading the database.",
                    schema::CURRENT_VERSION
                ),
            });
        }

        for migration in schema::MIGRATIONS {
            if migration.version <= current {
                continue;
            }
            let tx = conn.transaction().map_err(|e| StoreError::Migration {
                version: migration.version,
                reason: e.to_string(),
            })?;
            tx.execute_batch(migration.sql)
                .map_err(|e| StoreError::Migration {
                    version: migration.version,
                    reason: format!("{}: {e}", migration.description),
                })?;
            tx.pragma_update(None, "user_version", migration.version)
                .map_err(|e| StoreError::Migration {
                    version: migration.version,
                    reason: e.to_string(),
                })?;
            tx.commit().map_err(|e| StoreError::Migration {
                version: migration.version,
                reason: e.to_string(),
            })?;
            tracing::info!(
                version = migration.version,
                description = migration.description,
                "applied schema migration"
            );
        }
        Ok(())
    }

    fn durability(&self) -> Durability {
        if self.in_memory {
            return Durability::Memory;
        }
        match self.config.synchronous {
            // FULL survives power loss. NORMAL and OFF leave a window, and
            // reporting otherwise would mislead an operator sizing their
            // deployment.
            Synchronous::Full => Durability::Durable,
            Synchronous::Normal | Synchronous::Off => Durability::BestEffort,
        }
    }

    fn flush(&self) -> StoreResult<()> {
        let conn = self.lock()?;
        if self.in_memory {
            return Ok(());
        }
        conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")
            .map_err(|e| StoreError::Write(e.to_string()))
    }

    // -- synapses ----------------------------------------------------------

    fn put_synapse(&self, record: &SynapseRecord) -> StoreResult<()> {
        let affinity = serde_json::to_string(&record.type_affinity)
            .map_err(|e| StoreError::Write(e.to_string()))?;
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO synapses (id, peer, weight, attenuation_factor, state, \
                 type_affinity, established_at_ns, last_active_ns, \
                 signals_transmitted, signals_received, avg_latency_ns, error_rate) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12) \
             ON CONFLICT(id) DO UPDATE SET \
                 peer=excluded.peer, weight=excluded.weight, \
                 attenuation_factor=excluded.attenuation_factor, \
                 state=excluded.state, type_affinity=excluded.type_affinity, \
                 established_at_ns=excluded.established_at_ns, \
                 last_active_ns=excluded.last_active_ns, \
                 signals_transmitted=excluded.signals_transmitted, \
                 signals_received=excluded.signals_received, \
                 avg_latency_ns=excluded.avg_latency_ns, \
                 error_rate=excluded.error_rate",
            params![
                record.id.0,
                record.peer.0,
                f64::from(record.weight),
                f64::from(record.attenuation_factor),
                state_to_str(record.state),
                affinity,
                to_i64(record.established_at_ns),
                to_i64(record.last_active_ns),
                to_i64(record.signals_transmitted),
                to_i64(record.signals_received),
                to_i64(record.avg_latency_ns),
                f64::from(record.error_rate),
            ],
        )
        .map_err(write_err)?;
        Ok(())
    }

    fn get_synapse(&self, id: &SynapseId) -> StoreResult<Option<SynapseRecord>> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                &format!("SELECT {SYNAPSE_COLUMNS} FROM synapses WHERE id = ?1"),
                params![id.0],
                row_to_synapse,
            )
            .optional()
            .map_err(read_err)?;
        row.transpose()
    }

    fn synapse_for_peer(&self, peer: &NodeId) -> StoreResult<Option<SynapseRecord>> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                &format!("SELECT {SYNAPSE_COLUMNS} FROM synapses WHERE peer = ?1"),
                params![peer.0],
                row_to_synapse,
            )
            .optional()
            .map_err(read_err)?;
        row.transpose()
    }

    fn list_synapses(&self, filter: &SynapseFilter) -> StoreResult<Vec<SynapseRecord>> {
        let mut sql = format!("SELECT {SYNAPSE_COLUMNS} FROM synapses WHERE 1=1");
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if !filter.states.is_empty() {
            let placeholders = filter
                .states
                .iter()
                .map(|_| "?")
                .collect::<Vec<_>>()
                .join(",");
            sql.push_str(&format!(" AND state IN ({placeholders})"));
            for state in &filter.states {
                args.push(Box::new(state_to_str(*state)));
            }
        }
        if let Some(min) = filter.min_weight {
            sql.push_str(" AND weight >= ?");
            args.push(Box::new(f64::from(min)));
        }
        if let Some(before) = filter.last_active_before_ns {
            sql.push_str(" AND last_active_ns < ?");
            args.push(Box::new(to_i64(before)));
        }
        // Ties break on id so the order is total and routing is reproducible.
        sql.push_str(" ORDER BY weight DESC, id ASC");
        if let Some(limit) = filter.limit {
            sql.push_str(" LIMIT ?");
            args.push(Box::new(to_i64(limit as u64)));
        }

        let conn = self.lock()?;
        let mut stmt = conn.prepare(&sql).map_err(read_err)?;
        let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(std::convert::AsRef::as_ref).collect();
        let rows = stmt
            .query_map(refs.as_slice(), row_to_synapse)
            .map_err(read_err)?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(read_err)??);
        }
        Ok(out)
    }

    fn delete_synapse(&self, id: &SynapseId) -> StoreResult<()> {
        let conn = self.lock()?;
        conn.execute("DELETE FROM synapses WHERE id = ?1", params![id.0])
            .map_err(write_err)?;
        Ok(())
    }

    // -- topology ----------------------------------------------------------

    fn put_peer(&self, record: &PeerRecord) -> StoreResult<()> {
        let addresses = serde_json::to_string(&record.addresses)
            .map_err(|e| StoreError::Write(e.to_string()))?;
        let types = serde_json::to_string(&record.advertised_types)
            .map_err(|e| StoreError::Write(e.to_string()))?;
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO peers (id, addresses, region, advertised_types, last_seen_ns, source) \
             VALUES (?1,?2,?3,?4,?5,?6) \
             ON CONFLICT(id) DO UPDATE SET \
                 addresses=excluded.addresses, region=excluded.region, \
                 advertised_types=excluded.advertised_types, \
                 last_seen_ns=excluded.last_seen_ns, source=excluded.source",
            params![
                record.id.0,
                addresses,
                record.region,
                types,
                to_i64(record.last_seen_ns),
                source_to_str(record.source),
            ],
        )
        .map_err(write_err)?;
        Ok(())
    }

    fn get_peer(&self, id: &NodeId) -> StoreResult<Option<PeerRecord>> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT id, addresses, region, advertised_types, last_seen_ns, source \
                 FROM peers WHERE id = ?1",
                params![id.0],
                row_to_peer,
            )
            .optional()
            .map_err(read_err)?;
        row.transpose()
    }

    fn list_peers(&self, region: Option<&str>, limit: usize) -> StoreResult<Vec<PeerRecord>> {
        let conn = self.lock()?;
        let sql = if region.is_some() {
            "SELECT id, addresses, region, advertised_types, last_seen_ns, source \
             FROM peers WHERE region = ?1 ORDER BY last_seen_ns DESC, id ASC LIMIT ?2"
        } else {
            "SELECT id, addresses, region, advertised_types, last_seen_ns, source \
             FROM peers ORDER BY last_seen_ns DESC, id ASC LIMIT ?1"
        };
        let mut stmt = conn.prepare(sql).map_err(read_err)?;
        let limit = to_i64(limit as u64);
        let rows = match region {
            Some(r) => stmt.query_map(params![r, limit], row_to_peer),
            None => stmt.query_map(params![limit], row_to_peer),
        }
        .map_err(read_err)?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(read_err)??);
        }
        Ok(out)
    }

    fn count_peers(&self, source: PeerSource) -> StoreResult<u64> {
        let conn = self.lock()?;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM peers WHERE source = ?1",
                params![source_to_str(source)],
                |row| row.get(0),
            )
            .map_err(read_err)?;
        Ok(from_i64(count))
    }

    // -- deduplication -----------------------------------------------------

    fn check_and_set_seen(&self, id: &SignalId, now_ns: u64, ttl_secs: u64) -> StoreResult<bool> {
        let expires = now_ns.saturating_add(ttl_secs.saturating_mul(1_000_000_000));
        let conn = self.lock()?;

        // One statement decides presence and inserts, which is what makes this
        // atomic. A SELECT followed by an INSERT would leave a race, and that
        // race is duplicate propagation — the exact failure Rule 4 exists to
        // prevent.
        //
        // The WHERE clause on the upsert means an *expired* row is treated as
        // absent: it is refreshed and reports unseen.
        let changed = conn
            .execute(
                "INSERT INTO seen_signals (id, expires_ns) VALUES (?1, ?2) \
                 ON CONFLICT(id) DO UPDATE SET expires_ns = excluded.expires_ns \
                 WHERE seen_signals.expires_ns <= ?3",
                params![
                    id.to_bytes().to_vec(),
                    to_i64(expires),
                    to_i64(now_ns)
                ],
            )
            .map_err(write_err)?;

        // A row was inserted or refreshed => it was absent or expired.
        Ok(changed == 0)
    }

    fn has_seen(&self, id: &SignalId, now_ns: u64) -> StoreResult<bool> {
        let conn = self.lock()?;
        let found: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM seen_signals WHERE id = ?1 AND expires_ns > ?2",
                params![id.to_bytes().to_vec(), to_i64(now_ns)],
                |row| row.get(0),
            )
            .optional()
            .map_err(read_err)?;
        Ok(found.is_some())
    }

    fn purge_expired_seen(&self, now_ns: u64) -> StoreResult<u64> {
        let conn = self.lock()?;
        let removed = conn
            .execute(
                "DELETE FROM seen_signals WHERE expires_ns <= ?1",
                params![to_i64(now_ns)],
            )
            .map_err(write_err)?;
        Ok(removed as u64)
    }

    // -- activation --------------------------------------------------------

    fn save_activation(&self, snapshot: &ActivationSnapshot) -> StoreResult<()> {
        let conn = self.lock()?;
        // A fixed primary key means snapshots replace rather than accumulate.
        conn.execute(
            "INSERT INTO activation \
                 (singleton, potential, threshold, refractory_until_ns, signals_fired, taken_at_ns) \
             VALUES (0,?1,?2,?3,?4,?5) \
             ON CONFLICT(singleton) DO UPDATE SET \
                 potential=excluded.potential, threshold=excluded.threshold, \
                 refractory_until_ns=excluded.refractory_until_ns, \
                 signals_fired=excluded.signals_fired, taken_at_ns=excluded.taken_at_ns",
            params![
                f64::from(snapshot.potential),
                f64::from(snapshot.threshold),
                to_i64(snapshot.refractory_until_ns),
                to_i64(snapshot.signals_fired),
                to_i64(snapshot.taken_at_ns),
            ],
        )
        .map_err(write_err)?;
        Ok(())
    }

    fn load_activation(&self) -> StoreResult<Option<ActivationSnapshot>> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT potential, threshold, refractory_until_ns, signals_fired, taken_at_ns \
             FROM activation WHERE singleton = 0",
            [],
            |row| {
                let potential: f64 = row.get(0)?;
                let threshold: f64 = row.get(1)?;
                let refractory: i64 = row.get(2)?;
                let fired: i64 = row.get(3)?;
                let taken: i64 = row.get(4)?;
                Ok(ActivationSnapshot {
                    #[allow(clippy::cast_possible_truncation)]
                    potential: potential as f32,
                    #[allow(clippy::cast_possible_truncation)]
                    threshold: threshold as f32,
                    refractory_until_ns: from_i64(refractory),
                    signals_fired: from_i64(fired),
                    taken_at_ns: from_i64(taken),
                })
            },
        )
        .optional()
        .map_err(read_err)
    }

    // -- learning journal --------------------------------------------------

    fn append_decision(&self, entry: &JournalEntry) -> StoreResult<JournalId> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO journal \
                 (signal, signal_type, synapse, peer, score, explored, decided_at_ns, \
                  outcome, resolved_at_ns) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                entry.signal.to_bytes().to_vec(),
                signal_type_to_str(&entry.signal_type),
                entry.synapse.0,
                entry.peer.0,
                f64::from(entry.score),
                i64::from(entry.explored),
                to_i64(entry.decided_at_ns),
                outcome_to_str(entry.outcome),
                entry.resolved_at_ns.map(to_i64),
            ],
        )
        .map_err(write_err)?;
        Ok(JournalId(from_i64(conn.last_insert_rowid())))
    }

    fn resolve_decision(
        &self,
        id: JournalId,
        outcome: Outcome,
        now_ns: u64,
    ) -> StoreResult<Option<JournalEntry>> {
        let conn = self.lock()?;

        // The WHERE clause is what makes resolution idempotent: only a still-
        // pending row is updated, so the first receipt wins and a replay is a
        // no-op. At-least-once delivery makes duplicate receipts routine, not
        // exceptional.
        conn.execute(
            "UPDATE journal SET outcome = ?1, resolved_at_ns = ?2 \
             WHERE id = ?3 AND outcome = 'pending'",
            params![outcome_to_str(outcome), to_i64(now_ns), to_i64(id.0)],
        )
        .map_err(write_err)?;

        let row = conn
            .query_row(
                &format!("SELECT {JOURNAL_COLUMNS} FROM journal WHERE id = ?1"),
                params![to_i64(id.0)],
                row_to_journal,
            )
            .optional()
            .map_err(read_err)?;
        row.transpose()
    }

    fn pending_decision_for(
        &self,
        signal: &SignalId,
        peer: &NodeId,
    ) -> StoreResult<Option<JournalEntry>> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                &format!(
                    "SELECT {JOURNAL_COLUMNS} FROM journal \
                     WHERE signal = ?1 AND peer = ?2 AND outcome = 'pending' \
                     ORDER BY decided_at_ns ASC LIMIT 1"
                ),
                params![signal.to_bytes().to_vec(), peer.0],
                row_to_journal,
            )
            .optional()
            .map_err(read_err)?;
        row.transpose()
    }

    fn expired_decisions(&self, deadline_ns: u64, limit: usize) -> StoreResult<Vec<JournalEntry>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {JOURNAL_COLUMNS} FROM journal \
                 WHERE outcome = 'pending' AND decided_at_ns < ?1 \
                 ORDER BY decided_at_ns ASC LIMIT ?2"
            ))
            .map_err(read_err)?;
        let rows = stmt
            .query_map(
                params![to_i64(deadline_ns), to_i64(limit as u64)],
                row_to_journal,
            )
            .map_err(read_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(read_err)??);
        }
        Ok(out)
    }

    fn recent_decisions(
        &self,
        signal_type: Option<&SignalType>,
        limit: usize,
    ) -> StoreResult<Vec<JournalEntry>> {
        let conn = self.lock()?;
        let limit = to_i64(limit as u64);
        let mut out = Vec::new();
        match signal_type {
            Some(t) => {
                let mut stmt = conn
                    .prepare(&format!(
                        "SELECT {JOURNAL_COLUMNS} FROM journal WHERE signal_type = ?1 \
                         ORDER BY decided_at_ns DESC, id DESC LIMIT ?2"
                    ))
                    .map_err(read_err)?;
                let rows = stmt
                    .query_map(params![signal_type_to_str(t), limit], row_to_journal)
                    .map_err(read_err)?;
                for row in rows {
                    out.push(row.map_err(read_err)??);
                }
            }
            None => {
                let mut stmt = conn
                    .prepare(&format!(
                        "SELECT {JOURNAL_COLUMNS} FROM journal \
                         ORDER BY decided_at_ns DESC, id DESC LIMIT ?1"
                    ))
                    .map_err(read_err)?;
                let rows = stmt
                    .query_map(params![limit], row_to_journal)
                    .map_err(read_err)?;
                for row in rows {
                    out.push(row.map_err(read_err)??);
                }
            }
        }
        Ok(out)
    }

    fn trim_journal(&self, older_than_ns: u64) -> StoreResult<u64> {
        let conn = self.lock()?;
        let removed = conn
            .execute(
                "DELETE FROM journal WHERE decided_at_ns < ?1",
                params![to_i64(older_than_ns)],
            )
            .map_err(write_err)?;
        Ok(removed as u64)
    }

    // -- influence accounting ---------------------------------------------

    fn record_influence(
        &self,
        peer: &NodeId,
        magnitude: f32,
        now_ns: u64,
        window_start_ns: u64,
    ) -> StoreResult<f32> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO influence (peer, magnitude, at_ns) VALUES (?1,?2,?3)",
            params![peer.0, f64::from(magnitude.abs()), to_i64(now_ns)],
        )
        .map_err(write_err)?;

        let total: Option<f64> = conn
            .query_row(
                "SELECT SUM(magnitude) FROM influence WHERE peer = ?1 AND at_ns >= ?2",
                params![peer.0, to_i64(window_start_ns)],
                |row| row.get(0),
            )
            .map_err(read_err)?;
        #[allow(clippy::cast_possible_truncation)]
        let out = total.unwrap_or(0.0) as f32;
        Ok(out)
    }

    fn influence_since(&self, peer: &NodeId, window_start_ns: u64) -> StoreResult<f32> {
        let conn = self.lock()?;
        let total: Option<f64> = conn
            .query_row(
                "SELECT SUM(magnitude) FROM influence WHERE peer = ?1 AND at_ns >= ?2",
                params![peer.0, to_i64(window_start_ns)],
                |row| row.get(0),
            )
            .map_err(read_err)?;
        #[allow(clippy::cast_possible_truncation)]
        let out = total.unwrap_or(0.0) as f32;
        Ok(out)
    }

    fn purge_influence(&self, older_than_ns: u64) -> StoreResult<u64> {
        let conn = self.lock()?;
        let removed = conn
            .execute(
                "DELETE FROM influence WHERE at_ns < ?1",
                params![to_i64(older_than_ns)],
            )
            .map_err(write_err)?;
        Ok(removed as u64)
    }

    // -- node metadata -----------------------------------------------------

    fn put_meta(&self, key: &str, value: &[u8]) -> StoreResult<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1,?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )
        .map_err(write_err)?;
        Ok(())
    }

    fn get_meta(&self, key: &str) -> StoreResult<Option<Vec<u8>>> {
        let conn = self.lock()?;
        conn.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |row| {
            row.get(0)
        })
        .optional()
        .map_err(read_err)
    }

    // -- optional: signal history -----------------------------------------

    fn signal_history_enabled(&self) -> bool {
        self.config.retain_signal_history
    }

    fn put_signal_history(&self, id: &SignalId, body: &[u8], now_ns: u64) -> StoreResult<()> {
        if !self.config.retain_signal_history {
            // Refuse rather than accept-and-discard: an operator who believes
            // replay is available will find out during an incident.
            return Err(StoreError::Unsupported("signal history"));
        }
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO signal_history (id, body, added_ns) VALUES (?1,?2,?3) \
             ON CONFLICT(id) DO UPDATE SET body = excluded.body, added_ns = excluded.added_ns",
            params![id.to_bytes().to_vec(), body, to_i64(now_ns)],
        )
        .map_err(write_err)?;
        Ok(())
    }

    fn get_signal_history(&self, id: &SignalId) -> StoreResult<Option<Vec<u8>>> {
        if !self.config.retain_signal_history {
            return Err(StoreError::Unsupported("signal history"));
        }
        let conn = self.lock()?;
        conn.query_row(
            "SELECT body FROM signal_history WHERE id = ?1",
            params![id.to_bytes().to_vec()],
            |row| row.get(0),
        )
        .optional()
        .map_err(read_err)
    }
}

fn row_to_peer(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoreResult<PeerRecord>> {
    let id: Vec<u8> = row.get(0)?;
    let addresses: String = row.get(1)?;
    let region: Option<String> = row.get(2)?;
    let types: String = row.get(3)?;
    let last_seen: i64 = row.get(4)?;
    let source: String = row.get(5)?;

    Ok((|| {
        Ok(PeerRecord {
            id: NodeId(id),
            addresses: json_vec(&addresses, "peers")?,
            region,
            advertised_types: json_vec(&types, "peers")?,
            last_seen_ns: from_i64(last_seen),
            source: source_from_str(&source)?,
        })
    })())
}

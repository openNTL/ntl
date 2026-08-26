//! PostgreSQL storage backend for NTL — **not yet implemented**.
//!
//! This crate exists to hold the design decisions and the contract, so that
//! whoever implements it does not have to rediscover them. It deliberately
//! does **not** provide a partial [`ntl_core::NodeStore`] implementation: a
//! backend that satisfies the trait's signature but not its guarantees is
//! worse than none, because a node would appear to be persisting and learning
//! when it was not.
//!
//! Use [`ntl-store-sqlite`](https://docs.rs/ntl-store-sqlite) today. It
//! serves both edge and full-node deployments.
//!
//! # Why Postgres at all
//!
//! For operators who already run PostgreSQL and want NTL state to live where
//! their backups, monitoring, and access control already reach. That is the
//! whole case; it is an operational convenience, not a performance argument.
//! SQLite is faster for a single node.
//!
//! # The one non-obvious design decision
//!
//! [`ntl_core::NodeStore`] is **synchronous**, and every Postgres driver worth
//! using is asynchronous.
//!
//! The resolution is that this crate owns the mismatch: it wraps its calls in
//! a blocking pool at its own boundary. `ntl-core` stays free of an async
//! runtime, which is what lets it build for `wasm32-unknown-unknown` and keeps
//! its time-dependent logic deterministically testable.
//!
//! ```ignore
//! // The shape, not working code.
//! impl NodeStore for PostgresStore {
//!     fn get_synapse(&self, id: &SynapseId) -> StoreResult<Option<SynapseRecord>> {
//!         // `handle` is a tokio runtime handle owned by this struct.
//!         self.handle.block_on(async { self.get_synapse_async(id).await })
//!     }
//! }
//! ```
//!
//! Two traps in that pattern:
//!
//! 1. `block_on` panics if called from inside a runtime worker thread. The
//!    store must own a dedicated runtime, or use
//!    `tokio::task::block_in_place`, rather than borrowing the caller's.
//! 2. Connection pool exhaustion becomes a deadlock rather than an error if
//!    the blocking pool and the connection pool are sized independently. Size
//!    the blocking pool no larger than the connection pool.
//!
//! # Requirements beyond the trait signature
//!
//! An implementation must satisfy every MUST in
//! [spec/storage-interface](https://openntl.org/spec/storage-interface). Three
//! are easy to get wrong in SQL and are worth stating here:
//!
//! - **Deduplication must be one atomic statement.** A `SELECT` followed by an
//!   `INSERT` leaves a race, and that race is duplicate propagation — the
//!   exact failure Propagation Rule 4 exists to prevent. Use
//!   `INSERT ... ON CONFLICT DO UPDATE ... WHERE expires_ns <= $now` and read
//!   the affected row count.
//! - **Outcome resolution must be idempotent.** `UPDATE journal SET ... WHERE
//!   id = $1 AND outcome = 'pending'`, so the first receipt wins. At-least-once
//!   delivery makes duplicate receipts routine.
//! - **`durability()` must be honest.** Report
//!   [`ntl_core::store::Durability::Durable`] only if `synchronous_commit` is
//!   on and the write has actually reached durable storage before the call
//!   returns. Operators size deployments on that promise.
//!
//! Run [`ntl_core::store::conformance::run_all`] against a real database in
//! CI. Passing it is necessary but not sufficient — the concurrency and
//! durability requirements are not fully mechanically checkable.
//!
//! # Schema
//!
//! Start from `ntl-store-sqlite`'s embedded schema; it is close to portable.
//! The differences that matter:
//!
//! | SQLite | PostgreSQL |
//! |---|---|
//! | `INTEGER` timestamps | `BIGINT`. Note NTL timestamps are `u64` and Postgres `BIGINT` is signed — saturate rather than wrap. |
//! | `BLOB` | `BYTEA` |
//! | `REAL` | `REAL` (`float4`) — matches the `f32` weights; do not widen to `double precision` and imply precision the model does not have. |
//! | `TEXT` holding JSON | `JSONB` for `type_affinity`, which makes affinity queryable server-side |
//! | `INTEGER PRIMARY KEY AUTOINCREMENT` | `BIGSERIAL` |
//! | `PRAGMA user_version` | a `schema_version` table |
//!
//! Namespace the tables in their own schema (`ntl` by default) rather than
//! `public`, so NTL state is separable from whatever else the database holds.

#![forbid(unsafe_code)]

/// Why this backend is unavailable, and what to use instead.
///
/// Returned by the runtime when a configuration selects `backend =
/// "postgres"`, so an operator gets a clear answer rather than a confusing
/// failure at first write.
pub const UNIMPLEMENTED: &str = "the PostgreSQL backend is not implemented yet; \
     use `backend = \"sqlite\"`. See https://openntl.org/guides/storage-backends";

/// Configuration a PostgreSQL backend will accept.
///
/// Defined now so the configuration surface is stable before the
/// implementation lands, and so `ntl-core`'s
/// [`ntl_core::config::StorageConfig`] has something to agree with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostgresConfig {
    /// Connection URL.
    pub url: String,
    /// Schema to place NTL tables in, keeping them out of `public`.
    pub schema: String,
    /// Connection pool size.
    ///
    /// The blocking pool must not exceed this, or pool exhaustion becomes a
    /// deadlock instead of an error.
    pub max_connections: u32,
}

impl PostgresConfig {
    /// Configuration for a connection URL, with defaults elsewhere.
    #[must_use]
    pub fn at(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            schema: "ntl".to_string(),
            max_connections: 16,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unavailability_message_names_the_alternative() {
        // An operator who picks this backend should learn what to do instead,
        // not just that something failed.
        assert!(UNIMPLEMENTED.contains("sqlite"));
        assert!(UNIMPLEMENTED.contains("openntl.org"));
    }

    #[test]
    fn default_schema_is_not_public() {
        assert_eq!(PostgresConfig::at("postgres://localhost/ntl").schema, "ntl");
    }
}

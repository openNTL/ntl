//! Transport and runtime driver for NTL.
//!
//! `ntl-core` holds no runtime assumptions — no executor, no sockets — so
//! that it stays testable and builds for `wasm32`. This crate supplies the
//! other half: a Tokio-based TCP transport, peer sessions with a signed
//! handshake, and the periodic maintenance the learning model depends on.
//!
//! The binaries (`ntl-cli`, `ntl-node`, `ntl-edge`) are thin wrappers over
//! [`Runtime`].

#![forbid(unsafe_code)]

pub mod bin_support;
pub mod frame;
pub mod identity;
pub mod runtime;

pub use identity::{Identity, IdentityError};
pub use runtime::{Event, Runtime, RuntimeConfig, RuntimeError};

use std::sync::Arc;

use ntl_core::NodeStore;

/// Open the storage backend named by a configuration.
///
/// # Errors
/// Returns an error describing why the backend could not be opened.
pub fn open_store(config: &ntl_core::config::StorageConfig) -> Result<Arc<dyn NodeStore>, String> {
    use ntl_core::config::StorageConfig;
    match config {
        StorageConfig::Sqlite {
            path,
            retain_signal_history,
        } => {
            let mut sqlite_config = ntl_store_sqlite::SqliteConfig::at(expand(path));
            sqlite_config.retain_signal_history = *retain_signal_history;
            let store = ntl_store_sqlite::SqliteStore::with_config(sqlite_config)
                .map_err(|e| e.to_string())?;
            Ok(Arc::new(store))
        }
        StorageConfig::Memory => Ok(Arc::new(ntl_core::store::MemoryStore::new())),
        StorageConfig::Postgres { .. } => Err(
            "the PostgreSQL backend is not implemented yet; use sqlite or memory. \
             See https://openntl.org/guides/storage-backends"
                .to_string(),
        ),
    }
}

/// Expand a leading `~/` against `$HOME`.
fn expand(path: &str) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return std::path::PathBuf::from(home).join(rest);
        }
    }
    std::path::PathBuf::from(path)
}

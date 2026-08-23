//! # Neural Transfer Layer (NTL)
//!
//! **The Neural Transfer Layer for Modern Compute**
//!
//! NTL is an open source data transfer layer that replaces the request-response
//! paradigm of traditional APIs with neural signal propagation.
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use ntl_core::{Node, Signal, SignalType};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), ntl_core::Error> {
//!     let node = Node::builder()
//!         .with_config_file("~/.ntl/config.toml")
//!         .build()
//!         .await?;
//!
//!     let signal = Signal::data("hello")
//!         .with_payload(serde_json::json!({"message": "world"}))
//!         .with_weight(0.5)
//!         .emit(&node)
//!         .await?;
//!
//!     println!("Signal emitted: {}", signal.id);
//!     Ok(())
//! }
//! ```

#![forbid(unsafe_code)]
#![warn(clippy::all, clippy::pedantic)]
#![warn(missing_docs)]

pub mod activation;
pub mod adapter;
pub mod config;
pub mod crypto;
pub mod error;
pub mod node;
pub mod propagation;
pub mod signal;
pub mod store;
pub mod synapse;
pub mod testing;
pub mod topology;

// Re-exports for convenience
pub use error::Error;
pub use node::{Node, NodeBuilder};
pub use signal::{Signal, SignalId, SignalType};
pub use synapse::{Synapse, SynapseId, SynapseState};
pub use propagation::PropagationScope;
pub use activation::ActivationFunction;
pub use adapter::{Adapter, AdapterHealth};
pub use store::{NodeStore, StoreError};

/// Result type alias for NTL operations.
pub type Result<T> = std::result::Result<T, Error>;

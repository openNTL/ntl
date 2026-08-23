//! # Neural Transfer Layer (NTL)
//!
//! **The Neural Transfer Layer for Modern Compute**
//!
//! NTL is an open source data transfer layer that replaces the request-response
//! paradigm of traditional APIs with neural signal propagation.
//!
//! ## Quick Start
//!
//! ```rust
//! use ntl_core::{Node, NodeConfig, Signal};
//! use ntl_core::store::MemoryStore;
//! use std::sync::Arc;
//!
//! # fn main() -> Result<(), ntl_core::Error> {
//! let node = Node::builder()
//!     .with_config(NodeConfig::default())
//!     .with_store(Arc::new(MemoryStore::new()))
//!     .build()?;
//!
//! let signal = node.emit(
//!     Signal::data("hello")
//!         .with_payload(serde_json::json!({"message": "world"}))
//!         .with_weight(0.5),
//! )?;
//!
//! println!("Signal emitted: {}", signal.id);
//! # Ok(())
//! # }
//! ```
//!
//! ## No runtime assumptions
//!
//! `ntl-core` is pure Rust: no async executor, no transport, and no ambient
//! clock or randomness. Time and randomness are injected
//! ([`time::Clock`], [`rng::Rng`]), which keeps the crate buildable for
//! `wasm32-unknown-unknown` and — more usefully — makes decay, exploration,
//! and timeouts deterministically testable.
//!
//! Networking lives in the node binaries; storage lives behind
//! [`store::NodeStore`].

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// Clippy configuration lives in the workspace manifest, so every crate is
// linted the same way and the allow-list is documented in one place.

pub mod activation;
pub mod adapter;
pub mod config;
pub mod crypto;
pub mod delivery;
pub mod error;
pub mod learning;
pub mod node;
pub mod propagation;
pub mod rng;
pub mod signal;
pub mod store;
pub mod synapse;
pub mod testing;
pub mod time;
pub mod topology;

// Re-exports for convenience
pub use activation::ActivationFunction;
pub use adapter::{Adapter, AdapterHealth};
pub use config::NodeConfig;
pub use delivery::{DeliveryClass, Receipt, RejectReason};
pub use error::Error;
pub use learning::{DeploymentClass, ExplorationPolicy, LearningConfig};
pub use node::{Node, NodeBuilder};
pub use propagation::PropagationScope;
pub use rng::{Rng, SplitMix64};
pub use signal::{NodeId, Signal, SignalId, SignalType};
pub use store::{NodeStore, Outcome, StoreError};
pub use synapse::{Synapse, SynapseId, SynapseState};
pub use time::Clock;

/// Result type alias for NTL operations.
pub type Result<T> = std::result::Result<T, Error>;

//! Adapter trait and types for protocol translation.

use crate::signal::Signal;

/// Health state of an adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterHealth {
    /// Operating normally.
    Healthy,
    /// Partial functionality.
    Degraded {
        /// Why the adapter is degraded.
        reason: String,
    },
    /// Not functioning.
    Unhealthy {
        /// Why the adapter is unhealthy.
        reason: String,
    },
}

/// Capabilities an adapter can declare.
#[derive(Debug, Clone)]
pub struct AdapterCapabilities {
    /// Can receive external data and convert to signals.
    pub can_ingest: bool,
    /// Can convert signals to external format.
    pub can_emit: bool,
    /// Supports persistent bidirectional channels.
    pub bidirectional: bool,
    /// Supports request-response correlation.
    pub correlation: bool,
    /// Supports streaming signals.
    pub streaming: bool,
}

/// Generic container for external protocol data.
#[derive(Debug, Clone)]
pub struct ExternalPayload {
    /// Raw payload bytes.
    pub data: Vec<u8>,
    /// MIME content type.
    pub content_type: String,
    /// Protocol-specific metadata.
    pub metadata: std::collections::HashMap<String, String>,
}

/// Protocol identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Protocol {
    /// HTTP/HTTPS
    Http,
    /// WebSocket
    WebSocket,
    /// gRPC
    Grpc,
    /// GraphQL
    GraphQl,
    /// EVM-compatible blockchain
    EvmChain,
    /// Decentralized Identity
    Did,
    /// Custom protocol
    Custom(String),
}

/// The adapter interface for protocol translation.
///
/// Adapters bridge NTL's signal-based transport with external protocols.
pub trait Adapter: Send + Sync {
    /// Translate an external payload into an NTL signal.
    ///
    /// # Errors
    /// Returns an error if the payload cannot be represented as a signal.
    fn ingest(&self, external: ExternalPayload) -> crate::Result<Signal>;

    /// Translate an NTL signal into an external payload.
    ///
    /// # Errors
    /// Returns an error if the signal cannot be represented in the external
    /// protocol.
    fn emit(&self, signal: Signal) -> crate::Result<ExternalPayload>;

    /// The external protocol this adapter speaks.
    fn protocol(&self) -> Protocol;

    /// Adapter capabilities.
    fn capabilities(&self) -> AdapterCapabilities;

    /// Current health state.
    fn health(&self) -> AdapterHealth;
}

//! Configuration loading and management for NTL nodes.

use serde::{Deserialize, Serialize};

use crate::activation::ActivationConfig;
use crate::delivery::RetryPolicy;
use crate::learning::{DeploymentClass, LearningConfig};
use crate::propagation::PropagationConfig;
use crate::synapse::SynapseConfig;

/// Complete node configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    /// Network configuration.
    pub network: NetworkConfig,
    /// Synapse configuration.
    pub synapse: SynapseConfig,
    /// Propagation configuration.
    pub propagation: PropagationConfig,
    /// Activation configuration.
    pub activation: ActivationConfig,
    /// Routing-model hyperparameters.
    #[serde(default)]
    pub learning: LearningConfig,
    /// Sender-side retry policy for acknowledged delivery.
    #[serde(default)]
    pub retry: RetryPolicy,
    /// Storage backend configuration.
    #[serde(default)]
    pub storage: StorageConfig,
    /// Crypto module to use.
    ///
    /// Must name a module this build supports; see
    /// [`crate::crypto::supported_modules`]. Validated at load, because a
    /// silently-ignored crypto setting is worse than a missing one.
    #[serde(default = "default_crypto_module")]
    pub crypto_module: String,
}

/// Serde default for [`NodeConfig::crypto_module`].
///
/// Falls back to the literal rather than panicking when no crypto feature is
/// enabled, so parsing still succeeds and `validate` produces the useful
/// error instead of a deserialization failure.
fn default_crypto_module() -> String {
    crate::crypto::default_module()
        .unwrap_or("classical-v1")
        .to_string()
}

/// Which storage backend a node uses, and how.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "lowercase")]
pub enum StorageConfig {
    /// `SQLite` — the default. One file, no configuration.
    Sqlite {
        /// Path to the database file.
        path: String,
        /// Whether to retain signal bodies for replay. Off by default:
        /// payloads are the most privacy-sensitive data a node handles.
        #[serde(default)]
        retain_signal_history: bool,
    },
    /// `PostgreSQL` — planned.
    Postgres {
        /// Connection URL.
        url: String,
        /// Schema to keep NTL tables in.
        #[serde(default = "default_pg_schema")]
        schema: String,
    },
    /// In-memory. Nothing survives process exit.
    Memory,
}

fn default_pg_schema() -> String {
    "ntl".to_string()
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self::Sqlite {
            path: "~/.ntl/node.db".to_string(),
            retain_signal_history: false,
        }
    }
}

/// Network-level configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// Bootstrap node addresses.
    pub bootstrap_nodes: Vec<String>,
    /// Address to bind to.
    pub bind_address: String,
    /// Port to listen on.
    pub port: u16,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            network: NetworkConfig {
                bootstrap_nodes: vec![
                    "ntl://bootstrap-1.openntl.org:4433".to_string(),
                    "ntl://bootstrap-2.openntl.org:4433".to_string(),
                ],
                bind_address: "0.0.0.0".to_string(),
                port: 4433,
            },
            synapse: SynapseConfig::default(),
            propagation: PropagationConfig::default(),
            activation: ActivationConfig::default(),
            learning: LearningConfig::default(),
            retry: RetryPolicy::default(),
            storage: StorageConfig::default(),
            crypto_module: default_crypto_module(),
        }
    }
}

impl NodeConfig {
    /// Defaults for a deployment class, with activation and learning
    /// parameters chosen consistently.
    ///
    /// Mixing an edge learning rate with a server-class queue is a
    /// configuration people reach by accident; this makes the coherent
    /// combination the easy one.
    #[must_use]
    pub fn for_class(class: DeploymentClass) -> Self {
        let node_class = match class {
            DeploymentClass::Edge => crate::activation::NodeClass::Edge,
            DeploymentClass::FullNode => crate::activation::NodeClass::Standard,
            DeploymentClass::HighTraffic => crate::activation::NodeClass::Server,
        };
        let learning = LearningConfig::for_class(class);
        Self {
            activation: ActivationConfig::for_class(node_class),
            propagation: PropagationConfig {
                max_fanout: match class {
                    DeploymentClass::Edge => 3,
                    DeploymentClass::FullNode => 5,
                    DeploymentClass::HighTraffic => 8,
                },
                ..PropagationConfig::default()
            },
            learning,
            ..Self::default()
        }
    }

    /// Validate the whole configuration.
    ///
    /// # Errors
    /// Returns a description of the first inconsistency found.
    pub fn validate(&self) -> Result<(), String> {
        self.activation.validate()?;
        self.learning.validate()?;
        self.propagation
            .validate(self.retry.required_dedup_secs())?;
        // A crypto module this build cannot provide must fail loudly. The
        // alternative is a node that reports one algorithm and uses another.
        if !crate::crypto::supported_modules().contains(&self.crypto_module.as_str()) {
            let supported = crate::crypto::supported_modules();
            return Err(if supported.is_empty() {
                format!(
                    "crypto_module is {:?}, but this build has no crypto module \
                     compiled in. Enable the `classical-crypto` feature.",
                    self.crypto_module
                )
            } else {
                format!(
                    "crypto_module {:?} is not supported by this build. \
                     Supported: {}.",
                    self.crypto_module,
                    supported.join(", ")
                )
            });
        }
        if self.synapse.max_weight > self.learning.max_weight {
            return Err(format!(
                "synapse.max_weight ({}) exceeds learning.max_weight ({})",
                self.synapse.max_weight, self.learning.max_weight
            ));
        }
        Ok(())
    }

    /// Load configuration from a TOML file.
    ///
    /// # Errors
    /// Returns [`crate::Error::Config`] if the file cannot be read, cannot be
    /// parsed, or fails validation.
    pub fn from_file(path: &str) -> crate::Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| crate::Error::Config(format!("Failed to read {path}: {e}")))?;

        let parsed: Self = toml::from_str(&content)
            .map_err(|e| crate::Error::Config(format!("Failed to parse config: {e}")))?;
        parsed
            .validate()
            .map_err(|e| crate::Error::Config(format!("invalid config in {path}: {e}")))?;
        Ok(parsed)
    }

    /// Write configuration to a TOML file.
    ///
    /// # Errors
    /// Returns [`crate::Error::Config`] if serialization or the write fails.
    pub fn to_file(&self, path: &str) -> crate::Result<()> {
        let content = toml::to_string_pretty(self)
            .map_err(|e| crate::Error::Config(format!("Failed to serialize config: {e}")))?;

        std::fs::write(path, content)
            .map_err(|e| crate::Error::Config(format!("Failed to write {path}: {e}")))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_crypto_module_this_build_cannot_provide_is_rejected() {
        // The field defaulted to "pq-v1" and was read by nothing, so every
        // generated config advertised post-quantum crypto while signing with
        // Ed25519. A config option that appears to make a security choice and
        // does not is worse than no option.
        let mut config = NodeConfig::default();
        assert_eq!(
            config.crypto_module, "classical-v1",
            "the default must name what the build actually uses"
        );
        config.validate().expect("the default must validate");

        for bogus in ["pq-v1", "hybrid-v1", "", "CLASSICAL-V1"] {
            config.crypto_module = bogus.to_string();
            let err = config
                .validate()
                .expect_err("an unsupported module must be refused");
            assert!(
                err.contains("crypto_module"),
                "the error must name the field, got {err:?}"
            );
            assert!(
                err.contains("classical-v1"),
                "the error must say what is supported, got {err:?}"
            );
        }
    }

    #[test]
    fn the_shipped_example_config_parses_and_validates() {
        // config.example.toml is the file operators copy. A key placed after a
        // table header belongs to that table in TOML, so a top-level field
        // written in the wrong place is silently misfiled — which is exactly
        // what had happened to crypto_module.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config.example.toml");
        let text = std::fs::read_to_string(path).expect("config.example.toml must exist");
        let parsed: NodeConfig =
            toml::from_str(&text).expect("the example config must parse as a NodeConfig");
        parsed
            .validate()
            .expect("the example config must pass validation");
        assert_eq!(
            parsed.crypto_module, "classical-v1",
            "the example must name a module that exists"
        );
    }

    #[test]
    fn default_config_is_valid() {
        NodeConfig::default()
            .validate()
            .expect("shipped defaults must be self-consistent");
    }

    #[test]
    fn every_class_default_is_valid() {
        for class in [
            DeploymentClass::Edge,
            DeploymentClass::FullNode,
            DeploymentClass::HighTraffic,
        ] {
            NodeConfig::for_class(class)
                .validate()
                .unwrap_or_else(|e| panic!("{class:?} defaults invalid: {e}"));
        }
    }

    #[test]
    fn class_defaults_pair_activation_with_learning() {
        // Mixing an edge learning rate with a server queue is a mistake
        // people make by accident; the class constructor must not.
        let edge = NodeConfig::for_class(DeploymentClass::Edge);
        assert_eq!(
            edge.activation.node_class,
            crate::activation::NodeClass::Edge
        );
        let high = NodeConfig::for_class(DeploymentClass::HighTraffic);
        assert_eq!(
            high.activation.node_class,
            crate::activation::NodeClass::Server
        );
        assert!(high.propagation.max_fanout > edge.propagation.max_fanout);
    }

    #[test]
    fn validation_catches_dedup_shorter_than_retry_budget() {
        let mut c = NodeConfig::default();
        c.propagation.dedup_cache_seconds = 5;
        c.retry.total_deadline_secs = 300;
        assert!(c.validate().is_err());
    }

    #[test]
    fn config_roundtrips_through_toml() {
        let original = NodeConfig::for_class(DeploymentClass::FullNode);
        let text = toml::to_string_pretty(&original).expect("serialize");
        let parsed: NodeConfig = toml::from_str(&text).expect("deserialize");
        assert_eq!(parsed.learning, original.learning);
        assert_eq!(parsed.activation, original.activation);
        assert_eq!(parsed.storage, original.storage);
    }

    #[test]
    fn storage_default_is_sqlite_without_history() {
        match NodeConfig::default().storage {
            StorageConfig::Sqlite {
                retain_signal_history,
                ..
            } => assert!(
                !retain_signal_history,
                "signal history must default to off — payloads are sensitive"
            ),
            other => panic!("expected sqlite default, got {other:?}"),
        }
    }
}

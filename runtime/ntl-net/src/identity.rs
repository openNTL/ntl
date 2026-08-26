//! Node identity: keypair generation and on-disk storage.
//!
//! Identity must persist. A node that generates a fresh keypair on each start
//! loses every synapse it formed, so it relearns its routing model from
//! scratch every restart — and the learning model never converges.

use ntl_core::NodeStore;
use ntl_core::crypto::{PrivateKey, PublicKey, node_id_from_public_key};
use ntl_core::signal::NodeId;

/// A node's signing identity.
pub struct Identity {
    /// Public key.
    pub public: PublicKey,
    /// Private key. Never leaves the node.
    pub private: PrivateKey,
    /// Node identifier, derived from the public key.
    pub node_id: NodeId,
}

const SEED_KEY: &str = "identity-seed";
const NODE_ID_KEY: &str = "node-id";

impl Identity {
    /// Load the stored identity, generating and persisting one if absent.
    ///
    /// The seed comes from the operating system's CSPRNG. `ntl-core` has no
    /// ambient entropy source by design, so supplying real entropy is this
    /// crate's job.
    ///
    /// # Errors
    /// Returns an error if the store is unavailable or the stored seed is
    /// malformed.
    pub fn load_or_create(store: &dyn NodeStore) -> Result<Self, IdentityError> {
        if let Some(seed) = store
            .get_meta(SEED_KEY)
            .map_err(|e| IdentityError::Store(e.to_string()))?
        {
            return Self::from_seed(&seed);
        }

        // A fresh identity. Use the OS CSPRNG: the statistical generator used
        // for exploration sampling is not adequate for key material.
        let mut seed = [0u8; 32];
        {
            use rand::RngCore as _;
            rand::rngs::OsRng.fill_bytes(&mut seed);
        }

        let identity = Self::from_seed(&seed)?;
        store
            .put_meta(SEED_KEY, &seed)
            .map_err(|e| IdentityError::Store(e.to_string()))?;
        store
            .put_meta(NODE_ID_KEY, &identity.node_id.0)
            .map_err(|e| IdentityError::Store(e.to_string()))?;
        store
            .flush()
            .map_err(|e| IdentityError::Store(e.to_string()))?;
        Ok(identity)
    }

    /// Derive an identity from a 32-byte seed.
    ///
    /// # Errors
    /// Returns [`IdentityError::Malformed`] if the seed is the wrong length.
    pub fn from_seed(seed: &[u8]) -> Result<Self, IdentityError> {
        let (public, private) = ntl_core::crypto::ClassicalModule::keypair_from_seed(seed)
            .map_err(|e| IdentityError::Malformed(e.to_string()))?;
        let node_id = node_id_from_public_key(&public);
        Ok(Self {
            public,
            private,
            node_id,
        })
    }

    /// A short, human-readable form for logs and CLI output.
    #[must_use]
    pub fn short(&self) -> String {
        self.node_id
            .0
            .iter()
            .take(6)
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

/// Identity errors.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// The store could not be read or written.
    #[error("identity store error: {0}")]
    Store(String),
    /// The stored seed was not usable.
    #[error("malformed identity: {0}")]
    Malformed(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use ntl_core::store::MemoryStore;

    #[test]
    fn identity_is_stable_across_loads() {
        // The property that matters: a restart must not lose the identity, or
        // the node loses every synapse and relearns from scratch.
        let store = MemoryStore::new();
        let first = Identity::load_or_create(&store).expect("create");
        let second = Identity::load_or_create(&store).expect("load");
        assert_eq!(first.node_id, second.node_id);
        assert_eq!(first.public, second.public);
    }

    #[test]
    fn distinct_stores_yield_distinct_identities() {
        let a = Identity::load_or_create(&MemoryStore::new()).expect("a");
        let b = Identity::load_or_create(&MemoryStore::new()).expect("b");
        assert_ne!(a.node_id, b.node_id, "identities must not collide");
    }

    #[test]
    fn node_id_is_derived_from_the_public_key() {
        let store = MemoryStore::new();
        let identity = Identity::load_or_create(&store).expect("create");
        assert_eq!(
            identity.node_id,
            node_id_from_public_key(&identity.public),
            "the node id must be verifiable from the key alone"
        );
    }

    #[test]
    fn a_malformed_seed_is_rejected() {
        assert!(Identity::from_seed(&[0u8; 8]).is_err());
    }

    #[test]
    fn identity_can_sign_and_verify_its_own_signals() {
        let store = MemoryStore::new();
        let identity = Identity::load_or_create(&store).expect("create");
        let mut signal = ntl_core::Signal::data("t")
            .with_weight(0.5)
            .build_unsigned(identity.node_id.clone());

        ntl_core::crypto::sign_signal(
            &ntl_core::crypto::ClassicalModule,
            &mut signal,
            &identity.private,
        )
        .expect("sign");
        assert!(
            ntl_core::crypto::verify_signal(
                &ntl_core::crypto::ClassicalModule,
                &signal,
                &identity.public
            )
            .expect("verify")
        );
    }

    #[test]
    fn short_form_is_stable_and_hex() {
        let store = MemoryStore::new();
        let identity = Identity::load_or_create(&store).expect("create");
        let short = identity.short();
        assert_eq!(short.len(), 12);
        assert!(short.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(short, identity.short());
    }
}

//! Pluggable cryptographic module interface.
//!
//! NTL treats cryptography as a swappable module, not a foundational
//! dependency. This enables post-quantum readiness and crypto agility.

use crate::Result;

/// Crypto module identifiers this build can actually use.
///
/// A configuration naming anything else is rejected at load rather than
/// ignored. The field used to default to `"pq-v1"` and be read by nothing,
/// so every generated config claimed a post-quantum module while signing
/// with Ed25519 — the worst kind of configuration option, one that appears
/// to make a security choice and does not.
#[must_use]
pub fn supported_modules() -> &'static [&'static str] {
    &[
        #[cfg(feature = "classical-crypto")]
        "classical-v1",
    ]
}

/// The module a build uses when configuration does not say.
///
/// `None` when no crypto feature is enabled, in which case a node cannot
/// sign and must be configured with one.
#[must_use]
pub fn default_module() -> Option<&'static str> {
    supported_modules().first().copied()
}

/// Public key bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKey(pub Vec<u8>);

/// Private key bytes.
#[derive(Debug, Clone)]
pub struct PrivateKey(pub Vec<u8>);

/// Cryptographic signature bytes.
#[derive(Debug, Clone)]
pub struct Signature(pub Vec<u8>);

/// Shared secret derived from key exchange.
#[derive(Debug, Clone)]
pub struct SharedSecret(pub Vec<u8>);

/// Hash output bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hash(pub Vec<u8>);

/// The pluggable cryptographic module interface.
///
/// All cryptographic operations in NTL go through this trait.
/// Implementations can be swapped at runtime.
pub trait CryptoModule: Send + Sync {
    /// Unique identifier for this module (e.g., "pq-v1", "classical-v1").
    fn module_id(&self) -> &str;

    /// Generate a new keypair.
    /// # Errors
    /// Returns an error if the module cannot produce a keypair — for example
    /// when it has no entropy source of its own.
    fn generate_keypair(&self) -> Result<(PublicKey, PrivateKey)>;

    /// Sign data with a private key.
    /// # Errors
    /// Returns an error if the key is malformed or signing fails.
    fn sign(&self, data: &[u8], key: &PrivateKey) -> Result<Signature>;

    /// Verify a signature against a public key.
    /// A malformed signature yields `Ok(false)` rather than an error: that is
    /// what a forged or truncated signature looks like on the wire.
    ///
    /// # Errors
    /// Returns an error only if the *public key* is malformed.
    fn verify(&self, data: &[u8], signature: &Signature, key: &PublicKey) -> Result<bool>;

    /// Encrypt data for a recipient's public key.
    /// # Errors
    /// Returns an error if the module does not support encryption, or the key
    /// is malformed.
    fn encrypt(&self, data: &[u8], recipient_key: &PublicKey) -> Result<Vec<u8>>;

    /// Decrypt data with a private key.
    /// # Errors
    /// Returns an error if the module does not support decryption, the key is
    /// malformed, or the ciphertext does not authenticate.
    fn decrypt(&self, data: &[u8], key: &PrivateKey) -> Result<Vec<u8>>;

    /// Perform key exchange to derive a shared secret.
    /// # Errors
    /// Returns an error if the module does not support key agreement, or a key
    /// is malformed.
    fn key_exchange(
        &self,
        local_private: &PrivateKey,
        remote_public: &PublicKey,
    ) -> Result<SharedSecret>;

    /// Compute a cryptographic hash.
    fn hash(&self, data: &[u8]) -> Hash;
}

/// BLAKE3-based hashing (used across all crypto modules).
#[must_use]
pub fn blake3_hash(data: &[u8]) -> Hash {
    Hash(blake3::hash(data).as_bytes().to_vec())
}

/// Derive a `NodeId` from a public key.
#[must_use]
pub fn node_id_from_public_key(key: &PublicKey) -> crate::signal::NodeId {
    let hash = blake3_hash(&key.0);
    crate::signal::NodeId(hash.0)
}

/// Ed25519 signing and X25519 key exchange.
///
/// Available under the `classical-crypto` feature. This is the module the
/// reference implementation uses today: it is small, fast, and pure Rust, so
/// it works on every target `ntl-core` builds for.
///
/// It is **not** post-quantum. A deployment with a long confidentiality
/// horizon should prefer a PQ module once one ships; see
/// [spec/crypto-interface](https://openntl.org/spec/crypto-interface). The
/// point of the trait is that this choice is not baked into the protocol.
#[cfg(feature = "classical-crypto")]
pub struct ClassicalModule;

#[cfg(feature = "classical-crypto")]
impl ClassicalModule {
    /// Module identifier used in handshakes.
    pub const ID: &'static str = "classical-v1";

    /// Derive a keypair deterministically from 32 seed bytes.
    ///
    /// Key generation takes its randomness as an argument, because
    /// `ntl-core` has no ambient entropy source — see [`crate::rng`] for why.
    /// The caller MUST supply cryptographically secure bytes: the statistical
    /// generator used for exploration sampling is not adequate here.
    ///
    /// # Errors
    /// Returns [`crate::Error::Crypto`] if the seed is not 32 bytes.
    pub fn keypair_from_seed(seed: &[u8]) -> crate::Result<(PublicKey, PrivateKey)> {
        let bytes: [u8; 32] = seed.try_into().map_err(|_| {
            crate::Error::Crypto(format!("seed must be 32 bytes, got {}", seed.len()))
        })?;
        let signing = ed25519_dalek::SigningKey::from_bytes(&bytes);
        Ok((
            PublicKey(signing.verifying_key().to_bytes().to_vec()),
            PrivateKey(signing.to_bytes().to_vec()),
        ))
    }

    fn signing_key(key: &PrivateKey) -> crate::Result<ed25519_dalek::SigningKey> {
        let bytes: [u8; 32] = key.0.as_slice().try_into().map_err(|_| {
            crate::Error::Crypto(format!("private key must be 32 bytes, got {}", key.0.len()))
        })?;
        Ok(ed25519_dalek::SigningKey::from_bytes(&bytes))
    }

    fn verifying_key(key: &PublicKey) -> crate::Result<ed25519_dalek::VerifyingKey> {
        let bytes: [u8; 32] = key.0.as_slice().try_into().map_err(|_| {
            crate::Error::Crypto(format!("public key must be 32 bytes, got {}", key.0.len()))
        })?;
        ed25519_dalek::VerifyingKey::from_bytes(&bytes)
            .map_err(|e| crate::Error::Crypto(format!("invalid public key: {e}")))
    }
}

#[cfg(feature = "classical-crypto")]
impl CryptoModule for ClassicalModule {
    fn module_id(&self) -> &str {
        Self::ID
    }

    fn generate_keypair(&self) -> Result<(PublicKey, PrivateKey)> {
        // Deliberately unsupported: this module cannot invent entropy, and
        // silently using a weak source would be worse than refusing.
        Err(crate::Error::Crypto(
            "use ClassicalModule::keypair_from_seed with 32 cryptographically \
             secure bytes; this module has no ambient entropy source"
                .to_string(),
        ))
    }

    fn sign(&self, data: &[u8], key: &PrivateKey) -> Result<Signature> {
        use ed25519_dalek::Signer as _;
        let signing = Self::signing_key(key)?;
        Ok(Signature(signing.sign(data).to_bytes().to_vec()))
    }

    fn verify(&self, data: &[u8], signature: &Signature, key: &PublicKey) -> Result<bool> {
        use ed25519_dalek::Verifier as _;
        let verifying = Self::verifying_key(key)?;
        let bytes: [u8; 64] = match signature.0.as_slice().try_into() {
            Ok(b) => b,
            // A wrong-length signature is a failed verification, not an
            // error: it is exactly what a malformed or forged signal looks
            // like, and callers must treat it as such.
            Err(_) => return Ok(false),
        };
        let sig = ed25519_dalek::Signature::from_bytes(&bytes);
        Ok(verifying.verify(data, &sig).is_ok())
    }

    fn encrypt(&self, _data: &[u8], _recipient_key: &PublicKey) -> Result<Vec<u8>> {
        Err(crate::Error::Crypto(
            "payload encryption is not implemented in classical-v1; NTL signs \
             payloads but does not encrypt them by default"
                .to_string(),
        ))
    }

    fn decrypt(&self, _data: &[u8], _key: &PrivateKey) -> Result<Vec<u8>> {
        Err(crate::Error::Crypto(
            "payload encryption is not implemented in classical-v1".to_string(),
        ))
    }

    fn key_exchange(
        &self,
        _local_private: &PrivateKey,
        _remote_public: &PublicKey,
    ) -> Result<SharedSecret> {
        // Ed25519 signing keys are not X25519 agreement keys. Conflating them
        // is a classic footgun, so this refuses rather than reinterpreting the
        // bytes.
        Err(crate::Error::Crypto(
            "classical-v1 signing keys are Ed25519 and cannot be used for \
             X25519 key exchange; a separate agreement key is required"
                .to_string(),
        ))
    }

    fn hash(&self, data: &[u8]) -> Hash {
        blake3_hash(data)
    }
}

/// Verify a signal's signature against its origin's public key.
///
/// Returns `false` for a malformed signature rather than erroring: a forged
/// or truncated signature is a verification failure, and the caller applies
/// the same penalty either way.
///
/// # Errors
/// Returns [`crate::Error::Serialization`] if the signal cannot be
/// re-encoded for verification.
pub fn verify_signal(
    module: &dyn CryptoModule,
    signal: &crate::signal::Signal,
    public_key: &PublicKey,
) -> crate::Result<bool> {
    let bytes = signal.signing_bytes()?;
    module.verify(&bytes, &Signature(signal.signature.clone()), public_key)
}

/// Sign a signal in place.
///
/// # Errors
/// Returns an error if encoding or signing fails.
pub fn sign_signal(
    module: &dyn CryptoModule,
    signal: &mut crate::signal::Signal,
    private_key: &PrivateKey,
) -> crate::Result<()> {
    let bytes = signal.signing_bytes()?;
    signal.signature = module.sign(&bytes, private_key)?.0;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blake3_hash_deterministic() {
        let data = b"hello NTL";
        let h1 = blake3_hash(data);
        let h2 = blake3_hash(data);
        assert_eq!(h1, h2);
    }

    #[test]
    fn blake3_hash_different_inputs() {
        let h1 = blake3_hash(b"hello");
        let h2 = blake3_hash(b"world");
        assert_ne!(h1, h2);
    }

    #[cfg(feature = "classical-crypto")]
    mod classical {
        use super::super::*;
        use crate::signal::Signal;

        fn keys() -> (PublicKey, PrivateKey) {
            ClassicalModule::keypair_from_seed(&[7u8; 32]).expect("keypair")
        }

        #[test]
        fn seed_derivation_is_deterministic() {
            let (a_pub, _) = keys();
            let (b_pub, _) = keys();
            assert_eq!(a_pub, b_pub, "the same seed must yield the same key");
        }

        #[test]
        fn distinct_seeds_yield_distinct_keys() {
            let (a, _) = ClassicalModule::keypair_from_seed(&[1u8; 32]).expect("a");
            let (b, _) = ClassicalModule::keypair_from_seed(&[2u8; 32]).expect("b");
            assert_ne!(a, b);
        }

        #[test]
        fn short_seed_is_rejected() {
            assert!(ClassicalModule::keypair_from_seed(&[0u8; 16]).is_err());
        }

        #[test]
        fn generate_keypair_refuses_rather_than_inventing_entropy() {
            assert!(
                ClassicalModule.generate_keypair().is_err(),
                "silently using a weak entropy source would be worse than refusing"
            );
        }

        #[test]
        fn sign_then_verify_succeeds() {
            let (public, private) = keys();
            let sig = ClassicalModule.sign(b"payload", &private).expect("sign");
            assert!(
                ClassicalModule
                    .verify(b"payload", &sig, &public)
                    .expect("verify")
            );
        }

        #[test]
        fn a_tampered_message_fails_verification() {
            let (public, private) = keys();
            let sig = ClassicalModule.sign(b"payload", &private).expect("sign");
            assert!(
                !ClassicalModule
                    .verify(b"tampered", &sig, &public)
                    .expect("verify")
            );
        }

        #[test]
        fn a_wrong_key_fails_verification() {
            let (_, private) = keys();
            let (other_public, _) = ClassicalModule::keypair_from_seed(&[9u8; 32]).expect("k");
            let sig = ClassicalModule.sign(b"payload", &private).expect("sign");
            assert!(
                !ClassicalModule
                    .verify(b"payload", &sig, &other_public)
                    .expect("verify")
            );
        }

        #[test]
        fn a_malformed_signature_is_a_failure_not_an_error() {
            let (public, _) = keys();
            // This is what a forged or truncated signature looks like on the
            // wire, so it must verify to false rather than blowing up.
            for bad in [vec![], vec![0u8; 8], vec![0u8; 63], vec![0u8; 65]] {
                assert!(
                    !ClassicalModule
                        .verify(b"payload", &Signature(bad.clone()), &public)
                        .expect("must not error"),
                    "a {}-byte signature should fail cleanly",
                    bad.len()
                );
            }
        }

        #[test]
        fn signal_signing_round_trips() {
            let (public, private) = keys();
            let origin = node_id_from_public_key(&public);
            let mut signal = Signal::data("test").with_weight(0.5).build_unsigned(origin);

            sign_signal(&ClassicalModule, &mut signal, &private).expect("sign");
            assert!(!signal.signature.is_empty());
            assert!(verify_signal(&ClassicalModule, &signal, &public).expect("verify"));
        }

        #[test]
        fn tampering_with_a_signal_field_breaks_its_signature() {
            let (public, private) = keys();
            let origin = node_id_from_public_key(&public);
            let mut signal = Signal::data("test").with_weight(0.5).build_unsigned(origin);
            sign_signal(&ClassicalModule, &mut signal, &private).expect("sign");

            // An intermediate node must not be able to upgrade the delivery
            // class, retarget the payload, or restamp the origin.
            let mut tampered = signal.clone();
            tampered.delivery = crate::delivery::DeliveryClass::Acknowledged;
            assert!(!verify_signal(&ClassicalModule, &tampered, &public).expect("verify"));

            let mut tampered = signal.clone();
            tampered.payload = serde_json::json!({"evil": true});
            assert!(!verify_signal(&ClassicalModule, &tampered, &public).expect("verify"));

            let mut tampered = signal.clone();
            tampered.timestamp = 0;
            assert!(!verify_signal(&ClassicalModule, &tampered, &public).expect("verify"));

            let mut tampered = signal.clone();
            tampered.origin = crate::signal::NodeId(vec![1u8; 32]);
            assert!(!verify_signal(&ClassicalModule, &tampered, &public).expect("verify"));

            let mut tampered = signal.clone();
            tampered.tags = vec!["injected".to_string()];
            assert!(!verify_signal(&ClassicalModule, &tampered, &public).expect("verify"));
        }

        #[test]
        fn weight_and_ttl_are_knowingly_unprotected() {
            // Documented limitation, asserted so it stays deliberate rather
            // than becoming an accident. Propagation mutates weight and TTL at
            // every hop, so an origin signature cannot cover them — an on-path
            // node CAN inflate both. See spec/threat-model §6 for what bounds
            // the abuse.
            let (public, private) = keys();
            let origin = node_id_from_public_key(&public);
            let mut signal = Signal::data("test").with_weight(0.5).build_unsigned(origin);
            sign_signal(&ClassicalModule, &mut signal, &private).expect("sign");

            let mut inflated = signal.clone();
            inflated.weight = 1.0;
            inflated.ttl = 255;
            assert!(
                verify_signal(&ClassicalModule, &inflated, &public).expect("verify"),
                "weight and TTL are excluded from the signature by design; a \
                 receiving node must enforce its own limits on both rather \
                 than trusting them"
            );
        }

        #[test]
        fn hops_do_not_invalidate_the_origin_signature() {
            // The trace grows as a signal travels, so it cannot be covered by
            // a signature that must still verify downstream.
            let (public, private) = keys();
            let origin = node_id_from_public_key(&public);
            let mut signal = Signal::data("test").with_weight(0.5).build_unsigned(origin);
            sign_signal(&ClassicalModule, &mut signal, &private).expect("sign");

            signal.hop(crate::signal::NodeId(vec![42u8; 32]));
            signal.hop(crate::signal::NodeId(vec![43u8; 32]));

            assert!(
                verify_signal(&ClassicalModule, &signal, &public).expect("verify"),
                "a signal must still verify after being forwarded"
            );
        }

        #[test]
        fn key_exchange_refuses_to_reuse_signing_keys() {
            let (public, private) = keys();
            assert!(
                ClassicalModule.key_exchange(&private, &public).is_err(),
                "Ed25519 signing keys are not X25519 agreement keys"
            );
        }
    }

    #[test]
    fn node_id_from_key_deterministic() {
        let key = PublicKey(vec![42u8; 32]);
        let id1 = node_id_from_public_key(&key);
        let id2 = node_id_from_public_key(&key);
        assert_eq!(id1, id2);
    }
}

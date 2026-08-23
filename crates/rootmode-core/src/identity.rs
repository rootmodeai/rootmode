//! ed25519 identity. The peer id *is* the public key, hex-encoded — no
//! registry, no account, nothing to look up. You own the node.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};

use crate::{CoreError, Result};

/// A locally-held keypair. The secret half never leaves the process except
/// through [`Identity::export_secret_hex`], which the UI gates behind a warning.
#[derive(Clone)]
pub struct Identity {
    signing: SigningKey,
}

/// The public half, safe to show and ship over the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublicIdentity {
    pub peer_id: String,
    pub public_key_hex: String,
}

impl Identity {
    pub fn generate() -> Self {
        Self {
            signing: SigningKey::generate(&mut OsRng),
        }
    }

    /// 32-byte seed, hex-encoded.
    pub fn from_secret_hex(hex_str: &str) -> Result<Self> {
        let raw = hex::decode(hex_str.trim())
            .map_err(|e| CoreError::Key(format!("secret is not hex: {e}")))?;
        let bytes: [u8; 32] = raw
            .try_into()
            .map_err(|_| CoreError::Key("secret must be 32 bytes (64 hex chars)".into()))?;
        Ok(Self {
            signing: SigningKey::from_bytes(&bytes),
        })
    }

    pub fn export_secret_hex(&self) -> String {
        hex::encode(self.signing.to_bytes())
    }

    pub fn public_key_hex(&self) -> String {
        hex::encode(self.signing.verifying_key().to_bytes())
    }

    /// Peer ids are the public key in hex. Stable, self-certifying, ugly —
    /// which is the point: it is checkable by eye against what a peer shows.
    pub fn peer_id(&self) -> String {
        self.public_key_hex()
    }

    pub fn public(&self) -> PublicIdentity {
        PublicIdentity {
            peer_id: self.peer_id(),
            public_key_hex: self.public_key_hex(),
        }
    }

    pub fn sign_hex(&self, msg: &[u8]) -> String {
        hex::encode(self.signing.sign(msg).to_bytes())
    }
}

/// Verify a hex signature against a hex ed25519 public key.
pub fn verify_hex(public_key_hex: &str, msg: &[u8], sig_hex: &str) -> Result<()> {
    let pk_raw = hex::decode(public_key_hex.trim())
        .map_err(|e| CoreError::Key(format!("public key is not hex: {e}")))?;
    let pk_bytes: [u8; 32] = pk_raw
        .try_into()
        .map_err(|_| CoreError::Key("public key must be 32 bytes".into()))?;
    let vk = VerifyingKey::from_bytes(&pk_bytes)
        .map_err(|e| CoreError::Key(format!("bad public key: {e}")))?;

    let sig_raw = hex::decode(sig_hex.trim())
        .map_err(|e| CoreError::Signature(format!("signature is not hex: {e}")))?;
    let sig_bytes: [u8; 64] = sig_raw
        .try_into()
        .map_err(|_| CoreError::Signature("signature must be 64 bytes".into()))?;

    vk.verify(msg, &Signature::from_bytes(&sig_bytes))
        .map_err(|e| CoreError::Signature(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_verify() {
        let id = Identity::generate();
        let restored = Identity::from_secret_hex(&id.export_secret_hex()).unwrap();
        assert_eq!(id.peer_id(), restored.peer_id());

        let sig = id.sign_hex(b"job.submit");
        verify_hex(&id.public_key_hex(), b"job.submit", &sig).unwrap();
        assert!(verify_hex(&id.public_key_hex(), b"tampered", &sig).is_err());
    }

    #[test]
    fn rejects_short_secret() {
        assert!(Identity::from_secret_hex("dead").is_err());
    }
}

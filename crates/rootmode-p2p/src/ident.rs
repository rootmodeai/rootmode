//! One key, two encodings.
//!
//! A rootmode `peer_id` is the hex ed25519 public key. A libp2p `PeerId` is a
//! multihash of the protobuf-encoded public key — and because ed25519 keys are
//! short, libp2p uses the *identity* hash, so the public key is carried inside
//! the PeerId rather than hashed away.
//!
//! That means the two are interchangeable, and a key you pinned in the client
//! is the same key that secures the libp2p connection. There is no second
//! identity system.

use libp2p::identity::{ed25519, Keypair, PublicKey};
use libp2p::PeerId;

use crate::{P2pError, Result};

/// Multihash code for "identity" (no hashing).
const IDENTITY_HASH: u64 = 0x00;

/// Build the libp2p keypair from a rootmode identity — the same secret,
/// so the node's network identity and its protocol identity cannot diverge.
pub fn keypair_from(identity: &rootmode_core::Identity) -> Result<Keypair> {
    let mut secret = hex::decode(identity.export_secret_hex())
        .map_err(|e| P2pError::Identity(format!("secret is not hex: {e}")))?;
    Keypair::ed25519_from_bytes(&mut secret)
        .map_err(|e| P2pError::Identity(format!("bad ed25519 secret: {e}")))
}

/// The rootmode peer id (hex public key) for a libp2p peer.
pub fn peer_id_to_hex(peer: &PeerId) -> Option<String> {
    let multihash = peer.as_ref();
    if multihash.code() != IDENTITY_HASH {
        return None;
    }
    let public = PublicKey::try_decode_protobuf(multihash.digest()).ok()?;
    let ed = public.try_into_ed25519().ok()?;
    Some(hex::encode(ed.to_bytes()))
}

/// The libp2p peer for a rootmode peer id.
pub fn peer_id_from_hex(hex_public_key: &str) -> Result<PeerId> {
    let raw = hex::decode(hex_public_key.trim())
        .map_err(|e| P2pError::Identity(format!("peer id is not hex: {e}")))?;
    let ed = ed25519::PublicKey::try_from_bytes(&raw)
        .map_err(|e| P2pError::Identity(format!("not an ed25519 public key: {e}")))?;
    Ok(PeerId::from_public_key(&PublicKey::from(ed)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rootmode_core::Identity;

    #[test]
    fn the_two_encodings_are_the_same_key() {
        let identity = Identity::generate();
        let keypair = keypair_from(&identity).unwrap();
        let peer = keypair.public().to_peer_id();

        assert_eq!(
            peer_id_to_hex(&peer).as_deref(),
            Some(identity.peer_id().as_str()),
            "the libp2p peer id carries our peer id"
        );
        assert_eq!(peer_id_from_hex(&identity.peer_id()).unwrap(), peer);
    }

    #[test]
    fn the_same_secret_always_yields_the_same_node() {
        let identity = Identity::generate();
        let a = keypair_from(&identity).unwrap().public().to_peer_id();
        let restored = Identity::from_secret_hex(&identity.export_secret_hex()).unwrap();
        let b = keypair_from(&restored).unwrap().public().to_peer_id();
        assert_eq!(a, b, "a node keeps its address across restarts");
    }

    #[test]
    fn rejects_junk() {
        assert!(peer_id_from_hex("nonsense").is_err());
        assert!(peer_id_from_hex(&"ab".repeat(10)).is_err());
    }
}

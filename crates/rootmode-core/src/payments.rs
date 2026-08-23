//! Paying for work.
//!
//! Money leaves a client's balance only with that client's signature. The
//! live path is [`ReserveTicket`] / [`SpendTicket`] against `RootmodePot`:
//! lock before work, capture after. [`ReserveAuth`] / [`SpendingAuth`] are
//! the earlier session-auth types; digest tests still pin them.

use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256};

use crate::{CoreError, Result};

/// USDC has six decimals, so amounts are integers of 1e-6 dollars. Floats have
/// no business in a number that becomes a transfer.
pub type Micros = u64;

/// What the contract knows itself as, for EIP-712.
pub const DOMAIN_NAME: &str = "RootmodeChannels";
pub const DOMAIN_VERSION: &str = "1";

pub fn keccak(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Keccak256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// Which contract, on which chain, these signatures are good for.
///
/// Part of every digest, so an authorisation for one deployment cannot be
/// replayed against another — a testnet signature must not spend mainnet
/// money.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Domain {
    pub chain_id: u64,
    /// The channels contract, `0x…`.
    pub verifying_contract: String,
}

impl Domain {
    /// Base mainnet. Base Sepolia is 84532 for testing.
    pub fn base(verifying_contract: impl Into<String>) -> Self {
        Self {
            chain_id: 8453,
            verifying_contract: verifying_contract.into(),
        }
    }

    pub fn for_chain(chain_id: u64, verifying_contract: impl Into<String>) -> Self {
        Self {
            chain_id,
            verifying_contract: verifying_contract.into(),
        }
    }

    fn separator_named(&self, name: &str) -> Result<[u8; 32]> {
        let type_hash = keccak(
            b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
        );
        let mut buf = Vec::with_capacity(160);
        buf.extend_from_slice(&type_hash);
        buf.extend_from_slice(&keccak(name.as_bytes()));
        buf.extend_from_slice(&keccak(DOMAIN_VERSION.as_bytes()));
        buf.extend_from_slice(&word_u64(self.chain_id));
        buf.extend_from_slice(&word_address(&self.verifying_contract)?);
        Ok(keccak(&buf))
    }
}

/// A session between one client and one worker.
///
/// Derived from both parties and a salt, so two sessions between the same pair
/// are separate ledgers and one cannot be replayed into the other.
pub fn channel_id(client: &str, worker: &str, salt: &str) -> String {
    let mut buf = Vec::new();
    buf.extend_from_slice(client.to_lowercase().as_bytes());
    buf.extend_from_slice(worker.to_lowercase().as_bytes());
    buf.extend_from_slice(salt.as_bytes());
    format!("0x{}", hex::encode(keccak(&buf)))
}

/// What the client commits to about the work being charged for.
///
/// Committed rather than carried: the contract never needs the model name or
/// the token counts, only that the client agreed to a specific piece of work
/// at a specific price. Anyone auditing a charge later can recompute this from
/// the job and the result.
pub fn metadata_hash(model: &str, tokens_in: u64, tokens_out: u64, images: u64, sha256: &str) -> String {
    let mut buf = Vec::new();
    buf.extend_from_slice(model.as_bytes());
    buf.push(0);
    buf.extend_from_slice(&tokens_in.to_be_bytes());
    buf.extend_from_slice(&tokens_out.to_be_bytes());
    buf.extend_from_slice(&images.to_be_bytes());
    buf.extend_from_slice(sha256.as_bytes());
    format!("0x{}", hex::encode(keccak(&buf)))
}

/// *"With this worker, I authorise up to `max_amount` until `deadline`."*
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReserveAuth {
    pub channel_id: String,
    /// The paying address, `0x…`. Not the peer id: money moves on the chain's
    /// terms, and the two key systems are different.
    pub client: String,
    pub worker_payout: String,
    pub max_amount: Micros,
    /// Unix seconds. After this the reservation is the client's to reclaim.
    pub deadline: u64,
    /// Hex signature, 65 bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

impl ReserveAuth {
    pub fn digest(&self, domain: &Domain) -> Result<[u8; 32]> {
        let type_hash = keccak(
            b"ReserveAuth(bytes32 channelId,address client,address workerPayout,uint256 maxAmount,uint256 deadline)",
        );
        let mut buf = Vec::new();
        buf.extend_from_slice(&type_hash);
        buf.extend_from_slice(&word_bytes32(&self.channel_id)?);
        buf.extend_from_slice(&word_address(&self.client)?);
        buf.extend_from_slice(&word_address(&self.worker_payout)?);
        buf.extend_from_slice(&word_u64(self.max_amount));
        buf.extend_from_slice(&word_u64(self.deadline));
        digest_of(domain, &keccak(&buf))
    }

    /// The address that signed, which must be the one paying.
    pub fn recover(&self, domain: &Domain) -> Result<String> {
        let sig = self
            .sig
            .as_deref()
            .ok_or_else(|| CoreError::Signature("reservation is unsigned".into()))?;
        recover(&self.digest(domain)?, sig)
    }

    pub fn verify(&self, domain: &Domain, now: u64) -> Result<()> {
        if self.deadline <= now {
            return Err(CoreError::Invalid("this reservation has expired".into()));
        }
        let signer = self.recover(domain)?;
        if !signer.eq_ignore_ascii_case(&self.client) {
            return Err(CoreError::Signature(format!(
                "reservation signed by {signer}, not by the paying account {}",
                self.client
            )));
        }
        Ok(())
    }
}

/// *"Cumulative spend on this channel is now `cumulative`."*
///
/// Monotonic on purpose: a worker keeps only the newest, and the contract pays
/// the difference against what it has already settled. Replaying an old one
/// therefore pays nothing, and losing one costs only the jobs since the last.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpendingAuth {
    pub channel_id: String,
    pub client: String,
    pub cumulative: Micros,
    /// What this spend is for. See [`metadata_hash`].
    pub metadata_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

impl SpendingAuth {
    pub fn digest(&self, domain: &Domain) -> Result<[u8; 32]> {
        let type_hash = keccak(
            b"SpendingAuth(bytes32 channelId,address client,uint256 cumulative,bytes32 metadataHash)",
        );
        let mut buf = Vec::new();
        buf.extend_from_slice(&type_hash);
        buf.extend_from_slice(&word_bytes32(&self.channel_id)?);
        buf.extend_from_slice(&word_address(&self.client)?);
        buf.extend_from_slice(&word_u64(self.cumulative));
        buf.extend_from_slice(&word_bytes32(&self.metadata_hash)?);
        digest_of(domain, &keccak(&buf))
    }

    pub fn recover(&self, domain: &Domain) -> Result<String> {
        let sig = self
            .sig
            .as_deref()
            .ok_or_else(|| CoreError::Signature("spending authorisation is unsigned".into()))?;
        recover(&self.digest(domain)?, sig)
    }

    /// Everything a worker must check before doing the work.
    ///
    /// `already` is the highest cumulative this worker has seen on the
    /// channel; `reserved` is what the session authorised. The three failures
    /// this catches are the three ways a worker ends up unpaid: a signature
    /// from somebody who cannot pay, an amount that goes backwards, and an
    /// amount beyond what was ever reserved.
    pub fn check(&self, domain: &Domain, already: Micros, reserved: Micros) -> Result<Micros> {
        let signer = self.recover(domain)?;
        if !signer.eq_ignore_ascii_case(&self.client) {
            return Err(CoreError::Signature(format!(
                "authorisation signed by {signer}, not by {}",
                self.client
            )));
        }
        if self.cumulative <= already {
            return Err(CoreError::Invalid(format!(
                "cumulative spend must rise: {} is not more than {already}",
                self.cumulative
            )));
        }
        if self.cumulative > reserved {
            return Err(CoreError::Invalid(format!(
                "{} is beyond the {reserved} reserved for this session",
                self.cumulative
            )));
        }
        Ok(self.cumulative - already)
    }
}

// ------------------------------------------------------------------ helpers

fn digest_of(domain: &Domain, struct_hash: &[u8; 32]) -> Result<[u8; 32]> {
    digest_of_named(domain, DOMAIN_NAME, struct_hash)
}

fn digest_of_named(domain: &Domain, name: &str, struct_hash: &[u8; 32]) -> Result<[u8; 32]> {
    // EIP-191 0x19 || 0x01 || domainSeparator || hashStruct
    let mut buf = Vec::with_capacity(66);
    buf.extend_from_slice(&[0x19, 0x01]);
    buf.extend_from_slice(&domain.separator_named(name)?);
    buf.extend_from_slice(struct_hash);
    Ok(keccak(&buf))
}

/// Lock this much of the pot for one worker, until `deadline`.
///
/// Posted on-chain before work. Withdraw cannot take it. That is what makes
/// payment independent of whatever JavaScript the client is running.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReserveTicket {
    pub client: String,
    pub worker_payout: String,
    pub max_amount: Micros,
    pub deadline: u64,
}

impl ReserveTicket {
    pub fn digest(&self, domain: &Domain) -> Result<[u8; 32]> {
        let type_hash = keccak(
            b"ReserveTicket(address client,address workerPayout,uint256 maxAmount,uint64 deadline)",
        );
        let mut buf = Vec::new();
        buf.extend_from_slice(&type_hash);
        buf.extend_from_slice(&word_address(&self.client)?);
        buf.extend_from_slice(&word_address(&self.worker_payout)?);
        buf.extend_from_slice(&word_u64(self.max_amount));
        buf.extend_from_slice(&word_u64(self.deadline));
        digest_of_named(domain, POT_DOMAIN_NAME, &keccak(&buf))
    }
}

/// The ticket the app signs after work: total authorised for this worker so far.
///
/// Cumulative on purpose. The newest supersedes everything before it, so ten
/// jobs are ten signatures and one on-chain settle. The wallet does not sign
/// this — the app key authorised at deposit does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpendTicket {
    pub client: String,
    pub worker_payout: String,
    pub cumulative: Micros,
    pub deadline: u64,
}

pub const POT_DOMAIN_NAME: &str = "RootmodePot";

impl SpendTicket {
    pub fn digest(&self, domain: &Domain) -> Result<[u8; 32]> {
        let type_hash = keccak(
            b"SpendTicket(address client,address workerPayout,uint256 cumulative,uint64 deadline)",
        );
        let mut buf = Vec::new();
        buf.extend_from_slice(&type_hash);
        buf.extend_from_slice(&word_address(&self.client)?);
        buf.extend_from_slice(&word_address(&self.worker_payout)?);
        buf.extend_from_slice(&word_u64(self.cumulative));
        buf.extend_from_slice(&word_u64(self.deadline));
        digest_of_named(domain, POT_DOMAIN_NAME, &keccak(&buf))
    }

    pub fn recover(&self, domain: &Domain, sig: &str) -> Result<String> {
        recover(&self.digest(domain)?, sig)
    }

    /// Everything a worker must confirm before doing or banking priced work.
    ///
    /// `app_key` is the key the client registered on the pot for this account —
    /// exactly what the contract checks `settle` against. A ticket signed by any
    /// other key can never settle, so recovering *a* signer is not enough: the
    /// signer must be `app_key`, and the ticket must not have expired. This is
    /// the pot-path equivalent of [`SpendingAuth::check`], which the earlier
    /// session-auth path already had.
    pub fn check(&self, domain: &Domain, sig: &str, app_key: &str, now: u64) -> Result<()> {
        if self.deadline <= now {
            return Err(CoreError::Invalid(format!(
                "spend ticket expired at {}, it is now {now}",
                self.deadline
            )));
        }
        let signer = self.recover(domain, sig)?;
        if !signer.eq_ignore_ascii_case(app_key) {
            return Err(CoreError::Signature(format!(
                "ticket signed by {signer}, not by the account's app key {app_key}"
            )));
        }
        Ok(())
    }
}

/// `jobId` on chain is the keccak of the protocol job uuid, so a UUID fits a bytes32.
pub fn job_id_word(job_id: &str) -> String {
    format!("0x{}", hex::encode(keccak(job_id.as_bytes())))
}

fn word_u64(value: u64) -> [u8; 32] {
    let mut word = [0u8; 32];
    word[24..].copy_from_slice(&value.to_be_bytes());
    word
}

fn word_address(address: &str) -> Result<[u8; 32]> {
    let raw = address.strip_prefix("0x").unwrap_or(address);
    let bytes = hex::decode(raw).map_err(|_| CoreError::Invalid(format!("not an address: {address}")))?;
    if bytes.len() != 20 {
        return Err(CoreError::Invalid(format!("not an address: {address}")));
    }
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(&bytes);
    Ok(word)
}

fn word_bytes32(value: &str) -> Result<[u8; 32]> {
    let raw = value.strip_prefix("0x").unwrap_or(value);
    let bytes = hex::decode(raw).map_err(|_| CoreError::Invalid(format!("not 32 bytes: {value}")))?;
    if bytes.len() != 32 {
        return Err(CoreError::Invalid(format!("not 32 bytes: {value}")));
    }
    let mut word = [0u8; 32];
    word.copy_from_slice(&bytes);
    Ok(word)
}

/// The address that produced a 65-byte signature over `digest`.
///
/// This is what the contract does with `ecrecover`, done here so a worker can
/// refuse bad money before spending a GPU-minute on it rather than finding out
/// at settlement.
pub fn recover(digest: &[u8; 32], sig_hex: &str) -> Result<String> {
    let raw = sig_hex.strip_prefix("0x").unwrap_or(sig_hex);
    let bytes = hex::decode(raw).map_err(|e| CoreError::Signature(format!("bad signature: {e}")))?;
    if bytes.len() != 65 {
        return Err(CoreError::Signature(format!(
            "a signature is 65 bytes, this is {}",
            bytes.len()
        )));
    }
    // Wallets write v as 27/28; the recovery id is 0/1.
    let v = match bytes[64] {
        0 | 27 => 0u8,
        1 | 28 => 1u8,
        other => {
            return Err(CoreError::Signature(format!(
                "unexpected recovery byte {other}"
            )))
        }
    };
    let signature = Signature::from_slice(&bytes[..64])
        .map_err(|e| CoreError::Signature(format!("bad signature: {e}")))?;
    // Match the contract's `ecrecover`, which rejects a high-s (malleable)
    // signature and uses the recovery id exactly as given. If this helper
    // "repaired" a signature by normalizing s or flipping v, it would recover a
    // signer here that the contract — handed the raw bytes at settle — does not,
    // so the worker would do the work and never be paid. `normalize_s` returns
    // `Some` only when the input was high-s, which is exactly what to reject.
    if signature.normalize_s().is_some() {
        return Err(CoreError::Signature(
            "signature has a high-s value (malleable); the contract will reject it".into(),
        ));
    }
    let recovery =
        RecoveryId::from_byte(v).ok_or_else(|| CoreError::Signature("bad recovery id".into()))?;
    let key = VerifyingKey::recover_from_prehash(digest, &signature, recovery)
        .map_err(|e| CoreError::Signature(format!("cannot recover a signer: {e}")))?;
    Ok(address_of(&key))
}

/// The Ethereum address of a public key: last 20 bytes of the keccak of the
/// uncompressed key, without its leading tag.
pub fn address_of(key: &VerifyingKey) -> String {
    let point = key.to_encoded_point(false);
    let hash = keccak(&point.as_bytes()[1..]);
    format!("0x{}", hex::encode(&hash[12..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use k256::ecdsa::{signature::hazmat::PrehashSigner, SigningKey};

    /// A wallet, and the signing a wallet would do.
    fn wallet(seed: u8) -> (SigningKey, String) {
        let key = SigningKey::from_bytes(&[seed; 32].into()).unwrap();
        let address = address_of(key.verifying_key());
        (key, address)
    }

    fn sign(key: &SigningKey, digest: &[u8; 32]) -> String {
        let (sig, recovery) = key.sign_prehash(digest).unwrap();
        format!("0x{}{}", hex::encode(sig.to_bytes()), hex::encode([recovery.to_byte() + 27]))
    }

    fn domain() -> Domain {
        Domain::base("0x1234567890abcdef1234567890abcdef12345678")
    }

    fn auth(client: &str, cumulative: Micros) -> SpendingAuth {
        SpendingAuth {
            channel_id: channel_id("client-peer", "worker-peer", "salt"),
            client: client.into(),
            cumulative,
            metadata_hash: metadata_hash("llama-3.1-70b", 1000, 500, 0, "ba78"),
            sig: None,
        }
    }

    /// The digest, byte for byte, as `RootmodeChannels.vy` computes it.
    ///
    /// Generated by `contracts/test/Parity.t.sol`. If this fails, a worker is
    /// accepting authorisations the contract will never honour — which it
    /// would otherwise discover only after doing the work.
    #[test]
    fn the_digest_matches_the_contract() {
        let auth = SpendingAuth {
            channel_id: "0x0000000000000000000000000000000000000000000000000000000000000011".into(),
            client: "0x00000000000000000000000000000000000000a1".into(),
            cumulative: 2_730_000,
            metadata_hash: "0x0000000000000000000000000000000000000000000000000000000000000022".into(),
            sig: None,
        };
        let domain = Domain::base("0x1234567890abcdef1234567890abcdef12345678");
        assert_eq!(
            hex::encode(auth.digest(&domain).unwrap()),
            "740011ac087b2ac5dacb03f1d4b32eb8e7d59869b4a96ad2aedee46a4ae71abf"
        );
    }

    #[test]
    fn an_authorisation_recovers_the_address_that_signed_it() {
        let (key, address) = wallet(1);
        let mut a = auth(&address, 2_730_000);
        a.sig = Some(sign(&key, &a.digest(&domain()).unwrap()));

        // This is `ecrecover` in the contract, done early so a worker can
        // refuse bad money before spending a GPU-minute on it.
        assert_eq!(a.recover(&domain()).unwrap(), address);
        assert_eq!(a.check(&domain(), 2_000_000, 20_000_000).unwrap(), 730_000);
    }

    #[test]
    fn a_signature_from_someone_who_cannot_pay_is_refused() {
        let (thief, _) = wallet(2);
        let (_, victim) = wallet(3);
        // Signed by the thief, but naming the victim as the payer.
        let mut a = auth(&victim, 5_000_000);
        a.sig = Some(sign(&thief, &a.digest(&domain()).unwrap()));

        let err = a.check(&domain(), 0, 20_000_000).unwrap_err().to_string();
        assert!(err.contains("not by"), "{err}");
    }

    #[test]
    fn spending_can_only_go_up_and_only_to_what_was_reserved() {
        let (key, address) = wallet(4);
        let mut a = auth(&address, 1_000_000);
        a.sig = Some(sign(&key, &a.digest(&domain()).unwrap()));

        // Replaying an older authorisation pays nothing, which is what makes
        // it safe for a worker to keep only the newest.
        assert!(a.check(&domain(), 1_000_000, 20_000_000).is_err());
        assert!(a.check(&domain(), 2_000_000, 20_000_000).is_err());
        // And nothing beyond the session's reservation.
        assert!(a.check(&domain(), 0, 500_000).is_err());
    }

    #[test]
    fn an_authorisation_for_one_deployment_cannot_be_replayed_against_another() {
        let (key, address) = wallet(5);
        let mut a = auth(&address, 1_000_000);
        a.sig = Some(sign(&key, &a.digest(&domain()).unwrap()));

        // Same signature, different chain: a testnet authorisation must not
        // spend mainnet money.
        let testnet = Domain {
            chain_id: 84532,
            verifying_contract: domain().verifying_contract,
        };
        assert_ne!(a.recover(&testnet).unwrap(), address);

        // Same chain, different contract.
        let elsewhere = Domain::base("0x000000000000000000000000000000000000dead");
        assert_ne!(a.recover(&elsewhere).unwrap(), address);
    }

    #[test]
    fn changing_what_the_money_is_for_breaks_the_signature() {
        let (key, address) = wallet(6);
        let mut a = auth(&address, 1_000_000);
        a.sig = Some(sign(&key, &a.digest(&domain()).unwrap()));

        // The metadata hash commits to the model, the measured tokens and the
        // bytes delivered. Re-pointing a charge at different work is therefore
        // a different signature.
        let mut moved = a.clone();
        moved.metadata_hash = metadata_hash("llama-3.1-70b", 1000, 500, 0, "different");
        assert_ne!(moved.recover(&domain()).unwrap(), address);

        let mut inflated = a;
        inflated.cumulative = 999_000_000;
        assert_ne!(inflated.recover(&domain()).unwrap(), address);
    }

    #[test]
    fn a_reservation_is_checked_for_expiry_and_signer() {
        let (key, address) = wallet(7);
        let mut r = ReserveAuth {
            channel_id: channel_id("c", "w", "s"),
            client: address.clone(),
            worker_payout: "0x00000000000000000000000000000000000000ff".into(),
            max_amount: 20_000_000,
            deadline: 2_000,
            sig: None,
        };
        r.sig = Some(sign(&key, &r.digest(&domain()).unwrap()));

        assert!(r.verify(&domain(), 1_000).is_ok());
        // Past the deadline the money is the client's to reclaim, so a worker
        // must not start work against it.
        let err = r.verify(&domain(), 2_001).unwrap_err().to_string();
        assert!(err.contains("expired"), "{err}");
    }

    #[test]
    fn a_channel_is_specific_to_a_pair_and_a_session() {
        let a = channel_id("client", "worker", "monday");
        assert_eq!(a, channel_id("CLIENT", "WORKER", "monday"), "addresses are caseless");
        assert_ne!(a, channel_id("client", "worker", "tuesday"), "a new salt is a new ledger");
        assert_ne!(a, channel_id("client", "other-worker", "monday"));
        assert!(a.starts_with("0x") && a.len() == 66);
    }

    /// Generated by `contracts/test/RootmodePot.t.sol::test_print_digests`.
    #[test]
    fn the_spend_ticket_digest_matches_the_contract() {
        let ticket = SpendTicket {
            client: "0x00000000000000000000000000000000000000a1".into(),
            worker_payout: "0x00000000000000000000000000000000000000b0".into(),
            cumulative: 2_730_000,
            deadline: 1_700_000_000,
        };
        let domain = Domain::base("0x1234567890abcdef1234567890abcdef12345678");
        assert_eq!(
            hex::encode(ticket.digest(&domain).unwrap()),
            "9a758a5dc36ac9923b268e0e82c001fd8258f307aaaf2293d97b72c3e6544960"
        );
    }

    #[test]
    fn a_spend_ticket_recovers_the_app_key_that_signed_it() {
        let (key, address) = wallet(8);
        let ticket = SpendTicket {
            client: "0x00000000000000000000000000000000000000a1".into(),
            worker_payout: address.clone(),
            cumulative: 250_000,
            deadline: 2_000,
        };
        let sig = sign(&key, &ticket.digest(&domain()).unwrap());
        assert_eq!(recover(&ticket.digest(&domain()).unwrap(), &sig).unwrap(), address);
    }

    /// A spend ticket must be signed by the account's registered app key, not
    /// merely by *some* key. This is the check whose absence let any throwaway
    /// key authorize a priced job; if it regresses, this fails.
    #[test]
    fn a_spend_ticket_signed_by_the_wrong_key_is_refused() {
        let (app, app_addr) = wallet(20);
        let (thief, _) = wallet(21);
        let ticket = SpendTicket {
            client: "0x00000000000000000000000000000000000000a1".into(),
            worker_payout: "0x00000000000000000000000000000000000000b0".into(),
            cumulative: 250_000,
            deadline: 9_000_000_000,
        };
        // Signed by the thief, but the account's registered app key is `app`.
        let bad = sign(&thief, &ticket.digest(&domain()).unwrap());
        let err = ticket
            .check(&domain(), &bad, &app_addr, 1_000)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not by the account's app key"), "{err}");
        // The real app key's signature is accepted...
        let good = sign(&app, &ticket.digest(&domain()).unwrap());
        assert!(ticket.check(&domain(), &good, &app_addr, 1_000).is_ok());
        // ...but not once it has expired.
        assert!(ticket.check(&domain(), &good, &app_addr, 9_000_000_001).is_err());
    }
}

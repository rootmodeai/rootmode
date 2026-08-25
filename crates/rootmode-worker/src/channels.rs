//! What each client has authorised this node to spend, and how much of it is
//! already earned.
//!
//! One entry per channel, holding only the newest authorisation: they are
//! cumulative, so the latest supersedes everything before it and there is no
//! pile to keep. Losing one costs the jobs since the previous — which is why
//! it is written to disk as it arrives, not at shutdown.
//!
//! Nothing here talks to a chain. It is the ledger a worker settles *from*:
//! when it wants paying it submits the newest authorisation per channel, and
//! the contract transfers the difference against what it has already paid.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use rootmode_core::payments::{channel_id, Domain, Micros, SpendTicket, SpendingAuth};
use serde::{Deserialize, Serialize};

/// A channel as this worker knows it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Channel {
    pub channel_id: String,
    /// The paying address.
    pub client: String,
    /// The highest cumulative amount this client has signed for.
    pub authorised: Micros,
    /// What was reserved for the session, from the client's `ReserveAuth`.
    /// Until a reservation is seen this is what has been authorised so far —
    /// a worker with no reservation is trusting the run of authorisations it
    /// has actually been handed, which is the honest position.
    pub reserved: Micros,
    /// The newest authorisation, kept whole because settlement needs the
    /// signature, not just the number.
    pub latest: SpendingAuth,
    /// Newest pot SpendTicket, when the client paid via `job.pay`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend: Option<SpendTicket>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend_sig: Option<String>,
    /// What has been redeemed on-chain, once anything has.
    #[serde(default)]
    pub settled: Micros,
    pub updated_at: i64,
}

impl Channel {
    /// Earned and not yet paid out.
    pub fn owed(&self) -> Micros {
        self.authorised.saturating_sub(self.settled)
    }
}

/// The open channels, kept in memory and mirrored to a file.
pub struct Channels {
    path: PathBuf,
    open: RwLock<BTreeMap<String, Channel>>,
}

impl Channels {
    /// Load whatever a previous run left behind. A missing or unreadable file
    /// is an empty ledger, not a failure to start: a node that will not boot
    /// because of its billing file is a node that stops earning entirely.
    pub fn load(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let open = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Vec<Channel>>(&text).ok())
            .map(|list| {
                list.into_iter()
                    .map(|c| (c.channel_id.clone(), c))
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();
        if !open.is_empty() {
            tracing::info!("{} open payment channel(s)", open.len());
        }
        Self {
            path,
            open: RwLock::new(open),
        }
    }

    /// Check an authorisation and record it, returning what this job earns.
    ///
    /// Refusing here is refusing before the work: a GPU-minute spent against
    /// an authorisation that will not settle is a minute given away.
    pub fn accept(
        &self,
        auth: &SpendingAuth,
        domain: &Domain,
        now: i64,
    ) -> Result<Micros, String> {
        let mut open = self.open.write().unwrap_or_else(|e| e.into_inner());
        let existing = open.get(&auth.channel_id);
        let already = existing.map(|c| c.authorised).unwrap_or(0);
        // With no reservation on record, the client's own signature is the
        // ceiling: it cannot authorise less than it just authorised.
        let reserved = existing.map(|c| c.reserved).unwrap_or(auth.cumulative).max(auth.cumulative);

        let earned = auth
            .check(domain, already, reserved)
            .map_err(|e| e.to_string())?;

        // A channel belongs to the address that opened it. Letting a second
        // address raise the number on someone else's channel would be a way
        // to spend their reservation.
        if let Some(channel) = existing {
            if !channel.client.eq_ignore_ascii_case(&auth.client) {
                return Err(format!(
                    "this channel belongs to {}, not to {}",
                    channel.client, auth.client
                ));
            }
        }

        let updated = Channel {
            channel_id: auth.channel_id.clone(),
            client: auth.client.clone(),
            authorised: auth.cumulative,
            reserved,
            latest: auth.clone(),
            spend: existing.and_then(|c| c.spend.clone()),
            spend_sig: existing.and_then(|c| c.spend_sig.clone()),
            settled: existing.map(|c| c.settled).unwrap_or(0),
            updated_at: now,
        };
        open.insert(auth.channel_id.clone(), updated);
        self.write(&open);
        Ok(earned)
    }

    /// Bank a pot SpendTicket from `job.pay`. `expected_delta` is this job's
    /// invoice; the ticket must raise the cumulative by exactly that.
    pub fn accept_spend(
        &self,
        ticket: &SpendTicket,
        sig: &str,
        domain: &Domain,
        app_key: &str,
        expected_delta: Micros,
        now: i64,
    ) -> Result<Micros, String> {
        // The signer must be the account's on-chain app key — the key the pot
        // checks `settle` against. Recovering *a* signer is not enough: a ticket
        // signed by any other key can never be redeemed, so banking it would
        // only poison this channel's ledger.
        ticket
            .check(domain, sig, app_key, now.max(0) as u64)
            .map_err(|e| e.to_string())?;
        let id = channel_id(&ticket.client, &ticket.worker_payout, "pot");
        let mut open = self.open.write().unwrap_or_else(|e| e.into_inner());
        let existing = open.get(&id);
        if let Some(channel) = existing {
            if !channel.client.eq_ignore_ascii_case(&ticket.client) {
                return Err(format!(
                    "this channel belongs to {}, not to {}",
                    channel.client, ticket.client
                ));
            }
        }
        let already = existing.map(|c| c.authorised).unwrap_or(0);
        if ticket.cumulative < already.saturating_add(expected_delta) {
            return Err(format!(
                "ticket cumulative {} is less than {} + {expected_delta}",
                ticket.cumulative, already
            ));
        }
        if ticket.cumulative <= already {
            return Err(format!(
                "cumulative spend must rise: {} is not more than {already}",
                ticket.cumulative
            ));
        }
        // The rise above what this node last banked may exceed the invoice:
        // several nodes can share one payout channel (a fleet with one
        // treasury), and every settle any of them lands moves the client's
        // cumulative on without passing through this ledger. That excess is
        // not this job's earning — it is money already recognised on-chain —
        // so credit only the invoice. Refusing it was worse than either
        // error: nothing got banked, nothing settled, and the upstream bill
        // for the work was paid by nobody.
        let earned = expected_delta;

        let latest = existing
            .map(|c| c.latest.clone())
            .unwrap_or_else(|| SpendingAuth {
                channel_id: id.clone(),
                client: ticket.client.clone(),
                cumulative: ticket.cumulative,
                metadata_hash: format!("0x{}", hex::encode([0u8; 32])),
                sig: None,
            });
        let updated = Channel {
            channel_id: id.clone(),
            client: ticket.client.clone(),
            authorised: ticket.cumulative,
            reserved: existing
                .map(|c| c.reserved)
                .unwrap_or(ticket.cumulative)
                .max(ticket.cumulative),
            latest,
            spend: Some(ticket.clone()),
            spend_sig: Some(sig.to_string()),
            settled: existing.map(|c| c.settled).unwrap_or(0),
            updated_at: now,
        };
        open.insert(id, updated);
        self.write(&open);
        Ok(earned)
    }

    /// Highest cumulative already signed for this client/worker pair.
    pub fn authorised_for(&self, client: &str, worker: &str) -> Micros {
        let id = channel_id(client, worker, "pot");
        self.open
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .map(|c| c.authorised)
            .unwrap_or(0)
    }

    /// Everything this worker could redeem, newest authorisation per channel.
    pub fn redeemable(&self) -> Vec<Channel> {
        self.open
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|c| c.owed() > 0)
            .cloned()
            .collect()
    }

    /// What one channel has banked beyond what it has settled, in micros.
    pub fn owed_for(&self, channel_id: &str) -> Micros {
        self.open
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(channel_id)
            .map(|c| c.owed())
            .unwrap_or(0)
    }

    /// Total owed across every channel, in micros.
    pub fn owed(&self) -> Micros {
        self.open
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|c| c.owed())
            .sum()
    }

    /// Record what a settlement transaction actually paid.
    pub fn settled(&self, channel_id: &str, upto: Micros) {
        let mut open = self.open.write().unwrap_or_else(|e| e.into_inner());
        if let Some(channel) = open.get_mut(channel_id) {
            channel.settled = channel.settled.max(upto);
        }
        self.write(&open);
    }

    fn write(&self, open: &BTreeMap<String, Channel>) {
        let list: Vec<&Channel> = open.values().collect();
        let Ok(text) = serde_json::to_string_pretty(&list) else {
            return;
        };
        // Written whole and moved into place: a half-written ledger read back
        // after a crash would lose every channel, not one.
        let temp = self.path.with_extension("tmp");
        if std::fs::write(&temp, text).is_ok() {
            let _ = std::fs::rename(&temp, &self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k256::ecdsa::{signature::hazmat::PrehashSigner, SigningKey};
    use rootmode_core::payments::{address_of, channel_id, metadata_hash};

    fn domain() -> Domain {
        Domain::base("0x1234567890abcdef1234567890abcdef12345678")
    }

    fn signed(key: &SigningKey, client: &str, cumulative: Micros) -> SpendingAuth {
        let mut auth = SpendingAuth {
            channel_id: channel_id("client", "worker", "salt"),
            client: client.into(),
            cumulative,
            metadata_hash: metadata_hash("m", 1, 1, 0, "abc"),
            sig: None,
        };
        let digest = auth.digest(&domain()).unwrap();
        let (sig, recovery) = key.sign_prehash(&digest).unwrap();
        auth.sig = Some(format!(
            "0x{}{}",
            hex::encode(sig.to_bytes()),
            hex::encode([recovery.to_byte() + 27])
        ));
        auth
    }

    fn temp() -> PathBuf {
        std::env::temp_dir().join(format!("rootmode-channels-{}.json", uuid::Uuid::new_v4()))
    }

    #[test]
    fn each_authorisation_earns_only_the_difference() {
        let key = SigningKey::from_bytes(&[9u8; 32].into()).unwrap();
        let client = address_of(key.verifying_key());
        let path = temp();
        let channels = Channels::load(&path);

        assert_eq!(channels.accept(&signed(&key, &client, 1_000_000), &domain(), 1).unwrap(), 1_000_000);
        // The second job is billed for what it added, not for the total.
        assert_eq!(channels.accept(&signed(&key, &client, 1_500_000), &domain(), 2).unwrap(), 500_000);
        assert_eq!(channels.owed(), 1_500_000);

        // Replaying the older one pays nothing and is refused outright.
        assert!(channels.accept(&signed(&key, &client, 1_000_000), &domain(), 3).is_err());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_ledger_survives_a_restart() {
        let key = SigningKey::from_bytes(&[10u8; 32].into()).unwrap();
        let client = address_of(key.verifying_key());
        let path = temp();

        let channels = Channels::load(&path);
        channels.accept(&signed(&key, &client, 2_000_000), &domain(), 1).unwrap();
        drop(channels);

        // A worker that forgets what it is owed has worked for free.
        let reopened = Channels::load(&path);
        assert_eq!(reopened.owed(), 2_000_000);
        assert_eq!(reopened.redeemable().len(), 1);
        assert!(reopened.redeemable()[0].latest.sig.is_some(), "the signature is what settles");

        // And what has been paid is not owed twice.
        reopened.settled(&channel_id("client", "worker", "salt"), 2_000_000);
        assert_eq!(reopened.owed(), 0);
        assert!(reopened.redeemable().is_empty());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn one_address_cannot_spend_anothers_channel() {
        let owner = SigningKey::from_bytes(&[11u8; 32].into()).unwrap();
        let other = SigningKey::from_bytes(&[12u8; 32].into()).unwrap();
        let (owner_addr, other_addr) = (
            address_of(owner.verifying_key()),
            address_of(other.verifying_key()),
        );
        let path = temp();
        let channels = Channels::load(&path);
        channels.accept(&signed(&owner, &owner_addr, 1_000_000), &domain(), 1).unwrap();

        // Properly signed, by the wrong person, on somebody else's channel.
        let err = channels
            .accept(&signed(&other, &other_addr, 5_000_000), &domain(), 2)
            .unwrap_err();
        assert!(err.contains("belongs to"), "{err}");
        assert_eq!(channels.owed(), 1_000_000, "the owner's balance is untouched");
        std::fs::remove_file(&path).ok();
    }
}

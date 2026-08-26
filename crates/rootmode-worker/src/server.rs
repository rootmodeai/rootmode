//! The worker's front door: a WebSocket server speaking RootmodeProtocol v1.
//!
//! One connection may carry several jobs. Each job runs on its own task and
//! writes through a shared sink, so a slow render never blocks the status
//! updates of another job on the same socket.
//!
//! This is the layer a DHT/gossip transport would sit beside, not replace:
//! everything below `handle_submit` deals in protocol messages and knows
//! nothing about how the bytes arrived.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rootmode_core::{
    protocol::{ClientMessage, JobInvoice, JobPay},
    Identity, JobPayload, JobResult, JobStatus, JobStatusUpdate, JobSubmit, ModelDescriptor,
    PeerAnnounce, Price, TokenUsage, WorkerMessage, PROTOCOL_VERSION,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Semaphore};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use crate::backends::{Progress, Registry, TokenDelta};
use crate::config::Config;
use crate::error::{Result, WorkerError};
use crate::stats::{self, Meter, Report};
use crate::channels::Channels;

/// A country code fit to put on the wire, or nothing.
///
/// Two letters, upper-cased, and that is the whole of the checking: this is a
/// claim by the operator, not a fact, and pretending otherwise by validating
/// it against a list would only make it look verified. Anything that is not
/// two letters is dropped rather than shown — "wherever I feel like" is not a
/// country, and a client should display nothing instead of nonsense.
fn normalise_country(raw: &str) -> Option<String> {
    let code = raw.trim();
    (code.len() == 2 && code.chars().all(|c| c.is_ascii_alphabetic()))
        .then(|| code.to_ascii_uppercase())
}

/// A payout address fit to advertise, or nothing.
///
/// Shape-checked only — `0x` and forty hex characters. Whether it is a
/// contract, an EOA, or a typo'd address belonging to nobody is between the
/// operator and the chain; this refuses only what is obviously not an address,
/// so a client is never shown half of one.
fn payout_of(raw: &str) -> Option<String> {
    let address = raw.trim();
    let looks_right = address.len() == 42
        && address.starts_with("0x")
        && address[2..].chars().all(|c| c.is_ascii_hexdigit());
    looks_right.then(|| address.to_string())
}

/// Caps on what untrusted peers can make this node hold at once, independent
/// of the GPU semaphore (which only gates `backend.run`, long after a frame is
/// buffered and verified). Generous enough never to touch honest use; low
/// enough that a flood of large submits cannot exhaust memory or file
/// descriptors before any admission gate.
const MAX_CONNECTIONS: usize = 512;
const MAX_INFLIGHT_JOBS: usize = 256;
/// Settle only once a channel has this much unsettled, or a ticket is about
/// to expire. A settle on Base costs ~$0.0014 in gas; a job that earned
/// $0.00002 must not spend seventy times its revenue collecting it. Tickets
/// are cumulative, so waiting loses nothing as long as the newest is settled
/// before its deadline — [`Worker::settle_due`] sweeps for exactly that.
const SETTLE_MIN_MICROS: u64 = 50_000;
const SETTLE_BEFORE_DEADLINE_SECS: u64 = 20 * 60;

pub struct Worker {
    config: Config,
    identity: Identity,
    registry: Registry,
    /// Bounds concurrent jobs across every connection, because the GPU is
    /// shared whether or not the clients know about each other.
    permits: Arc<Semaphore>,
    /// Bounds concurrent inbound connections, so one peer cannot open a
    /// thousand sockets and exhaust descriptors/memory before any job runs.
    conn_permits: Arc<Semaphore>,
    /// Bounds jobs being parsed, verified and queued at once — the work before
    /// a GPU permit — capping the memory a flood of large submits can pin.
    job_permits: Arc<Semaphore>,
    /// What this node has served since the last report. Always counted;
    /// only sent anywhere when the operator configured a collector.
    meter: Meter,
    /// What clients have authorised this node to be paid, per channel.
    channels: Arc<Channels>,
    /// A job in flight can be told to stop. Entered when its task is spawned,
    /// removed the moment it ends for any reason — so this is only ever the
    /// jobs actually running or queued right now, never a growing history.
    cancellations: Mutex<HashMap<Uuid, Arc<tokio::sync::Notify>>>,
    /// Priced jobs waiting for `job.pay` (actual capture of a prepaid chunk).
    pending_pays: Mutex<HashMap<Uuid, tokio::sync::oneshot::Sender<JobPay>>>,
    /// Prepaid 1M-token chunks, held until actual capture or timeout.
    pending_bonds: Mutex<HashMap<Uuid, Bond>>,
    /// Test-only override for the on-chain channel read, so the priced-flow
    /// tests can exercise streaming and capture without a live RPC while the
    /// production path still goes to the chain.
    #[cfg(test)]
    test_channel: Mutex<Option<crate::chain::ChannelState>>,
}

struct Bond {
    pay: JobPay,
    delta: u64,
    /// The account's on-chain app key, read once when the bond is admitted.
    /// Every later ticket on this job (top-up, capture) must be signed by it,
    /// checked against this copy so no extra RPC round-trip is needed.
    app_key: String,
}

/// Unix seconds now, saturating to 0 before the epoch.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Removes a job from the cancellable set the moment it stops existing —
/// whichever of `handle_submit_cancellable`'s many return points that turns
/// out to be. Without this, a job that fails early (a bad signature, a
/// screened prompt) would sit in the map forever: reachable by a `job.cancel`
/// that arrives too late to mean anything, and never cleaned up.
struct ForgetOnDrop<'a> {
    worker: &'a Worker,
    job_id: Uuid,
}

impl Drop for ForgetOnDrop<'_> {
    fn drop(&mut self) {
        self.worker
            .cancellations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.job_id);
    }
}

impl Worker {
    pub fn new(config: Config, identity: Identity, registry: Registry) -> Self {
        let permits = Arc::new(Semaphore::new(config.worker.max_concurrent as usize));
        Self {
            identity,
            registry,
            channels: Arc::new(Channels::load(&config.payments.channels_file)),
            permits,
            conn_permits: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            job_permits: Arc::new(Semaphore::new(MAX_INFLIGHT_JOBS)),
            meter: Meter::new(),
            cancellations: Mutex::new(HashMap::new()),
            pending_pays: Mutex::new(HashMap::new()),
            pending_bonds: Mutex::new(HashMap::new()),
            #[cfg(test)]
            test_channel: Mutex::new(None),
            config,
        }
    }

    /// Read the payer's channel: its remaining lock and the on-chain app key.
    /// In test builds an injected value stands in for the RPC so the priced
    /// flow can be exercised in-process.
    async fn read_channel(
        &self,
        payer: &str,
        payout: &str,
    ) -> Result<Option<crate::chain::ChannelState>> {
        #[cfg(test)]
        if let Some(state) = self
            .test_channel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Ok(Some(state));
        }
        crate::chain::channel_state(&self.config.payments, payer, payout).await
    }

    #[cfg(test)]
    fn set_test_channel(&self, remaining: u64, app_key: &str) {
        *self.test_channel.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(crate::chain::ChannelState {
                remaining,
                reserved: remaining,
                earned: 0,
                max_per_job: 0,
                app_key: app_key.to_string(),
            });
    }

    /// Build everything from a config file: identity, backends, model list.
    pub async fn from_config(config: Config) -> Result<Self> {
        let identity = rootmode_core::keyfile::load_or_create(&config.worker.identity_file)?;
        let registry = Registry::build(&config.backends).await?;
        Ok(Self::new(config, identity, registry))
    }

    pub fn peer_id(&self) -> String {
        self.identity.peer_id()
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn models(&self) -> Vec<ModelDescriptor> {
        self.registry.models()
    }

    /// What this node tells the network it can do. A DHT publisher would
    /// advertise exactly this record.
    pub fn announce(&self) -> PeerAnnounce {
        PeerAnnounce {
            v: PROTOCOL_VERSION,
            peer_id: self.peer_id(),
            label: Some(self.config.worker.label.clone()),
            country: normalise_country(&self.config.worker.country),
            caps: self.registry.caps(),
            models: self.registry.models(),
            max_concurrent: self.config.worker.max_concurrent,
            payout: payout_of(&self.config.worker.payout_address),
        }
    }

    /// Bank a legacy spend-on-submit authorisation, if one arrived.
    ///
    /// Priced jobs no longer rely on this: they take a 1M-token chunk on
    /// submit, stream against it, and capture the actual bill afterwards.
    /// `require_auth` still refuses a free-looking job that carries neither
    /// a spend nor a chunk, so an operator who turned billing on is not
    /// quietly serving for free.
    fn take_payment(
        &self,
        submit: &JobSubmit,
        holdback: bool,
    ) -> std::result::Result<(), WorkerError> {
        let Some(domain) = self.config.payments.domain() else {
            return Ok(()); // Not charging. Anything attached is ignored.
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        match &submit.spend {
            Some(auth) => {
                let earned = self
                    .channels
                    .accept(auth, &domain, now)
                    .map_err(|e| WorkerError::Rejected(format!("payment authorisation: {e}")))?;
                tracing::info!(
                    channel = %auth.channel_id,
                    earned_micros = earned,
                    owed_micros = self.channels.owed(),
                    "authorised"
                );
                Ok(())
            }
            None if self.config.payments.require_auth && !holdback => {
                // No ticket, and none was due: the node lists this model at
                // no charge, so there is nothing to sign for. Only a model
                // nobody here priced is refused — an operator who turned
                // billing on never serves for free what they did not list.
                match self.listed_price(&submit.payload) {
                    Some(price) if price.is_free() => Ok(()),
                    _ => Err(WorkerError::Rejected(
                        "this node requires a signed spending authorisation with each job".into(),
                    )),
                }
            }
            None => Ok(()),
        }
    }

    /// True when this job must be prepaid in 1M-token chunks before streaming.
    fn holdback_for(&self, payload: &JobPayload) -> bool {
        self.config.payments.domain().is_some() && !self.advertised_price(payload).is_free()
    }

    /// Bank the prepaid chunk. Returns its delta in micros.
    fn take_bond(&self, submit: &JobSubmit) -> std::result::Result<u64, WorkerError> {
        let Some(bond) = submit.bond.clone() else {
            return Err(WorkerError::Rejected(
                "priced jobs need a signed 1M-token chunk before work".into(),
            ));
        };
        if bond.job_id != submit.job_id {
            return Err(WorkerError::Rejected("chunk ticket is for a different job".into()));
        }
        let domain = self
            .config
            .payments
            .domain()
            .ok_or_else(|| WorkerError::Rejected("this node has no settlement contract".into()))?;
        bond.ticket
            .recover(&domain, &bond.sig)
            .map_err(|e| WorkerError::Rejected(format!("chunk ticket: {e}")))?;
        if let Some(payout) = payout_of(&self.config.worker.payout_address) {
            if !bond.ticket.worker_payout.eq_ignore_ascii_case(&payout) {
                return Err(WorkerError::Rejected("chunk ticket is for a different worker".into()));
            }
        }
        if let Some(payer) = submit.payer.as_deref() {
            if !bond.ticket.client.eq_ignore_ascii_case(payer) {
                return Err(WorkerError::Rejected("chunk ticket is for a different payer".into()));
            }
        }
        let already = self
            .channels
            .authorised_for(&bond.ticket.client, &bond.ticket.worker_payout);
        if bond.ticket.cumulative <= already {
            return Err(WorkerError::Rejected(
                "chunk ticket does not raise the authorised spend".into(),
            ));
        }
        let delta = bond.ticket.cumulative - already;
        self.pending_bonds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                submit.job_id,
                Bond {
                    pay: bond,
                    delta,
                    // Filled by `ensure_lock` once it has read the on-chain key.
                    app_key: String::new(),
                },
            );
        Ok(delta)
    }

    /// Bound generation to what is already prepaid. Mid-stream top-ups raise
    /// the prepaid amount; this is only the starting ceiling.
    fn clamp_to_chunk(&self, payload: JobPayload, bond_micros: u64) -> std::result::Result<JobPayload, WorkerError> {
        let price = self.advertised_price(&payload);
        let JobPayload::Llm(mut params) = payload else {
            return Ok(payload);
        };
        let prompt = TokenUsage::measure(&params, None, None, &[]).prompt;
        // Price the prompt at the input rate and the answer at the output
        // rate — the way the bill is computed — rather than everything at
        // the dearest rate. On a model whose output costs ten times its
        // input, the old arithmetic refused prompts that cost a few cents
        // as if they cost the whole lock. Cache hits are unknown here and
        // billed as fresh, which only ever leaves more room than expected.
        let (input, output, _cache, cache_write) = price.llm_rates();
        let fresh = input.max(cache_write);
        let prompt_micros = (prompt as f64 * fresh).ceil() as u64;
        if prompt_micros >= bond_micros {
            return Err(WorkerError::Rejected(format!(
                "this prompt is {prompt} tokens (about ${:.2} at this model's input rate), more than \
                 your ${:.2} limit for a single job",
                prompt_micros as f64 / 1_000_000.0,
                bond_micros as f64 / 1_000_000.0
            )));
        }
        let room = if output > 0.0 {
            ((bond_micros - prompt_micros) as f64 / output).floor() as u64
        } else {
            u64::MAX
        };
        params.max_tokens = params.max_tokens.min(room.max(1).min(u32::MAX as u64) as u32);
        Ok(JobPayload::Llm(params))
    }

    fn advertised_price(&self, payload: &JobPayload) -> Price {
        self.listed_price(payload).unwrap_or_default()
    }

    /// The price this node lists for the job's model — `None` when the
    /// model is not in the catalogue at all (an unpriced model is `Some`
    /// of a free price; that distinction is what `require_auth` turns on).
    fn listed_price(&self, payload: &JobPayload) -> Option<Price> {
        let model = match payload {
            JobPayload::Llm(p) => p.model_id.as_deref().or(p.model_hash.as_deref()),
            JobPayload::Image(p) => p.checkpoint_id.as_deref().or(p.model_hash.as_deref()),
            JobPayload::Video(p) => p.checkpoint_id.as_deref().or(p.model_hash.as_deref()),
        };
        let models = self.registry.models();
        if let Some(model) = model {
            if let Some(found) = models
                .iter()
                .find(|m| m.id == model || model.starts_with(&m.id) || m.id.starts_with(model))
            {
                return Some(found.price.clone().unwrap_or_default().round_protocol());
            }
        }
        if models.len() == 1 {
            return Some(models[0].price.clone().unwrap_or_default().round_protocol());
        }
        None
    }

    fn bill_micros(&self, payload: &JobPayload, result: &JobResult) -> u64 {
        let price = self.price_of(result);
        if price.is_free() {
            return 0;
        }
        match result.kind {
            rootmode_core::JobKind::Llm => {
                let local = match payload {
                    JobPayload::Llm(p) => TokenUsage::measure(
                        p,
                        result.text.as_deref(),
                        result.thinking.as_deref(),
                        &result.tool_calls,
                    ),
                    _ => TokenUsage::default(),
                };
                let usage = local.reconcile(TokenUsage::from_meta(&result.meta));
                let by_rate = price.charge_llm_micros(usage.prompt, usage.completion, usage.cached);
                // A backend that knows what the job actually cost sets a
                // floor — see the OpenRouter backend. The rate table is the
                // advertised price; the floor is the guarantee of margin.
                let floor = result
                    .meta
                    .get("min_bill_micros")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                by_rate.max(floor)
            }
            rootmode_core::JobKind::Image | rootmode_core::JobKind::Video => {
                let flat = (price.amount * 1_000_000.0).round().max(1.0) as u64;
                // A backend that knows the picture's actual cost bills that
                // plus margin — the advertised price is the ceiling the
                // client locked, and a picture that cost less is billed less.
                match result.meta.get("min_bill_micros").and_then(|v| v.as_u64()) {
                    Some(floor) => floor.min(flat).max(1),
                    None => flat,
                }
            }
        }
    }

    /// Confirm, before any GPU time, that this priced job will actually pay:
    /// the payer still has unused lock, and the prepaid ticket is signed by the
    /// account's on-chain app key — the key the contract checks `settle`
    /// against. A ticket signed by anyone else settles to nothing, so serving
    /// it is free work. The app key is stashed on the bond so the later top-up
    /// and capture tickets are held to the same bar.
    ///
    /// Fails closed: a priced node that cannot read the chain (no RPC, or a
    /// transient RPC error) cannot verify payment and so must refuse, rather
    /// than serve for free.
    async fn ensure_lock(&self, submit: &JobSubmit) -> std::result::Result<(), WorkerError> {
        let Some(payout) = payout_of(&self.config.worker.payout_address) else {
            return Ok(());
        };
        let Some(payer) = submit.payer.as_deref() else {
            return Err(WorkerError::Rejected(
                "priced jobs need a payer address so this node can check the on-chain lock".into(),
            ));
        };
        let domain = self
            .config
            .payments
            .domain()
            .ok_or_else(|| WorkerError::Rejected("this node has no settlement contract".into()))?;

        let mut state = match self.read_channel(payer, &payout).await {
            Ok(Some(state)) => state,
            Ok(None) => {
                return Err(WorkerError::Rejected(
                    "this node cannot verify payment without an RPC (set payments.rpc); \
                     priced work refused"
                        .into(),
                ));
            }
            // A configured RPC that errored is transient — fail closed rather
            // than serve a priced job we cannot check.
            Err(e) => return Err(e),
        };
        // How much lock this job needs is measured from what the chain has
        // already recognised, not from this node's own ledger. Every seed
        // shares one payout channel, so other nodes settle on it between our
        // jobs; a delta taken against a stale local ledger grows with each
        // of those settles until no reserve is ever "enough".
        let need = self
            .pending_bonds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&submit.job_id)
            .map(|b| b.pay.ticket.cumulative.saturating_sub(state.earned).max(1))
            .unwrap_or(1);
        tracing::info!(
            remaining = state.remaining,
            need,
            has_reserve = submit.reserve.is_some(),
            "priced lock check"
        );
        // A picture or clip is one fixed bill at its advertised price. If
        // that single bill is above the per-job cap snapshotted into the
        // payer's channel, the settle would revert OverCap — the work done
        // and the cost borne, nothing collected — so refuse it here, before
        // the backend is called. Text is NOT checked this way: its bond is a
        // prepaid 1M-token chunk and its ticket is cumulative across a shared
        // channel, so `need` is neither the reply's cost nor a single settle;
        // the reply meters in small per-token deltas the contract gates on
        // its own, and lumping it in here refused ordinary chats with a
        // wild figure.
        if state.max_per_job > 0 && submit.payload.kind() != rootmode_core::JobKind::Llm {
            let flat = (self.advertised_price(&submit.payload).amount * 1_000_000.0).round() as u64;
            if flat > state.max_per_job {
                return Err(WorkerError::Rejected(format!(
                    "this {} costs ${:.2} but the payer's channel allows ${:.2} per job; nothing above that can settle. \
                     Raise the limit in your pot and reopen the channel",
                    if submit.payload.kind() == rootmode_core::JobKind::Video { "clip" } else { "picture" },
                    flat as f64 / 1_000_000.0,
                    state.max_per_job as f64 / 1_000_000.0
                )));
            }
        }
        // Post a raise whenever the payer signed one above the current lock,
        // not only once the lock is already short. The client tops up with
        // headroom for several jobs; waiting until a job cannot fit means
        // every concurrent request at the margin is refused first.
        if let Some(post) = submit.reserve.as_ref() {
            if !post.ticket.worker_payout.eq_ignore_ascii_case(&payout) {
                return Err(WorkerError::Rejected(
                    "reserve ticket is for a different payout address".into(),
                ));
            }
            if !post.ticket.client.eq_ignore_ascii_case(payer) {
                return Err(WorkerError::Rejected(
                    "reserve ticket is for a different payer".into(),
                ));
            }
            let sig = hex::decode(post.sig.trim_start_matches("0x"))
                .map_err(|e| WorkerError::Rejected(format!("reserve signature: {e}")))?;
            if post.ticket.max_amount <= state.reserved && state.remaining < need {
                // The chain already holds a lock at or above what this job
                // signed for, yet it is short: another job's raise landed
                // between the client's read and ours, and its settle may be
                // consuming it, or a bigger raise is in flight. Give the
                // chain a few seconds before calling the lock empty.
                let mut waited = 0;
                while state.remaining < need && waited < 8 {
                    tokio::time::sleep(Duration::from_millis(750)).await;
                    waited += 1;
                    if let Ok(Some(s)) = self.read_channel(payer, &payout).await {
                        state = s;
                    }
                }
            }
            if post.ticket.max_amount > state.reserved {
                let posted = crate::chain::reserve(&self.config.payments, &post.ticket, &sig).await;
                state = match self.read_channel(payer, &payout).await {
                    Ok(Some(s)) => s,
                    Ok(None) => {
                        return Err(WorkerError::Rejected(
                            "reserve posted but the channel could not be read back".into(),
                        ));
                    }
                    Err(e) => return Err(e),
                };
                if let Err(e) = posted {
                    // Concurrent jobs at the margin all post the same raise;
                    // one lands and the rest revert NotMonotonic. Give the
                    // winner's transaction a few seconds to be mined before
                    // deciding the lock really is short.
                    let mut waited = 0;
                    while state.remaining < need && waited < 8 {
                        tokio::time::sleep(Duration::from_millis(750)).await;
                        waited += 1;
                        if let Ok(Some(s)) = self.read_channel(payer, &payout).await {
                            state = s;
                        }
                    }
                    if state.remaining < need {
                        return Err(WorkerError::Rejected(format!(
                            "could not post reserve: {e}"
                        )));
                    }
                }
            }
        }
        if state.remaining < need {
            return Err(WorkerError::Rejected(if submit.reserve.is_none() {
                "priced job is missing a reserve ticket".into()
            } else {
                "no remaining reserve on this channel; lock funds before sending work".into()
            }));
        }
        if crate::chain::is_zero_address(&state.app_key) {
            return Err(WorkerError::Rejected(
                "no app key is registered on-chain for this payer".into(),
            ));
        }
        if let Some(bond) = submit.bond.as_ref() {
            bond.ticket
                .check(&domain, &bond.sig, &state.app_key, now_secs())
                .map_err(|e| WorkerError::Rejected(format!("chunk ticket: {e}")))?;
        }
        if let Some(entry) = self
            .pending_bonds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&submit.job_id)
        {
            entry.app_key = state.app_key;
            // What this job earns is the rise above whichever is higher: the
            // last ticket this node banked, or what the chain shows settled
            // by anyone on this channel. Crediting the gap other nodes already
            // earned would overstate this job's bill.
            let above_chain = entry.pay.ticket.cumulative.saturating_sub(state.earned).max(1);
            entry.delta = entry.delta.min(above_chain);
        }
        Ok(())
    }

    /// The open payment channels, for settlement and for the operator's screen.
    pub fn channels(&self) -> &Arc<Channels> {
        &self.channels
    }

    /// What this node charges for whatever produced `result`.
    fn price_of(&self, result: &JobResult) -> rootmode_core::Price {
        let Some(model) = result.meta.get("model").and_then(|m| m.as_str()) else {
            return rootmode_core::Price::default();
        };
        self.registry
            .models()
            .iter()
            .find(|m| m.id == model)
            .and_then(|m| m.price.clone())
            .unwrap_or_default()
    }

    /// A signed account of what this node served since the last one.
    fn report(&self, counters: stats::Counters, window_secs: u64) -> Result<Report> {
        let models = self.registry.models();
        let currency = models
            .iter()
            .find_map(|m| m.price.as_ref().map(|p| p.currency.clone()))
            .unwrap_or_else(|| "USD".into());

        Report {
            v: PROTOCOL_VERSION,
            peer_id: self.peer_id(),
            label: self.config.worker.label.clone(),
            country: normalise_country(&self.config.worker.country),
            caps: self.registry.caps(),
            models: models.into_iter().map(|m| m.id).collect(),
            window_secs,
            counters,
            currency,
            sig: None,
        }
        .signed_by(&self.identity)
        .map_err(|e| WorkerError::Net(format!("cannot sign stats report: {e}")))
    }

    /// Post accumulated counters until `shutdown`. Started by `run` only when
    /// a collector is configured; a worker with no `[stats] url` never opens
    /// this connection at all.
    pub async fn report_stats(self: Arc<Self>, shutdown: impl std::future::Future<Output = ()>) {
        let url = self.config.stats.url.trim().to_string();
        let every = self.config.stats.interval_secs.max(30);
        let http = reqwest::Client::new();
        tracing::info!("reporting usage to {url} every {every}s");

        tokio::pin!(shutdown);
        let mut ticker = tokio::time::interval(Duration::from_secs(every));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await; // fires immediately; the first window is not up yet

        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = &mut shutdown => return,
            }

            let counters = self.meter.drain();
            let idle = stats::nothing_happened(&counters);
            let report = match self.report(counters.clone(), every) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("{e}");
                    continue;
                }
            };
            if let Err(e) = stats::send(&http, &url, &report).await {
                // Keep the numbers rather than the schedule: they go out with
                // the next window instead of leaving a hole in the chart.
                if !idle {
                    self.meter.restore(counters);
                }
                tracing::debug!("stats report failed ({e}); will retry");
            }
        }
    }

    pub async fn bind(&self) -> Result<TcpListener> {
        TcpListener::bind(&self.config.worker.listen)
            .await
            .map_err(|e| {
                WorkerError::Net(format!(
                    "cannot listen on {}: {e}",
                    self.config.worker.listen
                ))
            })
    }

    /// Accept until `shutdown` resolves.
    pub async fn serve(
        self: Arc<Self>,
        listener: TcpListener,
        shutdown: impl std::future::Future<Output = ()> + Send,
    ) -> Result<()> {
        tokio::pin!(shutdown);

        // Poll the backends so a model loaded after boot becomes servable
        // without a restart. `tokio::time::interval` fires immediately the
        // first time, which would just repeat the discovery `build` did.
        let refresh = self.config.worker.refresh_secs;
        let mut ticker = (refresh > 0).then(|| {
            let mut t = tokio::time::interval(std::time::Duration::from_secs(refresh));
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            t
        });

        loop {
            tokio::select! {
                _ = async {
                    match ticker.as_mut() {
                        Some(t) => { t.tick().await; }
                        // Nothing to poll: park this branch forever rather
                        // than spinning the select loop.
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    if self.registry.refresh().await {
                        tracing::info!(
                            "now serving: {}",
                            self.models()
                                .iter()
                                .map(|m| m.id.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                }
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, addr)) => {
                            match self.conn_permits.clone().try_acquire_owned() {
                                Ok(permit) => {
                                    let worker = self.clone();
                                    tokio::spawn(async move {
                                        let _permit = permit;
                                        if let Err(e) = worker.serve_connection(stream).await {
                                            tracing::info!(%addr, "connection ended: {e}");
                                        }
                                    });
                                }
                                // At the ceiling: drop the stream (closing it)
                                // rather than pile on more concurrent work.
                                Err(_) => tracing::warn!(%addr, "connection limit reached; refusing"),
                            }
                        }
                        Err(e) => tracing::warn!("accept failed: {e}"),
                    }
                }
                _ = &mut shutdown => {
                    tracing::info!("shutting down");
                    return Ok(());
                }
            }
        }
    }

    async fn serve_connection(self: Arc<Self>, stream: TcpStream) -> Result<()> {
        let ws = tokio_tungstenite::accept_async(stream)
            .await
            .map_err(|e| WorkerError::Net(e.to_string()))?;
        let (mut sink, mut source) = ws.split();

        // One writer owns the socket; jobs queue messages to it.
        let (tx, mut rx) = mpsc::unbounded_channel::<WorkerMessage>();
        let writer = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                let Ok(text) = serde_json::to_string(&msg) else {
                    continue;
                };
                if sink.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        // Announce first: a client that only wants to know what we are can
        // read one frame and hang up.
        let _ = tx.send(WorkerMessage::PeerAnnounce(self.announce()));

        while let Some(frame) = source.next().await {
            let frame = match frame {
                Ok(f) => f,
                Err(e) => {
                    tracing::debug!("read error: {e}");
                    break;
                }
            };
            match frame {
                Message::Text(text) => self.on_line(&text, &tx),
                Message::Close(_) => break,
                _ => continue,
            }
        }

        drop(tx);
        let _ = writer.await;
        Ok(())
    }

    /// Serve one client over a libp2p stream. Same protocol, same handling —
    /// only the pipe is different.
    pub async fn serve_stream(self: Arc<Self>, stream: rootmode_p2p::Stream) {
        let mut json = rootmode_p2p::JsonStream::new(stream);
        let (tx, mut rx) = mpsc::unbounded_channel::<WorkerMessage>();

        if json
            .send(&WorkerMessage::PeerAnnounce(self.announce()))
            .await
            .is_err()
        {
            return;
        }

        loop {
            tokio::select! {
                outbound = rx.recv() => match outbound {
                    Some(msg) => {
                        if let Err(e) = json.send(&msg).await {
                            tracing::debug!("write failed: {e}");
                            break;
                        }
                    }
                    None => break,
                },
                inbound = json.recv() => match inbound {
                    Ok(Some(line)) => self.on_line(&line, &tx),
                    Ok(None) => break,
                    Err(e) => {
                        tracing::debug!("read failed: {e}");
                        break;
                    }
                },
            }
        }

        json.close().await;
    }

    /// One inbound line of protocol, whatever carried it.
    fn on_line(self: &Arc<Self>, text: &str, tx: &mpsc::UnboundedSender<WorkerMessage>) {
        match ClientMessage::parse(text) {
            Ok(ClientMessage::PeerHello(hello)) => {
                tracing::info!(peer = %hello.peer_id, "client said hello");
            }
            Ok(ClientMessage::JobSubmit(submit)) => {
                let job_id = submit.job_id;
                // Bound in-flight jobs before retaining the (up to 64 MiB) raw
                // frame or spawning: a flood cannot pin unbounded memory.
                let Ok(permit) = self.job_permits.clone().try_acquire_owned() else {
                    tracing::warn!(%job_id, "in-flight job limit reached; refusing");
                    send_failed(tx, job_id, "worker is busy; too many jobs in flight");
                    return;
                };
                let worker = self.clone();
                let tx = tx.clone();
                let raw = text.to_string();
                let notify = Arc::new(tokio::sync::Notify::new());
                self.cancellations
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(job_id, notify.clone());
                tokio::spawn(async move {
                    let _permit = permit;
                    worker
                        .handle_submit_cancellable(submit, tx, notify, Some(raw))
                        .await
                });
            }
            Ok(ClientMessage::JobPay(pay)) => {
                if let Some(wait) = self
                    .pending_pays
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&pay.job_id)
                {
                    let _ = wait.send(pay);
                }
            }
            Ok(ClientMessage::JobCancel(cancel)) => {
                // Notified, not removed — the running task owns removing
                // itself, from whichever branch it actually stops in. Doing it
                // here too would race a job that finishes at the same moment
                // its cancellation arrives.
                if let Some(notify) = self
                    .cancellations
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&cancel.job_id)
                {
                    notify.notify_waiters();
                }
            }
            Ok(ClientMessage::Unknown) => tracing::debug!("ignoring unknown message type"),
            // A malformed frame is the client's problem; the connection stays
            // up so its other jobs are unaffected.
            Err(e) => tracing::debug!("ignoring unparseable frame: {e}"),
        }
    }

    /// Everything from here down is transport-agnostic.
    /// Run a job with no way to stop it early — the shape every caller wants
    /// except the socket loop, which hands out a real one so `job.cancel` has
    /// something to notify.
    #[cfg(test)]
    pub async fn handle_submit(&self, submit: JobSubmit, tx: mpsc::UnboundedSender<WorkerMessage>) {
        self.handle_submit_cancellable(submit, tx, Arc::new(tokio::sync::Notify::new()), None)
            .await;
    }

    pub async fn handle_submit_cancellable(
        &self,
        submit: JobSubmit,
        tx: mpsc::UnboundedSender<WorkerMessage>,
        stop: Arc<tokio::sync::Notify>,
        raw: Option<String>,
    ) {
        let job_id = submit.job_id;
        // Whatever branch this returns from, the job is no longer something
        // `job.cancel` can reach — so it comes out of the registry no matter
        // which of the many early returns below fires.
        let _forget = ForgetOnDrop {
            worker: self,
            job_id,
        };

        if let Err(e) = self.authorize(&submit, raw.as_deref()) {
            tracing::warn!(%job_id, from = %submit.from, "refused: {e}");
            self.meter.rejected();
            send_failed(&tx, job_id, &e.to_string());
            return;
        }
        // `raw` was only needed for signature verification against the exact
        // wire bytes. Release it now — held to the job's end it would pin up to
        // 64 MiB per in-flight job.
        drop(raw);

        let mut submit = submit;
        let holdback = self.holdback_for(&submit.payload);
        if let Err(e) = self.take_payment(&submit, holdback) {
            tracing::warn!(%job_id, from = %submit.from, "refused: {e}");
            self.meter.rejected();
            send_failed(&tx, job_id, &e.to_string());
            return;
        }

        if holdback {
            match self.take_bond(&submit) {
                Ok(delta) => match self.clamp_to_chunk(submit.payload.clone(), delta) {
                    Ok(payload) => submit.payload = payload,
                    Err(e) => {
                        self.pending_bonds
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&job_id);
                        tracing::warn!(%job_id, from = %submit.from, "refused: {e}");
                        self.meter.rejected();
                        send_failed(&tx, job_id, &e.to_string());
                        return;
                    }
                },
                Err(e) => {
                    tracing::warn!(%job_id, from = %submit.from, "refused: {e}");
                    self.meter.rejected();
                    send_failed(&tx, job_id, &e.to_string());
                    return;
                }
            }
            if let Err(e) = self.ensure_lock(&submit).await {
                self.pending_bonds
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&job_id);
                tracing::warn!(%job_id, from = %submit.from, "refused: {e}");
                self.meter.rejected();
                send_failed(&tx, job_id, &e.to_string());
                return;
            }
        }

        if let Err(e) = submit.payload.validate() {
            self.meter.rejected();
            send_failed(&tx, job_id, &e.to_string());
            return;
        }

        // Before a permit, before a backend, before any GPU time: the one
        // thing this machine will not make.
        //
        // `a_screened_request_is_refused_before_any_work_happens` fails the
        // moment this is removed, which is the point — a check that can be
        // quietly commented out is not a check.
        if let Err(refusal) = crate::screen::check(&submit.payload) {
            tracing::warn!(%job_id, from = %submit.from, "screened out: {refusal}");
            self.meter.rejected();
            send_failed(&tx, job_id, &refusal.to_string());
            return;
        }

        let backend = match self.registry.route(&submit.payload) {
            Ok(b) => b.clone(),
            Err(e) => {
                self.meter.rejected();
                send_failed(&tx, job_id, &e.to_string());
                return;
            }
        };

        send_status(&tx, job_id, JobStatus::Queued, 0.0);

        // Waiting for a permit is the queue. The client sees `queued` until a
        // slot frees, which is the honest description of what is happening —
        // and a queued job is exactly the one most worth being able to stop,
        // since it hasn't cost either side anything yet.
        let permit = tokio::select! {
            biased;
            _ = stop.notified() => {
                tracing::info!(%job_id, "stopped while queued");
                send_failed(&tx, job_id, STOPPED);
                return;
            }
            acquired = self.permits.clone().acquire_owned() => match acquired {
                Ok(p) => p,
                Err(_) => {
                    send_failed(&tx, job_id, "worker is shutting down");
                    return;
                }
            },
        };

        send_status(&tx, job_id, JobStatus::Running, 0.0);

        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<f32>();
        let (delta_tx, mut delta_rx) = mpsc::unbounded_channel::<TokenDelta>();
        let (need_tx, mut need_rx) = mpsc::unbounded_channel::<tokio::sync::oneshot::Sender<u64>>();
        let price = self.advertised_price(&submit.payload);
        let start_authorized = if holdback {
            self.pending_bonds
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&job_id)
                .map(|b| price.tokens_for_micros(b.delta))
                .unwrap_or(0)
        } else {
            u64::MAX
        };
        let prompt_used = match &submit.payload {
            JobPayload::Llm(p) => TokenUsage::measure(p, None, None, &[]).prompt,
            _ => 0,
        };
        let forwarder = {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut authorized = start_authorized;
                let mut used = prompt_used;
                loop {
                    tokio::select! {
                        fraction = progress_rx.recv() => match fraction {
                            Some(f) => send_status(&tx, job_id, JobStatus::Running, f),
                            None => break,
                        },
                        delta = delta_rx.recv() => match delta {
                            Some(d) => {
                                used = used.saturating_add(delta_tokens(&d));
                                while used > authorized && authorized != u64::MAX {
                                    let (otx, orx) = tokio::sync::oneshot::channel();
                                    if need_tx.send(otx).is_err() {
                                        return;
                                    }
                                    match orx.await {
                                        Ok(next) => authorized = next,
                                        Err(_) => return,
                                    }
                                }
                                send_delta(&tx, job_id, d);
                            }
                            None => break,
                        },
                    }
                }
                while let Some(f) = progress_rx.recv().await {
                    send_status(&tx, job_id, JobStatus::Running, f);
                }
                while let Some(d) = delta_rx.recv().await {
                    send_delta(&tx, job_id, d);
                }
            })
        };

        let started = std::time::Instant::now();
        // Streaming is prepaid in 1M-token slices. The first slices are on
        // submit; if the reply runs long the pump below asks for another
        // without stopping the GPU. Unpaid tokens are never forwarded.
        let mut topping = holdback;
        let mut stopped = false;
        let outcome = {
            let progress = Progress::new(progress_tx).with_tokens(delta_tx);
            let payload = submit.payload.clone();
            let run = backend.run(job_id, &payload, &progress);
            tokio::pin!(run);
            loop {
                tokio::select! {
                    biased;
                    _ = stop.notified() => {
                        tracing::info!(%job_id, "stopped mid-generation");
                        stopped = true;
                        break Err(WorkerError::Rejected(STOPPED.into()));
                    }
                    req = need_rx.recv(), if topping => {
                        match req {
                            Some(reply) => {
                                if self.request_top_up(&submit, &price, &tx, &stop).await {
                                    let next = self
                                        .pending_bonds
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .get(&job_id)
                                        .map(|b| price.tokens_for_micros(b.delta))
                                        .unwrap_or(0);
                                    let _ = reply.send(next);
                                }
                            }
                            None => topping = false,
                        }
                    }
                    outcome = &mut run => break outcome,
                }
            }
        };
        let _ = forwarder.await;
        drop(permit);
        if stopped {
            send_failed(&tx, job_id, STOPPED);
            return;
        }

        match outcome {
            Ok(result) => {
                tracing::info!(
                    %job_id,
                    backend = backend.name(),
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "done"
                );
                self.meter.record(&result, &self.price_of(&result));
                let amount = self.bill_micros(&submit.payload, &result);
                send_result(&tx, result.clone());
                // Invoice before Done. The desktop (and p2p transport) hang
                // up on a terminal status, so a capture sent after that never
                // reaches the client — then we wait 30s and settle the chunk.
                if holdback && amount > 0 {
                    self.capture_chunk(&submit, &result, amount, &tx, &stop)
                        .await;
                } else {
                    self.pending_bonds
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&job_id);
                }
                send_status(&tx, job_id, JobStatus::Done, 1.0);
            }
            Err(e) => {
                tracing::warn!(%job_id, backend = backend.name(), "failed: {e}");
                self.meter.failed();
                send_failed(&tx, job_id, &e.to_string());
            }
        }
    }

    /// Ask the client for the next 1M-token slice. The desktop signs this
    /// with the app key and no UI, so a long reply does not hitch.
    async fn request_top_up(
        &self,
        submit: &JobSubmit,
        price: &Price,
        tx: &mpsc::UnboundedSender<WorkerMessage>,
        stop: &tokio::sync::Notify,
    ) -> bool {
        let job_id = submit.job_id;
        let amount = match submit.payload.kind() {
            rootmode_core::JobKind::Llm => price.chunk_micros(),
            rootmode_core::JobKind::Image | rootmode_core::JobKind::Video => {
                (price.amount * 1_000_000.0).round().max(1.0) as u64
            }
        };
        let invoice = JobInvoice {
            v: PROTOCOL_VERSION,
            job_id,
            amount,
            sha256: String::new(),
            prompt_tokens: 0,
            completion_tokens: rootmode_core::TOKEN_CHUNK,
            cached_tokens: 0,
            top_up: true,
        };
        let (pay_tx, pay_rx) = tokio::sync::oneshot::channel();
        self.pending_pays
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(job_id, pay_tx);
        let _ = tx.send(WorkerMessage::JobInvoice(invoice));
        let wait = if cfg!(test) {
            Duration::from_millis(400)
        } else {
            Duration::from_secs(5)
        };
        let paid = tokio::select! {
            biased;
            _ = stop.notified() => {
                self.pending_pays
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&job_id);
                None
            }
            p = pay_rx => p.ok(),
            _ = tokio::time::sleep(wait) => {
                self.pending_pays
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&job_id);
                None
            }
        };
        let Some(pay) = paid else {
            return false;
        };
        let already = self
            .channels
            .authorised_for(&pay.ticket.client, &pay.ticket.worker_payout);
        let Some(domain) = self.config.payments.domain() else {
            return false;
        };
        let mut bonds = self.pending_bonds.lock().unwrap_or_else(|e| e.into_inner());
        let Some(bond) = bonds.get_mut(&job_id) else {
            return false;
        };
        // A top-up ticket must be signed by the same on-chain app key as the
        // original bond, or it will never settle.
        if pay
            .ticket
            .check(&domain, &pay.sig, &bond.app_key, now_secs())
            .is_err()
        {
            return false;
        }
        let new_delta = pay.ticket.cumulative.saturating_sub(already);
        if new_delta <= bond.delta {
            return false;
        }
        bond.delta = new_delta;
        bond.pay = pay;
        true
    }

    /// After the stream, capture the actual bill if the client signs it,
    /// otherwise settle the prepaid chunk they already authorised.
    async fn capture_chunk(
        &self,
        submit: &JobSubmit,
        result: &JobResult,
        amount: u64,
        tx: &mpsc::UnboundedSender<WorkerMessage>,
        stop: &tokio::sync::Notify,
    ) {
        let job_id = submit.job_id;
        let bond = self
            .pending_bonds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&job_id);
        let Some(bond) = bond else {
            return;
        };
        let app_key = bond.app_key.clone();
        let amount = amount.min(bond.delta).max(1);
        let usage = match &submit.payload {
            JobPayload::Llm(p) => TokenUsage::measure(
                p,
                result.text.as_deref(),
                result.thinking.as_deref(),
                &result.tool_calls,
            )
            .reconcile(TokenUsage::from_meta(&result.meta)),
            _ => TokenUsage::from_meta(&result.meta).unwrap_or_default(),
        };
        let invoice = JobInvoice {
            v: PROTOCOL_VERSION,
            job_id,
            amount,
            sha256: result.sha256.clone(),
            prompt_tokens: usage.prompt,
            completion_tokens: usage.completion,
            cached_tokens: usage.cached,
            top_up: false,
        };
        let (pay_tx, pay_rx) = tokio::sync::oneshot::channel();
        self.pending_pays
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(job_id, pay_tx);
        let _ = tx.send(WorkerMessage::JobInvoice(invoice));

        let wait = if cfg!(test) {
            Duration::from_millis(400)
        } else {
            Duration::from_secs(30)
        };
        let paid = tokio::select! {
            biased;
            _ = stop.notified() => {
                self.pending_pays
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&job_id);
                None
            }
            p = pay_rx => p.ok(),
            _ = tokio::time::sleep(wait) => {
                self.pending_pays
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&job_id);
                None
            }
        };

        let (chosen, billed) = match paid {
            Some(pay) if self.bank_pay(submit, &pay, amount, &app_key).is_ok() => (pay, amount),
            _ => {
                if let Err(e) = self.bank_pay(submit, &bond.pay, bond.delta, &app_key) {
                    tracing::warn!(%job_id, "could not bank prepaid chunk: {e}");
                    return;
                }
                (bond.pay, bond.delta)
            }
        };
        // One line per paid job that an operator can reconcile against the
        // upstream provider's statement: what the client was billed, and
        // what the work cost, in the same units.
        tracing::info!(
            %job_id,
            model = result.meta.get("model").and_then(|m| m.as_str()).unwrap_or(""),
            billed_micros = billed,
            upstream_cost_usd = result
                .meta
                .get("upstream_cost")
                .and_then(|c| c.as_f64())
                .unwrap_or(0.0),
            prompt = usage.prompt,
            cached = usage.cached,
            completion = usage.completion,
            reasoning = usage.reasoning,
            "billed"
        );
        self.settle_if_configured(job_id, &chosen).await;
    }

    /// Settle every channel that is worth a transaction or whose newest
    /// ticket is close to expiring. Run periodically; see `SETTLE_MIN_MICROS`.
    pub async fn settle_due(&self) {
        if !self.can_settle() {
            return;
        }
        for ch in self.channels.redeemable() {
            let (Some(ticket), Some(sig)) = (ch.spend.clone(), ch.spend_sig.clone()) else {
                continue;
            };
            if self.worth_settling(Uuid::nil(), &ticket).await {
                let pay = JobPay {
                    v: PROTOCOL_VERSION,
                    job_id: Uuid::nil(),
                    ticket,
                    sig,
                };
                self.settle_ticket(Uuid::nil(), &ch.channel_id, &pay).await;
            }
        }
    }

    fn can_settle(&self) -> bool {
        !self.config.payments.rpc.trim().is_empty()
            && !(self.config.payments.sender.trim().is_empty()
                && self.config.payments.key.trim().is_empty())
    }

    async fn settle_if_configured(&self, job_id: Uuid, pay: &JobPay) {
        if !self.can_settle() {
            return;
        }
        let id = rootmode_core::payments::channel_id(
            &pay.ticket.client,
            &pay.ticket.worker_payout,
            "pot",
        );
        if self.worth_settling(job_id, &pay.ticket).await {
            self.settle_ticket(job_id, &id, pay).await;
        }
    }

    /// Whether to spend a transaction on this ticket now.
    ///
    /// Measured against the chain, not this node's ledger: on a payout
    /// channel shared by a fleet, the delta a settle actually claims is the
    /// ticket's rise over what the chain has recognised — every sibling's
    /// unsettled bills included. Wait too long and that delta passes the
    /// channel's per-job cap, the settle reverts `OverCap`, and the tickets
    /// expire unpaid. So settle once the chain delta is worth the gas, well
    /// before it nears the cap, and always before the deadline. If the chain
    /// cannot be read, settle: an unnecessary transaction costs a cent, an
    /// expired ticket costs the job.
    async fn worth_settling(&self, job_id: Uuid, ticket: &rootmode_core::payments::SpendTicket) -> bool {
        let due_soon = ticket.deadline.saturating_sub(now_secs()) < SETTLE_BEFORE_DEADLINE_SECS;
        if due_soon {
            return true;
        }
        let Ok(Some(state)) = self.read_channel(&ticket.client, &ticket.worker_payout).await else {
            return true;
        };
        let delta = ticket.cumulative.saturating_sub(state.earned);
        if delta == 0 {
            return false;
        }
        let near_cap = state.max_per_job > 0 && delta >= state.max_per_job / 2;
        if delta >= SETTLE_MIN_MICROS || near_cap {
            return true;
        }
        tracing::info!(%job_id, delta_micros = delta, "settle deferred until worth a transaction");
        false
    }

    async fn settle_ticket(&self, job_id: Uuid, channel_id: &str, pay: &JobPay) {
        let Ok(sig) = hex::decode(pay.sig.trim_start_matches("0x")) else {
            tracing::warn!(%job_id, "pay signature is not hex");
            return;
        };
        if sig.len() != 65 {
            tracing::warn!(%job_id, "pay signature is not 65 bytes");
            return;
        }
        match crate::chain::settle(&self.config.payments, &pay.ticket, &sig).await {
            Ok(Some(hash)) => {
                tracing::info!(%job_id, %hash, cumulative = pay.ticket.cumulative, "settled");
                self.channels.settled(channel_id, pay.ticket.cumulative);
            }
            // The chain already recognises this cumulative — another node on
            // the same payout channel settled past it. The treasury has the
            // money; there is nothing left for this ticket to collect.
            Ok(None) => {
                tracing::info!(%job_id, "already settled on-chain; nothing new to pay");
                self.channels.settled(channel_id, pay.ticket.cumulative);
            }
            Err(e) => {
                let msg = e.to_string().to_lowercase();
                if msg.contains("insufficient funds") || msg.contains("gas required exceeds") {
                    tracing::error!(
                        %job_id,
                        "settle failed: the pay key is out of ETH for gas — top it up or paid work \
                         goes uncollected: {e}"
                    );
                } else {
                    tracing::warn!(%job_id, "settle later: {e}");
                }
            }
        }
    }

    fn bank_pay(
        &self,
        submit: &JobSubmit,
        pay: &JobPay,
        amount: u64,
        app_key: &str,
    ) -> std::result::Result<(), String> {
        if pay.job_id != submit.job_id {
            return Err("pay ticket is for a different job".into());
        }
        let domain = self
            .config
            .payments
            .domain()
            .ok_or_else(|| "this node has no settlement contract".to_string())?;
        if let Some(payout) = payout_of(&self.config.worker.payout_address) {
            if !pay.ticket.worker_payout.eq_ignore_ascii_case(&payout) {
                return Err("ticket is for a different worker".into());
            }
        }
        if let Some(payer) = submit.payer.as_deref() {
            if !pay.ticket.client.eq_ignore_ascii_case(payer) {
                return Err("ticket is for a different payer".into());
            }
        }
        self.channels
            .accept_spend(&pay.ticket, &pay.sig, &domain, app_key, amount, now_secs() as i64)
            .map_err(|e| format!("payment authorisation: {e}"))?;
        Ok(())
    }

    /// Signature and allowlist policy. Unsigned submits are legal in v1, so
    /// this is a deliberate operator choice rather than a default.
    fn authorize(&self, submit: &JobSubmit, raw: Option<&str>) -> Result<()> {
        if self.config.require_signature() || submit.sig.is_some() {
            let wire_ok = raw
                .map(JobSubmit::verify_wire)
                .map(|r| r.is_ok())
                .unwrap_or(false);
            if !wire_ok {
                submit
                    .verify()
                    .map_err(|e| WorkerError::Rejected(format!("signature: {e}")))?;
            }
        }

        let allow = &self.config.worker.allow_peers;
        if !allow.is_empty() && !allow.iter().any(|p| p.eq_ignore_ascii_case(&submit.from)) {
            return Err(WorkerError::Rejected(
                "this worker does not accept jobs from your peer id".into(),
            ));
        }
        Ok(())
    }
}

fn send_status(
    tx: &mpsc::UnboundedSender<WorkerMessage>,
    job_id: Uuid,
    status: JobStatus,
    progress: f32,
) {
    let _ = tx.send(WorkerMessage::JobStatus(JobStatusUpdate {
        v: PROTOCOL_VERSION,
        job_id,
        status,
        progress,
        error: None,
    }));
}

use rootmode_core::protocol::STOPPED;

fn send_failed(tx: &mpsc::UnboundedSender<WorkerMessage>, job_id: Uuid, error: &str) {
    let _ = tx.send(WorkerMessage::JobStatus(JobStatusUpdate {
        v: PROTOCOL_VERSION,
        job_id,
        status: JobStatus::Failed,
        progress: 0.0,
        error: Some(error.to_string()),
    }));
}

fn send_result(tx: &mpsc::UnboundedSender<WorkerMessage>, result: JobResult) {
    let _ = tx.send(WorkerMessage::JobResult(result));
}

fn delta_tokens(delta: &TokenDelta) -> u64 {
    let n = rootmode_core::tokens::count_text(
        rootmode_core::tokens::encoding_for(None),
        &delta.text,
    )
    .saturating_add(rootmode_core::tokens::count_text(
        rootmode_core::tokens::encoding_for(None),
        &delta.thinking,
    ));
    if n == 0 && (!delta.text.is_empty() || !delta.thinking.is_empty()) {
        1
    } else {
        n
    }
}

fn send_delta(tx: &mpsc::UnboundedSender<WorkerMessage>, job_id: Uuid, delta: TokenDelta) {
    if delta.text.is_empty() && delta.thinking.is_empty() {
        return;
    }
    let _ = tx.send(WorkerMessage::JobDelta(rootmode_core::JobDelta {
        v: PROTOCOL_VERSION,
        job_id,
        text: delta.text,
        thinking: delta.thinking,
    }));
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_long_prompt_is_priced_at_the_input_rate_not_the_output_rate() {
        use rootmode_core::{ChatMessage, JobPayload, LlmParams, ModelDescriptor, Price};
        // Input $1/M, output $20/M — a 30k-token prompt costs $0.03, and the
        // $0.50 lock must leave room for ~23k output tokens, not refuse it.
        let price = Price {
            amount: 20.0,
            currency: "USD".into(),
            input: Some(1.0),
            output: Some(20.0),
            cache: None,
            cache_write: None,
        };
        let worker = Worker::new(
            priced_config(),
            Identity::generate(),
            crate::backends::testing::registry_with(
                vec![ModelDescriptor {
                    id: "pricey".into(),
                    sha256: None,
                    kind: rootmode_core::JobKind::Llm,
                    price: Some(price),
                }],
                rootmode_core::JobKind::Llm,
            ),
        );
        let payload = JobPayload::Llm(LlmParams {
            model_hash: None,
            model_id: Some("pricey".into()),
            messages: vec![ChatMessage::new("user", &"word ".repeat(30_000))],
            tools: Vec::new(),
            max_tokens: 100_000,
            temperature: 0.0,
        });
        let JobPayload::Llm(clamped) = worker.clamp_to_chunk(payload, 500_000).unwrap() else {
            panic!("llm payload")
        };
        assert!(clamped.max_tokens > 15_000, "room left for the answer: {}", clamped.max_tokens);
        assert!(clamped.max_tokens < 25_000, "but bounded by what the lock buys: {}", clamped.max_tokens);
    }

    use super::*;
    use crate::backends::testing::registry_with;
    use crate::config::{BackendConfig, VllmConfig, WorkerConfig};
    use rootmode_core::{ChatMessage, JobKind, JobPayload, LlmParams};

    fn config(require_signature: bool, allow_peers: Vec<String>) -> Config {
        Config {
            payments: Default::default(),
            p2p: Default::default(),
            stats: Default::default(),
            worker: WorkerConfig {
                label: "test".into(),
                listen: "127.0.0.1:0".into(),
                max_concurrent: 1,
                require_signature,
                allow_peers,
                identity_file: "unused.key".into(),
                refresh_secs: 0,
                country: String::new(),
                payout_address: String::new(),
            },
            backends: vec![BackendConfig::Vllm(VllmConfig {
                endpoint: "http://127.0.0.1:1".into(),
                api_key: None,
                models: vec![],
                model_hashes: Default::default(),
                price: None,
                prices: Default::default(),
                currency: "USD".into(),
                timeout_secs: 5,
            })],
        }
    }

    fn worker(config: Config) -> Worker {
        Worker::new(
            config,
            Identity::generate(),
            registry_with(vec![], JobKind::Llm),
        )
    }

    fn payload() -> JobPayload {
        JobPayload::Llm(LlmParams {
            model_hash: None,
            model_id: None,
            messages: vec![ChatMessage::new("user", "ping")],
            tools: Vec::new(),
            max_tokens: 16,
            temperature: 0.0,
        })
    }

    async fn collect(worker: &Worker, submit: JobSubmit) -> Vec<WorkerMessage> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        worker.handle_submit(submit, tx).await;
        let mut out = vec![];
        while let Ok(m) = rx.try_recv() {
            out.push(m);
        }
        out
    }

    fn statuses(messages: &[WorkerMessage]) -> Vec<(JobStatus, Option<String>)> {
        messages
            .iter()
            .filter_map(|m| match m {
                WorkerMessage::JobStatus(s) => Some((s.status, s.error.clone())),
                _ => None,
            })
            .collect()
    }

    fn worker_slow(config: Config, delay: std::time::Duration) -> Worker {
        Worker::new(
            config,
            Identity::generate(),
            crate::backends::testing::registry_slow(JobKind::Llm, delay),
        )
    }

    /// Stopping a job that hasn't started yet — still queued behind another
    /// one — should refuse the slot rather than let it run once freed.
    #[tokio::test]
    async fn a_queued_job_can_be_stopped_before_it_ever_runs() {
        let mut cfg = config(false, vec![]);
        cfg.worker.max_concurrent = 1;
        let worker = Arc::new(worker_slow(cfg, std::time::Duration::from_millis(300)));

        // Occupies the one permit for the whole test.
        let (tx1, _rx1) = mpsc::unbounded_channel();
        let busy = worker.clone();
        let occupying = tokio::spawn(async move {
            busy.handle_submit(JobSubmit::new(Uuid::new_v4(), "client", payload()), tx1)
                .await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        let (tx2, mut rx2) = mpsc::unbounded_channel();
        let job_id = Uuid::new_v4();
        let stop = Arc::new(tokio::sync::Notify::new());
        let queued = worker.clone();
        let stop_clone = stop.clone();
        let handle = tokio::spawn(async move {
            queued
                .handle_submit_cancellable(
                    JobSubmit::new(job_id, "client", payload()),
                    tx2,
                    stop_clone,
                    None,
                )
                .await;
        });

        // Give it time to actually reach the queue, then stop it — well
        // before the first job's 300ms is up, so this only passes if the
        // stop landed on the wait itself.
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        stop.notify_waiters();
        handle.await.unwrap();

        let mut out = vec![];
        while let Ok(m) = rx2.try_recv() {
            out.push(m);
        }
        assert_eq!(statuses(&out), vec![(JobStatus::Queued, None), (JobStatus::Failed, Some(STOPPED.into()))]);

        occupying.await.unwrap();
    }

    /// Stopping a job already running should end it before it finishes, not
    /// merely stop caring about the result.
    #[tokio::test]
    async fn a_running_job_can_be_stopped_mid_generation() {
        let worker = Arc::new(worker_slow(config(false, vec![]), std::time::Duration::from_secs(5)));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let job_id = Uuid::new_v4();
        let stop = Arc::new(tokio::sync::Notify::new());

        let w = worker.clone();
        let s = stop.clone();
        let handle = tokio::spawn(async move {
            w.handle_submit_cancellable(JobSubmit::new(job_id, "client", payload()), tx, s, None)
                .await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let started = std::time::Instant::now();
        stop.notify_waiters();
        handle.await.unwrap();

        // The 5-second backend never got to finish — if it had, this would
        // have taken 5 seconds instead of effectively none.
        assert!(started.elapsed() < std::time::Duration::from_millis(500));

        let mut out = vec![];
        while let Ok(m) = rx.try_recv() {
            out.push(m);
        }
        let last = out
            .iter()
            .filter_map(|m| match m {
                WorkerMessage::JobStatus(s) => Some(s),
                _ => None,
            })
            .last()
            .unwrap();
        assert_eq!(last.status, JobStatus::Failed);
        assert_eq!(last.error.as_deref(), Some(STOPPED));
        assert!(!out.iter().any(|m| matches!(m, WorkerMessage::JobResult(_))), "a stopped job files no result");
    }

    /// A `job.cancel` for a job that already finished, or never existed, is a
    /// no-op — not a panic, not a stray notification landing on someone else's
    /// job that happens to reuse a freed slot in the map.
    #[tokio::test]
    async fn cancelling_an_unknown_job_does_nothing() {
        let worker = Arc::new(worker(config(false, vec![])));
        let messages = collect(&worker, JobSubmit::new(Uuid::new_v4(), "client", payload())).await;
        assert_eq!(statuses(&messages).last().map(|(s, _)| *s), Some(JobStatus::Done));

        // No cancellations map entry exists for a random id — this must not
        // panic, and the lock must not poison anything for the job after it.
        let (tx, _rx) = mpsc::unbounded_channel();
        worker.on_line(
            &serde_json::to_string(&rootmode_core::protocol::ClientMessage::JobCancel(
                rootmode_core::protocol::JobCancel { job_id: Uuid::new_v4() },
            ))
            .unwrap(),
            &tx,
        );
    }

    /// A screened request never reaches a backend.
    ///
    /// The check has its own unit tests; this one proves it is actually wired
    /// into the path a real job takes, which is the part that silently rots.
    #[tokio::test]
    async fn a_screened_request_is_refused_before_any_work_happens() {
        // A registry with no backend at all: if the screen let this through,
        // routing would fail with a different error, so the assertion below
        // really is testing the screen.
        let worker = worker(config(false, vec![]));

        let (tx, mut rx) = mpsc::unbounded_channel();
        let job_id = Uuid::new_v4();
        worker
            .handle_submit(
                JobSubmit::new(
                    job_id,
                    "client",
                    JobPayload::Image(rootmode_core::ImageParams {
                        model_hash: None,
                        checkpoint_id: None,
                        prompt: "naked child".into(),
                        from_image: None,
                        change: None,
                        mask: None,
                    }),
                ),
                tx,
            )
            .await;

        let mut refused = false;
        while let Some(msg) = rx.recv().await {
            if let WorkerMessage::JobStatus(s) = msg {
                if s.status == JobStatus::Failed {
                    refused = true;
                    let why = s.error.unwrap_or_default();
                    assert!(why.contains("minor"), "the refusal says why, got: {why}");
                }
            }
        }
        assert!(refused, "the job was refused, and said why");
    }

    #[tokio::test]
    async fn a_good_job_goes_queued_running_result_done() {
        let worker = worker(config(false, vec![]));
        let messages = collect(
            &worker,
            JobSubmit::new(Uuid::new_v4(), "anonymous", payload()),
        )
        .await;

        let kinds: Vec<JobStatus> = statuses(&messages).into_iter().map(|(s, _)| s).collect();
        assert_eq!(kinds.first(), Some(&JobStatus::Queued));
        assert_eq!(kinds.last(), Some(&JobStatus::Done));
        assert!(kinds.contains(&JobStatus::Running));
        assert!(messages
            .iter()
            .any(|m| matches!(m, WorkerMessage::JobResult(_))));
    }

    #[tokio::test]
    async fn announce_reports_what_is_actually_loaded() {
        let worker = worker(config(false, vec![]));
        let announce = worker.announce();
        assert_eq!(announce.v, 1);
        assert_eq!(announce.caps, vec!["llm"]);
        assert_eq!(announce.peer_id, worker.peer_id());
        assert_eq!(announce.max_concurrent, 1);
    }

    #[tokio::test]
    async fn unsigned_is_refused_when_the_operator_requires_signatures() {
        let worker = worker(config(true, vec![]));
        let messages = collect(&worker, JobSubmit::new(Uuid::new_v4(), "anon", payload())).await;

        let (status, error) = statuses(&messages)[0].clone();
        assert_eq!(status, JobStatus::Failed);
        assert!(error.unwrap().contains("unsigned"));
    }

    #[tokio::test]
    async fn a_forged_signature_is_refused_even_when_signing_is_optional() {
        let worker = worker(config(false, vec![]));
        let client = Identity::generate();
        let mut submit = JobSubmit::new(Uuid::new_v4(), "x", payload())
            .signed_by(&client)
            .unwrap();
        submit.job_id = Uuid::new_v4(); // signature no longer covers this

        let (status, error) = statuses(&collect(&worker, submit).await)[0].clone();
        assert_eq!(status, JobStatus::Failed);
        assert!(error.unwrap().contains("signature"));
    }

    /// The desktop sends a `type`-tagged envelope. Signing is over the job
    /// body. Authorize uses the raw JSON so floats cannot drift.
    #[tokio::test]
    async fn a_signed_chat_on_the_wire_is_accepted() {
        let client = Identity::generate();
        let worker = Arc::new(worker(config(true, vec![])));
        let payload = JobPayload::Llm(LlmParams {
            model_hash: None,
            model_id: Some("llama-3.1-8b-instruct".into()),
            messages: vec![
                ChatMessage::new("user", "Test"),
                ChatMessage::new("user", "Test"),
            ],
            tools: Vec::new(),
            max_tokens: 16384,
            temperature: 0.7,
        });
        let submit = JobSubmit::new(Uuid::new_v4(), "x", payload)
            .signed_by(&client)
            .unwrap();
        let wire =
            rootmode_core::canonical::wire_json(&ClientMessage::JobSubmit(submit.clone())).unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let parsed = ClientMessage::parse(&wire).expect("wire");
        let ClientMessage::JobSubmit(again) = parsed else {
            panic!("not a submit");
        };
        worker
            .handle_submit_cancellable(
                again,
                tx,
                Arc::new(tokio::sync::Notify::new()),
                Some(wire.clone()),
            )
            .await;

        let mut out = vec![];
        while let Ok(m) = rx.try_recv() {
            out.push(m);
        }
        let err = statuses(&out)
            .into_iter()
            .find_map(|(_, e)| e);
        assert!(
            err.as_ref().is_none_or(|e| !e.contains("signature")),
            "signed chat was rejected as a signature: {err:?}\nwire={wire}"
        );
        assert!(
            out.iter()
                .any(|m| matches!(m, WorkerMessage::JobResult(_))),
            "expected a result, got {out:?}"
        );
    }

    #[tokio::test]
    async fn an_allowlist_keeps_strangers_out_and_lets_members_in() {
        let member = Identity::generate();
        let stranger = Identity::generate();
        let worker = worker(config(false, vec![member.peer_id()]));

        let refused = collect(
            &worker,
            JobSubmit::new(Uuid::new_v4(), "x", payload())
                .signed_by(&stranger)
                .unwrap(),
        )
        .await;
        let (status, error) = statuses(&refused)[0].clone();
        assert_eq!(status, JobStatus::Failed);
        assert!(error.unwrap().contains("does not accept jobs"));

        let accepted = collect(
            &worker,
            JobSubmit::new(Uuid::new_v4(), "x", payload())
                .signed_by(&member)
                .unwrap(),
        )
        .await;
        assert!(accepted
            .iter()
            .any(|m| matches!(m, WorkerMessage::JobResult(_))));
    }

    #[tokio::test]
    async fn an_invalid_payload_fails_before_it_reaches_a_backend() {
        let worker = worker(config(false, vec![]));
        let bad = JobPayload::Llm(LlmParams {
            model_hash: None,
            model_id: None,
            messages: vec![],
            tools: Vec::new(),
            max_tokens: 16,
            temperature: 0.0,
        });
        let (status, error) =
            statuses(&collect(&worker, JobSubmit::new(Uuid::new_v4(), "x", bad)).await)[0].clone();
        assert_eq!(status, JobStatus::Failed);
        assert!(error.unwrap().contains("no messages"));
    }

    #[tokio::test]
    async fn an_image_job_on_an_llm_only_worker_is_refused_clearly() {
        let worker = worker(config(false, vec![]));
        let image = JobPayload::Image(rootmode_core::ImageParams {
            model_hash: None,
            checkpoint_id: None,
            prompt: "x".into(),
            from_image: None,
            change: None,
            mask: None,
        });
        let (status, error) =
            statuses(&collect(&worker, JobSubmit::new(Uuid::new_v4(), "x", image)).await)[0]
                .clone();
        assert_eq!(status, JobStatus::Failed);
        assert!(error.unwrap().contains("no image backend"));
    }

    #[test]
    fn a_country_is_two_letters_or_it_is_not_shown() {
        assert_eq!(normalise_country("de"), Some("DE".into()));
        assert_eq!(normalise_country("  GB "), Some("GB".into()));
        // Not a code: better to say nothing than to show a client nonsense
        // where a country should be.
        for junk in ["", "   ", "Germany", "D", "D1", "🇩🇪"] {
            assert_eq!(normalise_country(junk), None, "{junk:?}");
        }
    }

    fn priced_config() -> Config {
        let mut cfg = config(false, vec![]);
        cfg.payments.contract = "0x1234567890abcdef1234567890abcdef12345678".into();
        cfg.payments.channels_file = std::env::temp_dir()
            .join(format!("rootmode-chunk-{}.json", Uuid::new_v4()));
        cfg.worker.payout_address = "0x00000000000000000000000000000000000000b0".into();
        cfg
    }

    /// The address `sign_pay` signs with; here the paying client and the
    /// account's app key are the same key.
    fn test_payer() -> String {
        use k256::ecdsa::SigningKey;
        use rootmode_core::payments::address_of;
        let key = SigningKey::from_bytes(&[8u8; 32].into()).unwrap();
        address_of(key.verifying_key())
    }

    fn priced_worker() -> Worker {
        let worker = Worker::new(
            priced_config(),
            Identity::generate(),
            crate::backends::testing::registry_priced(JobKind::Llm, 20.0),
        );
        // Stand in for the on-chain read: ample remaining lock, and the app key
        // the prepaid ticket is signed by.
        worker.set_test_channel(u64::MAX, &test_payer());
        worker
    }

    fn sign_pay(job_id: Uuid, amount: u64, worker_payout: &str) -> JobPay {
        use k256::ecdsa::{signature::hazmat::PrehashSigner, SigningKey};
        use rootmode_core::payments::{address_of, Domain, SpendTicket};
        let key = SigningKey::from_bytes(&[8u8; 32].into()).unwrap();
        let client = address_of(key.verifying_key());
        let ticket = SpendTicket {
            client,
            worker_payout: worker_payout.into(),
            cumulative: amount,
            deadline: 2_000_000_000,
        };
        let domain = Domain::base("0x1234567890abcdef1234567890abcdef12345678");
        let digest = ticket.digest(&domain).unwrap();
        let (sig, rec) = key.sign_prehash(&digest).unwrap();
        let sig = format!("0x{}{:02x}", hex::encode(sig.to_bytes()), rec.to_byte() + 27);
        JobPay {
            v: 1,
            job_id,
            ticket,
            sig,
        }
    }

    async fn recv_until_invoice(
        rx: &mut mpsc::UnboundedReceiver<WorkerMessage>,
    ) -> (Vec<WorkerMessage>, rootmode_core::JobInvoice) {
        let mut out = vec![];
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("timed out waiting for invoice")
                .expect("channel closed");
            if let WorkerMessage::JobInvoice(inv) = &msg {
                let inv = inv.clone();
                out.push(msg);
                return (out, inv);
            }
            out.push(msg);
        }
    }

    fn priced_submit(job_id: Uuid, chunk: u64) -> JobSubmit {
        let mut submit = JobSubmit::new(job_id, "client", payload());
        // The payer names whose on-chain lock and app key the worker checks.
        submit.payer = Some(test_payer());
        submit.bond = Some(sign_pay(
            job_id,
            chunk,
            "0x00000000000000000000000000000000000000b0",
        ));
        submit
    }

    #[tokio::test]
    async fn a_priced_job_streams_against_a_prepaid_chunk() {
        let worker = Arc::new(priced_worker());
        let job_id = Uuid::new_v4();
        let chunk = rootmode_core::Price::new(20.0).chunk_micros();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let stop = Arc::new(tokio::sync::Notify::new());
        let w = worker.clone();
        let running = tokio::spawn(async move {
            w.handle_submit_cancellable(priced_submit(job_id, chunk), tx, stop, None)
                .await;
        });

        let mut got_delta = false;
        let mut got_result = false;
        let mut inv = None;
        while let Some(msg) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("job stalled")
        {
            match &msg {
                WorkerMessage::JobDelta(_) => got_delta = true,
                WorkerMessage::JobResult(_) => got_result = true,
                WorkerMessage::JobInvoice(i) => {
                    inv = Some(i.clone());
                    break;
                }
                _ => {}
            }
        }
        assert!(got_delta, "prepaid chunks stream live");
        assert!(got_result, "the result is not held back after a chunk is signed");
        let inv = inv.expect("invoice for the actual bill");
        assert!(inv.amount > 0);
        assert!(inv.amount <= chunk);

        let pay = sign_pay(job_id, inv.amount, "0x00000000000000000000000000000000000000b0");
        let (dummy, _rx) = mpsc::unbounded_channel();
        worker.on_line(
            &serde_json::to_string(&ClientMessage::JobPay(pay)).unwrap(),
            &dummy,
        );
        running.await.unwrap();
        assert_eq!(worker.channels.owed(), inv.amount, "actual, not the whole chunk");
    }

    #[tokio::test]
    async fn a_priced_job_without_a_chunk_is_refused() {
        let worker = priced_worker();
        let messages = collect(
            &worker,
            JobSubmit::new(Uuid::new_v4(), "client", payload()),
        )
        .await;
        assert!(
            !messages
                .iter()
                .any(|m| matches!(m, WorkerMessage::JobResult(_))),
            "no chunk, no goods"
        );
        let err = statuses(&messages)
            .into_iter()
            .find_map(|(_, e)| e)
            .unwrap_or_default();
        assert!(err.contains("1M-token chunk"), "{err}");
        assert_eq!(worker.channels.owed(), 0);
    }

    /// A node that charges for its other models can still list one for
    /// free — OpenRouter's `:free` tier, say. The client locks nothing for
    /// a $0 price, so no ticket arrives, and `require_auth` must not read
    /// that as a client dodging a bill.
    #[tokio::test]
    async fn a_free_model_on_a_paying_node_is_served_without_a_ticket() {
        let mut cfg = priced_config();
        cfg.payments.require_auth = true;
        let worker = Worker::new(
            cfg,
            Identity::generate(),
            crate::backends::testing::registry_priced(JobKind::Llm, 0.0),
        );
        let messages = collect(
            &worker,
            JobSubmit::new(Uuid::new_v4(), "client", payload()),
        )
        .await;
        assert!(
            messages
                .iter()
                .any(|m| matches!(m, WorkerMessage::JobResult(_))),
            "free is free: {:?}",
            statuses(&messages)
        );
        assert_eq!(worker.channels.owed(), 0);
    }

    /// The other side of that coin: with billing on, a model this node
    /// never listed is not quietly served for nothing.
    #[tokio::test]
    async fn an_unlisted_model_on_a_paying_node_still_needs_a_ticket() {
        let mut cfg = priced_config();
        cfg.payments.require_auth = true;
        let worker = Worker::new(
            cfg,
            Identity::generate(),
            crate::backends::testing::registry_priced_many(
                JobKind::Llm,
                &[("stub", 20.0), ("gratis", 0.0)],
            ),
        );
        let mut params = match payload() {
            JobPayload::Llm(p) => p,
            _ => unreachable!(),
        };
        params.model_id = Some("ghost".into());
        let messages = collect(
            &worker,
            JobSubmit::new(Uuid::new_v4(), "client", JobPayload::Llm(params)),
        )
        .await;
        assert!(
            !messages
                .iter()
                .any(|m| matches!(m, WorkerMessage::JobResult(_))),
            "nobody priced it, nobody serves it"
        );
        let err = statuses(&messages)
            .into_iter()
            .find_map(|(_, e)| e)
            .unwrap_or_default();
        assert!(err.contains("signed spending authorisation"), "{err}");
    }

    #[tokio::test]
    async fn a_silent_client_is_charged_the_prepaid_chunk() {
        let worker = priced_worker();
        let job_id = Uuid::new_v4();
        let chunk = rootmode_core::Price::new(20.0).chunk_micros();
        let messages = collect(&worker, priced_submit(job_id, chunk)).await;
        assert!(
            messages
                .iter()
                .any(|m| matches!(m, WorkerMessage::JobResult(_))),
            "they already paid for the chunk, so they get the stream"
        );
        assert_eq!(
            worker.channels.owed(),
            chunk,
            "no actual ticket → the chunk is captured"
        );
    }

    /// A ticket that rises by more than the invoice is not a bad ticket: on
    /// a payout channel shared by several nodes, the rise includes what the
    /// others settled meanwhile. The bill is the invoice — never the excess,
    /// and never the whole prepaid chunk, which is what refusing it cost.
    #[tokio::test]
    async fn an_overpaying_ticket_is_credited_at_the_invoice() {
        let worker = Arc::new(priced_worker());
        let job_id = Uuid::new_v4();
        let chunk = rootmode_core::Price::new(20.0).chunk_micros();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let stop = Arc::new(tokio::sync::Notify::new());
        let w = worker.clone();
        let running = tokio::spawn(async move {
            w.handle_submit_cancellable(priced_submit(job_id, chunk), tx, stop, None)
                .await;
        });

        let (_, inv) = recv_until_invoice(&mut rx).await;
        let pay = sign_pay(
            job_id,
            inv.amount.saturating_add(1),
            "0x00000000000000000000000000000000000000b0",
        );
        let (dummy, _rx) = mpsc::unbounded_channel();
        worker.on_line(
            &serde_json::to_string(&ClientMessage::JobPay(pay)).unwrap(),
            &dummy,
        );
        running.await.unwrap();
        // The channel carries what the client signed — the invoice plus the
        // one micro it over-signed, which is its own doing and bounded by its
        // own signature. What it must never carry is the prepaid chunk.
        assert_eq!(worker.channels.owed(), inv.amount + 1);
        assert!(
            worker.channels.owed() < chunk,
            "refusing the ticket and keeping the chunk was the old, worse outcome"
        );
    }
}

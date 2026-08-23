//! Client pot: deposit in MetaMask, lock a reserve before work, settle after.
//!
//! A ReserveTicket is posted on-chain before the GPU runs. Withdraw can only
//! take the unlocked remainder — omitting tickets from a forked client does
//! nothing, because the lock is already there. SpendTickets are cumulative;
//! the worker (or anyone) submits the newest.
//!
//! The wallet signs deposit, limits, withdraw of unlocked funds, and close.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use k256::ecdsa::{signature::hazmat::PrehashSigner, SigningKey};
use rootmode_core::payments::{address_of, keccak, Micros, Domain, ReserveTicket, SpendTicket};
use rootmode_core::{JobInvoice, JobKind, JobPay, JobPayload, Price, TokenUsage};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use uuid::Uuid;

use crate::error::{AppError, Result};
use crate::state::AppState;
use crate::store::{Db, StoredDeposit};

const FUND_HTML: &str = include_str!("fund.html"); // 7702 batch on Base only
const FUND_PORT: u16 = 17331;
const DEFAULT_MAX_JOB: Micros = 500_000; // $0.50
const TICKET_TTL_SECS: u64 = 3600;
const SETTLE_EVERY: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainConfig {
    pub rpc: String,
    #[serde(rename = "chainId", alias = "chain_id")]
    pub chain_id: u64,
    pub usdc: String,
    pub pot: String,
    #[serde(rename = "feeVault", alias = "fee_vault", default)]
    pub fee_vault: String,
    pub worker: String,
    #[serde(default)]
    pub client: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PotStatus {
    pub configured: bool,
    pub reachable: bool,
    pub client: Option<String>,
    pub app_key: String,
    pub balance_micros: u64,
    pub max_per_job_micros: u64,
    pub max_per_day_micros: u64,
    pub spent_today_micros: u64,
    pub reserved_micros: u64,
    pub rpc: String,
    pub pot: String,
    pub usdc: String,
    pub chain_id: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PotCheck {
    pub ready: bool,
    pub needs_fund: bool,
    pub reason: String,
    /// `ok`, `cap`, `empty`, or `chain` — so the UI can pick copy and a button.
    pub kind: String,
    pub cap_micros: u64,
}

/// One on-chain `Deposited` for this wallet, newest first.
#[derive(Debug, Clone, Serialize)]
pub struct Deposit {
    pub tx_hash: String,
    pub amount_micros: u64,
    pub max_per_job_micros: u64,
    pub max_per_day_micros: u64,
    pub block: u64,
    /// Unix seconds from the block, 0 if the node would not say.
    pub at: i64,
    /// Basescan (or similar) when we know the chain; none on Anvil.
    pub url: Option<String>,
}

struct DepositLog {
    client: String,
    app_key: String,
}

#[derive(Clone)]
struct PendingJob {
    price: Price,
    kind: JobKind,
    paid: bool,
    sha256: Option<String>,
    /// Prepaid 1M-token slices for this job, in micros.
    ceiling: Micros,
    /// Lock covering the whole job (all slices), in micros.
    job_cap: Micros,
}

struct LatestTicket {
    ticket: SpendTicket,
    sig: Vec<u8>,
    /// What the chain has already paid this worker from this client.
    on_chain_paid: u64,
}

#[derive(Serialize, Deserialize)]
struct DiskTicket {
    ticket: SpendTicket,
    sig: String,
    on_chain_paid: u64,
}

fn jobs() -> &'static Mutex<HashMap<Uuid, PendingJob>> {
    static T: OnceLock<Mutex<HashMap<Uuid, PendingJob>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn latest() -> &'static Mutex<HashMap<String, LatestTicket>> {
    static T: OnceLock<Mutex<HashMap<String, LatestTicket>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn flush_at() -> &'static Mutex<Option<Instant>> {
    static T: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(None))
}

fn gate() -> &'static tokio::sync::Mutex<()> {
    static G: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    G.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn worker_key(worker: &str) -> String {
    worker.trim_start_matches("0x").to_lowercase()
}

fn persist_path(app_data: &Path) -> PathBuf {
    app_data.join("pot-pending.json")
}

fn persist(app_data: &Path) {
    let snapshot: Vec<DiskTicket> = latest()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .map(|t| DiskTicket {
            ticket: t.ticket.clone(),
            sig: hex::encode(&t.sig),
            on_chain_paid: t.on_chain_paid,
        })
        .collect();
    if let Ok(bytes) = serde_json::to_vec(&snapshot) {
        let _ = std::fs::write(persist_path(app_data), bytes);
    }
}

fn restore(app_data: &Path) {
    let Ok(raw) = std::fs::read(persist_path(app_data)) else {
        return;
    };
    let Ok(list) = serde_json::from_slice::<Vec<DiskTicket>>(&raw) else {
        return;
    };
    let mut g = latest().lock().unwrap_or_else(|e| e.into_inner());
    for row in list {
        let Ok(sig) = hex::decode(row.sig.trim_start_matches("0x")) else {
            continue;
        };
        if sig.len() != 65 {
            continue;
        }
        g.insert(
            worker_key(&row.ticket.worker_payout),
            LatestTicket {
                ticket: row.ticket,
                sig,
                on_chain_paid: row.on_chain_paid,
            },
        );
    }
}

/// Load any unsigned-to-chain tickets from the last run and start the 60s
/// settle loop. Pending tickets flush in a couple of seconds, not a full
/// minute — a quit mid-session should still pay the worker.
pub fn boot(app_data: PathBuf) {
    restore(&app_data);
    let has_pending = latest()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .any(|t| t.ticket.cumulative > t.on_chain_paid);
    if has_pending {
        *flush_at().lock().unwrap_or_else(|e| e.into_inner()) =
            Some(Instant::now() + Duration::from_secs(2));
    }
    let _ = ensure_fund_server(app_data.clone());
    ensure_flush_loop(app_data);
}

pub fn load_chain_config(state: &AppState) -> Option<ChainConfig> {
    load_chain_config_at(&state.app_data)
}

/// Baked into the binary. Empty `pot` means Base contracts are not deployed
/// yet and the wallet stays unconfigured.
const BUNDLED_CHAIN: &str = include_str!("chain.base.json");

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn usable(cfg: ChainConfig) -> Option<ChainConfig> {
    if cfg.pot.trim().is_empty() {
        None
    } else {
        Some(cfg)
    }
}

fn load_chain_config_at(app_data: &Path) -> Option<ChainConfig> {
    let home = home_dir().join(".rootmode");
    read_chain_json(&home.join("chain.json"))
        .and_then(usable)
        .or_else(|| read_chain_json(&home.join("local-chain.json")).and_then(usable))
        .or_else(|| read_chain_json(&app_data.join("chain.json")).and_then(usable))
        .or_else(|| read_chain_json(&app_data.join("local-chain.json")).and_then(usable))
        .or_else(|| serde_json::from_str(BUNDLED_CHAIN).ok().and_then(usable))
        .or_else(|| {
            let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../../contracts/deployments/local.json");
            read_chain_json(&repo).and_then(usable)
        })
}

fn read_chain_json(path: &Path) -> Option<ChainConfig> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn app_key_address(state: &AppState) -> Result<String> {
    Ok(address_of(load_or_create_app_key(&state.app_data)?.verifying_key()))
}

fn load_or_create_app_key(app_data: &Path) -> Result<SigningKey> {
    let path = app_data.join("pot-app.key");
    if path.exists() {
        let hex = std::fs::read_to_string(&path)?;
        let bytes = hex::decode(hex.trim()).map_err(|e| AppError::Invalid(e.to_string()))?;
        if bytes.len() != 32 {
            return Err(AppError::Invalid("pot app key is the wrong length".into()));
        }
        return SigningKey::from_bytes(bytes.as_slice().into())
            .map_err(|e| AppError::Invalid(e.to_string()));
    }
    let mut bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
    let key = SigningKey::from_bytes((&bytes).into())
        .map_err(|e| AppError::Invalid(e.to_string()))?;
    std::fs::create_dir_all(app_data)?;
    std::fs::write(&path, hex::encode(key.to_bytes()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(key)
}

pub async fn status(state: &AppState) -> Result<PotStatus> {
    let app_key = app_key_address(state)?;
    let Some(cfg) = load_chain_config(state) else {
        return Ok(PotStatus {
            configured: false,
            reachable: false,
            client: None,
            app_key,
            balance_micros: 0,
            max_per_job_micros: 0,
            max_per_day_micros: 0,
            spent_today_micros: 0,
            reserved_micros: 0,
            rpc: String::new(),
            pot: String::new(),
            usdc: String::new(),
            chain_id: 0,
        });
    };
    let reachable = rpc(&cfg.rpc, "eth_chainId", serde_json::json!([])).await.is_ok();
    let client = match state.db.last_deposit_client() {
        Ok(Some(c)) if !c.trim().is_empty() => Some(c),
        _ => find_client(&cfg, &app_key).await,
    };
    let (balance, max_job, max_day, spent) = if let Some(ref c) = client {
        account(&cfg, c).await.unwrap_or((0, 0, 0, 0))
    } else {
        (0, 0, 0, 0)
    };
    let reserved = if let Some(ref c) = client {
        locked(&cfg, c, &cfg.worker).await.unwrap_or(0)
    } else {
        0
    };
    Ok(PotStatus {
        configured: true,
        reachable,
        client,
        app_key,
        balance_micros: balance,
        max_per_job_micros: max_job,
        max_per_day_micros: max_day,
        spent_today_micros: spent,
        reserved_micros: reserved,
        rpc: cfg.rpc,
        pot: cfg.pot,
        usdc: cfg.usdc,
        chain_id: cfg.chain_id,
    })
}

/// Deposits this app saw MetaMask confirm. Local sqlite, not a chain scan.
pub async fn deposits(state: &AppState) -> Result<Vec<Deposit>> {
    let chain_id = load_chain_config(state).map(|c| c.chain_id).unwrap_or(0);
    Ok(state
        .db
        .list_deposits()?
        .into_iter()
        .map(|d| {
            let id = if d.chain_id != 0 { d.chain_id } else { chain_id };
            Deposit {
                url: explorer_tx(id, &d.tx_hash),
                tx_hash: d.tx_hash,
                amount_micros: d.amount_micros,
                max_per_job_micros: d.max_per_job_micros,
                max_per_day_micros: d.max_per_day_micros,
                block: d.block,
                at: d.at,
            }
        })
        .collect())
}

#[derive(Deserialize)]
struct DepositBody {
    tx_hash: String,
    amount_micros: u64,
    max_per_job_micros: u64,
    max_per_day_micros: u64,
    #[serde(default)]
    block: u64,
    #[serde(default)]
    chain_id: u64,
    #[serde(default)]
    client: String,
}

fn save_deposit(db_path: &Path, body: DepositBody) -> Result<()> {
    let tx_hash = body.tx_hash.trim().to_string();
    if tx_hash.is_empty() {
        return Err(AppError::Invalid("missing tx hash".into()));
    }
    let db = Db::open(db_path)?;
    db.record_deposit(&StoredDeposit {
        tx_hash,
        amount_micros: body.amount_micros,
        max_per_job_micros: body.max_per_job_micros,
        max_per_day_micros: body.max_per_day_micros,
        block: body.block,
        at: crate::store::now(),
        chain_id: body.chain_id,
        client: body.client,
    })
}

pub async fn check(state: &AppState, price: f64, unpriced: bool, kind: JobKind) -> Result<PotCheck> {
    if unpriced || price <= 0.0 {
        return Ok(PotCheck {
            ready: true,
            needs_fund: false,
            reason: String::new(),
            kind: "ok".into(),
            cap_micros: 0,
        });
    }
    let st = status(state).await?;
    let cap = if st.max_per_job_micros == 0 {
        DEFAULT_MAX_JOB
    } else {
        st.max_per_job_micros
    };
    if !st.configured || !st.reachable {
        return Ok(PotCheck {
            ready: false,
            needs_fund: true,
            reason: "Can't reach Base. Check the network, then deposit USDC in MetaMask.".into(),
            kind: "chain".into(),
            cap_micros: cap,
        });
    }
    if st.client.is_none() || st.balance_micros == 0 {
        return Ok(PotCheck {
            ready: false,
            needs_fund: true,
            reason: "This provider charges. Fund your pot once to continue.".into(),
            kind: "empty".into(),
            cap_micros: cap,
        });
    }
    let worst = worst_case_micros(price, kind);
    if worst > cap {
        return Ok(PotCheck {
            ready: false,
            needs_fund: false,
            reason: format!(
                "This reply could cost more than your ${:.2} limit for a single job. Raise that limit in your pot.",
                cap as f64 / 1_000_000.0
            ),
            kind: "cap".into(),
            cap_micros: cap,
        });
    }
    if worst > st.balance_micros.saturating_add(st.reserved_micros) {
        return Ok(PotCheck {
            ready: false,
            needs_fund: true,
            reason: format!(
                "Your pot does not cover this job. You have ${:.2} free; this reply could cost up to ${:.2}.",
                st.balance_micros as f64 / 1_000_000.0,
                worst as f64 / 1_000_000.0
            ),
            kind: "empty".into(),
            cap_micros: cap,
        });
    }
    Ok(PotCheck {
        ready: true,
        needs_fund: false,
        reason: String::new(),
        kind: "ok".into(),
        cap_micros: cap,
    })
}

fn worst_case_micros(price: f64, kind: JobKind) -> Micros {
    match kind {
        JobKind::Llm => {
            // MIN_ANSWER_TOKENS is the floor we actually send.
            let tokens = crate::jobs::MIN_ANSWER_TOKENS as f64;
            (tokens * price).round().max(1.0) as u64
        }
        JobKind::Image | JobKind::Video => (price * 1_000_000.0).round().max(1.0) as u64,
    }
}

/// Lock enough 1M-token slices to cover this job and return the signed bond.
/// The stream starts immediately; `pay_invoice` later captures the actual bill
/// (or a mid-stream top-up if the reply runs past the prepaid slices).
pub async fn issue_ticket(
    state: &AppState,
    job_id: Uuid,
    price: Price,
    kind: JobKind,
    payload: &JobPayload,
    client: &str,
    worker_payout: &str,
) -> Result<JobPay> {
    ensure_flush_loop(state.app_data.clone());
    let cfg = load_chain_config(state).ok_or_else(|| AppError::Invalid("no local chain".into()))?;
    let st = status(state).await?;
    let cap = if st.max_per_job_micros == 0 {
        DEFAULT_MAX_JOB
    } else {
        st.max_per_job_micros
    };
    let chunk = match kind {
        JobKind::Llm => price.chunk_micros(),
        JobKind::Image | JobKind::Video => (price.amount * 1_000_000.0).round().max(1.0) as u64,
    };
    let job_cap = job_lock_micros(&price, kind, payload).min(cap).max(1);
    // First signature covers as many 1M-token slices as the job needs, so
    // a normal reply never stops to wait for another ticket.
    let need = job_cap;
    let _ = chunk;
    ensure_reserve(state, &cfg, client, worker_payout, need).await?;

    let _gate = gate().lock().await;
    let (authorised, _on_chain) = authorised_so_far(&cfg, client, worker_payout).await;
    let cumulative = authorised.saturating_add(need);
    let signed = sign_latest(&state.app_data, &cfg, client, worker_payout, cumulative)?;

    jobs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            job_id,
            PendingJob {
                price,
                kind,
                paid: false,
                sha256: None,
                ceiling: need,
                job_cap,
            },
        );
    Ok(JobPay {
        v: rootmode_core::PROTOCOL_VERSION,
        job_id,
        ticket: signed.0,
        sig: format!("0x{}", hex::encode(signed.1)),
    })
}

fn job_lock_micros(price: &Price, kind: JobKind, payload: &JobPayload) -> Micros {
    match payload {
        JobPayload::Llm(params) => {
            let prompt = TokenUsage::measure(params, None, None, &[]).prompt;
            let tokens = prompt.saturating_add(params.max_tokens as u64);
            let chunks = tokens
                .div_ceil(rootmode_core::TOKEN_CHUNK)
                .max(1);
            chunks.saturating_mul(price.chunk_micros()).max(1)
        }
        _ => lock_micros(price, kind, payload).max(1),
    }
}

fn llm_charge_micros(price: &Price, meta: Option<&serde_json::Value>) -> Micros {
    let usage = meta.and_then(TokenUsage::from_meta).unwrap_or_else(|| {
        let n = meta
            .and_then(|m| m.get("total_tokens").or_else(|| m.get("completion_tokens")))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        TokenUsage {
            prompt: 0,
            completion: n,
            cached: 0,
            reasoning: 0,
        }
    });
    if usage.is_zero() {
        return 0;
    }
    price.charge_llm_micros(usage.prompt, usage.completion, usage.cached)
}

fn lock_micros(price: &Price, kind: JobKind, payload: &JobPayload) -> Micros {
    match payload {
        JobPayload::Llm(params) => {
            let prompt = TokenUsage::measure(params, None, None, &[]).prompt;
            price.charge_llm_micros(prompt, crate::jobs::MIN_ANSWER_TOKENS as u64, 0)
        }
        _ => worst_case_micros(price.amount, kind),
    }
}

async fn ensure_reserve(
    state: &AppState,
    cfg: &ChainConfig,
    client: &str,
    worker: &str,
    need: u64,
) -> Result<()> {
    let (reserved, paid) = channel(cfg, client, worker).await.unwrap_or((0, 0));
    let locked_amt = reserved.saturating_sub(paid);
    if locked_amt >= need {
        return Ok(());
    }
    let new_max = reserved.saturating_add(need.saturating_sub(locked_amt));
    let key = load_or_create_app_key(&state.app_data)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let ticket = ReserveTicket {
        client: client.to_string(),
        worker_payout: worker.to_string(),
        max_amount: new_max,
        deadline: now + TICKET_TTL_SECS,
    };
    let domain = Domain::for_chain(cfg.chain_id, cfg.pot.clone());
    let digest = ticket.digest(&domain)?;
    let (sig, rec) = key
        .sign_prehash(&digest)
        .map_err(|e| AppError::Invalid(e.to_string()))?;
    let mut bytes = sig.to_bytes().to_vec();
    bytes.push(rec.to_byte() + 27);
    let data = encode_app_call(
        b"reserve(address,address,uint256,uint64,bytes)",
        &ticket.client,
        &ticket.worker_payout,
        ticket.max_amount,
        ticket.deadline,
        &bytes,
    )?;
    let tx = serde_json::json!([{
        "from": cfg.worker,
        "to": cfg.pot,
        "data": format!("0x{}", hex::encode(data)),
        "gas": "0x7a120"
    }]);
    let hash = rpc(&cfg.rpc, "eth_sendTransaction", tx).await?;
    let hash = hash.as_str().unwrap_or_default().to_string();
    wait_ok(cfg, &hash).await?;
    log::info!("pot reserved {new_max} for {worker} {hash}");
    Ok(())
}

pub fn drop_job(job_id: Uuid) {
    jobs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&job_id);
}

/// Sign a SpendTicket for this invoice. Refuses an amount above the
/// advertised price × claimed tokens, the prepaid chunk, or the per-job cap.
pub async fn pay_invoice(state: &AppState, job_id: Uuid, invoice: &JobInvoice) -> Result<JobPay> {
    if invoice.job_id != job_id {
        return Err(AppError::Invalid("invoice is for a different job".into()));
    }
    if invoice.amount == 0 {
        return Err(AppError::Invalid("invoice has no amount".into()));
    }
    let pending = {
        let g = jobs().lock().unwrap_or_else(|e| e.into_inner());
        g.get(&job_id).cloned()
    };
    let Some(pending) = pending else {
        return Err(AppError::Invalid("no lock on file for this job".into()));
    };
    if pending.paid {
        return Err(AppError::Invalid("this job is already paid".into()));
    }

    let chunk = match pending.kind {
        JobKind::Llm => pending.price.chunk_micros(),
        JobKind::Image | JobKind::Video => {
            (pending.price.amount * 1_000_000.0).round().max(1.0) as u64
        }
    };

    if invoice.top_up {
        let room = pending.job_cap.saturating_sub(pending.ceiling);
        if room == 0 || invoice.amount > room {
            return Err(AppError::Invalid(
                format!(
                    "this reply reached your ${:.2} limit for a single job",
                    pending.job_cap as f64 / 1_000_000.0
                ),
            ));
        }
        if invoice.amount > chunk {
            return Err(AppError::Invalid(
                "top-up is larger than a 1M-token slice".into(),
            ));
        }
    } else {
        let fair = match pending.kind {
            JobKind::Llm => pending.price.charge_llm_micros(
                invoice.prompt_tokens,
                invoice.completion_tokens,
                invoice.cached_tokens,
            ),
            JobKind::Image | JobKind::Video => {
                (pending.price.amount * 1_000_000.0).round() as u64
            }
        };
        if invoice.amount > fair {
            return Err(AppError::Invalid(format!(
                "worker billed {} µUSDC; advertised price only covers {fair}",
                invoice.amount
            )));
        }
        if invoice.amount > pending.ceiling {
            return Err(AppError::Invalid(
                "invoice exceeds the prepaid slices for this job".into(),
            ));
        }
    }

    let _gate = gate().lock().await;
    let cfg = load_chain_config(state).ok_or_else(|| AppError::Invalid("no local chain".into()))?;
    let st = status(state).await?;
    let client = st
        .client
        .clone()
        .ok_or_else(|| AppError::Invalid("no funded pot".into()))?;
    let worker = cfg.worker.clone();
    let cap = if st.max_per_job_micros == 0 {
        DEFAULT_MAX_JOB
    } else {
        st.max_per_job_micros
    };
    if invoice.amount > cap {
        return Err(AppError::Invalid(format!(
            "invoice exceeds your ${:.2} per-job cap",
            cap as f64 / 1_000_000.0
        )));
    }

    ensure_flush_loop(state.app_data.clone());

    let (authorised, on_chain) = authorised_so_far(&cfg, &client, &worker).await;
    let extra = if invoice.top_up {
        pending.ceiling.saturating_add(invoice.amount)
    } else {
        invoice.amount
    };
    let wk = worker_key(&worker);
    let pending_delta = authorised.saturating_sub(on_chain);
    if !invoice.top_up && pending_delta > 0 && pending_delta.saturating_add(extra) > cap {
        flush_worker(&state.app_data, &cfg, &wk).await?;
    }

    let (authorised, on_chain) = authorised_so_far(&cfg, &client, &worker).await;
    let cumulative = authorised.saturating_add(extra);
    let signed = sign_latest(&state.app_data, &cfg, &client, &worker, cumulative)?;
    if !invoice.top_up {
        latest()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                wk,
                LatestTicket {
                    ticket: signed.0.clone(),
                    sig: signed.1.clone(),
                    on_chain_paid: on_chain,
                },
            );
        persist(&state.app_data);
        schedule_flush();
    }

    {
        let mut g = jobs().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(row) = g.get_mut(&job_id) {
            if invoice.top_up {
                row.ceiling = row.ceiling.saturating_add(invoice.amount);
            } else {
                row.paid = true;
                row.sha256 = Some(invoice.sha256.clone());
            }
        }
    }

    Ok(JobPay {
        v: rootmode_core::PROTOCOL_VERSION,
        job_id,
        ticket: signed.0,
        sig: format!("0x{}", hex::encode(&signed.1)),
    })
}

pub fn expected_sha256(job_id: Uuid) -> Option<String> {
    jobs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&job_id)
        .and_then(|j| j.sha256.clone())
}

/// Sign a new cumulative ticket for this job's cost. Does not settle on-chain
/// yet — [`flush_all`] does that every [`SETTLE_EVERY`], or sooner if the
/// pending delta would exceed the per-job cap.
pub async fn settle_job(
    state: &AppState,
    job_id: Uuid,
    meta: Option<&serde_json::Value>,
) -> Result<Option<String>> {
    let pending = {
        let mut g = jobs().lock().unwrap_or_else(|e| e.into_inner());
        g.remove(&job_id)
    };
    let Some(pending) = pending else {
        return Ok(None);
    };
    if pending.paid {
        return Ok(None);
    }
    let amount = match pending.kind {
        JobKind::Llm => llm_charge_micros(&pending.price, meta),
        JobKind::Image | JobKind::Video => (pending.price.amount * 1_000_000.0).round() as u64,
    };
    if amount == 0 {
        return Ok(None);
    }

    let _gate = gate().lock().await;
    let cfg = load_chain_config(state).ok_or_else(|| AppError::Invalid("no local chain".into()))?;
    let st = status(state).await?;
    let client = st
        .client
        .clone()
        .ok_or_else(|| AppError::Invalid("no funded pot".into()))?;
    let worker = cfg.worker.clone();
    let cap = if st.max_per_job_micros == 0 {
        DEFAULT_MAX_JOB
    } else {
        st.max_per_job_micros
    };
    let amount = amount.min(cap);

    ensure_flush_loop(state.app_data.clone());

    let wk = worker_key(&worker);
    let (authorised, on_chain) = authorised_so_far(&cfg, &client, &worker).await;
    let pending_delta = authorised.saturating_sub(on_chain);
    if pending_delta > 0 && pending_delta.saturating_add(amount) > cap {
        // This settle's delta is capped on-chain at maxPerJob, so flush
        // first and start a new interval from whatever the chain now shows.
        flush_worker(&state.app_data, &cfg, &wk).await?;
    }

    let (authorised, on_chain) = authorised_so_far(&cfg, &client, &worker).await;
    let cumulative = authorised.saturating_add(amount);
    let signed = sign_latest(&state.app_data, &cfg, &client, &worker, cumulative)?;
    latest()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            wk,
            LatestTicket {
                ticket: signed.0,
                sig: signed.1,
                on_chain_paid: on_chain,
            },
        );
    persist(&state.app_data);
    schedule_flush();
    Ok(None)
}

async fn authorised_so_far(cfg: &ChainConfig, client: &str, worker: &str) -> (u64, u64) {
    let cached = {
        let g = latest().lock().unwrap_or_else(|e| e.into_inner());
        g.get(&worker_key(worker))
            .map(|t| (t.ticket.cumulative, t.on_chain_paid))
    };
    match cached {
        Some(v) => v,
        None => {
            let paid = channel(cfg, client, worker)
                .await
                .map(|(_, paid)| paid)
                .unwrap_or(0);
            (paid, paid)
        }
    }
}

fn sign_latest(
    app_data: &Path,
    cfg: &ChainConfig,
    client: &str,
    worker: &str,
    cumulative: u64,
) -> Result<(SpendTicket, Vec<u8>)> {
    let key = load_or_create_app_key(app_data)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let ticket = SpendTicket {
        client: client.to_string(),
        worker_payout: worker.to_string(),
        cumulative,
        deadline: now + TICKET_TTL_SECS,
    };
    let domain = Domain::for_chain(cfg.chain_id, cfg.pot.clone());
    let digest = ticket.digest(&domain)?;
    let (sig, rec) = key
        .sign_prehash(&digest)
        .map_err(|e| AppError::Invalid(e.to_string()))?;
    let mut bytes = sig.to_bytes().to_vec();
    bytes.push(rec.to_byte() + 27);
    Ok((ticket, bytes))
}

fn schedule_flush() {
    // Debounce from the last job so a burst of chats is still one tx, but a
    // withdraw a few seconds later is not racing a 60s window.
    *flush_at().lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now() + SETTLE_EVERY);
}

fn ensure_flush_loop(app_data: PathBuf) {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let due = {
                let mut g = flush_at().lock().unwrap_or_else(|e| e.into_inner());
                match *g {
                    Some(t) if Instant::now() >= t => {
                        *g = None;
                        true
                    }
                    _ => false,
                }
            };
            if due {
                match flush_all(&app_data).await {
                    Ok(0) => {}
                    Ok(n) => log::info!("pot flushed {n} channel(s)"),
                    Err(e) => log::warn!("pot flush: {e}"),
                }
            }
        }
    });
}

pub async fn flush_all(app_data: &Path) -> Result<usize> {
    let _gate = gate().lock().await;
    let Some(cfg) = load_chain_config_at(app_data) else {
        return Ok(0);
    };
    let workers: Vec<String> = latest()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .cloned()
        .collect();
    let mut n = 0;
    for wk in workers {
        if flush_worker(app_data, &cfg, &wk).await?.is_some() {
            n += 1;
        }
    }
    Ok(n)
}

async fn flush_worker(app_data: &Path, cfg: &ChainConfig, wk: &str) -> Result<Option<String>> {
    let pending = {
        let g = latest().lock().unwrap_or_else(|e| e.into_inner());
        g.get(wk).map(|t| (t.ticket.clone(), t.sig.clone(), t.on_chain_paid))
    };
    let Some((ticket, sig, on_chain)) = pending else {
        return Ok(None);
    };
    if ticket.cumulative <= on_chain {
        return Ok(None);
    }
    let settled_upto = ticket.cumulative;
    let hash = match send_settle(cfg, &ticket, &sig).await {
        Ok(h) => h,
        Err(e) => {
            let msg = e.to_string();
            if msg.to_lowercase().contains("expired") {
                let (fresh, fresh_sig) =
                    sign_latest(app_data, cfg, &ticket.client, &ticket.worker_payout, ticket.cumulative)?;
                let hash = send_settle(cfg, &fresh, &fresh_sig).await?;
                latest()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(
                        wk.to_string(),
                        LatestTicket {
                            ticket: fresh,
                            sig: fresh_sig,
                            on_chain_paid: settled_upto,
                        },
                    );
                persist(app_data);
                log::info!("pot settled {hash}");
                return Ok(Some(hash));
            }
            return Err(e);
        }
    };
    latest()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            wk.to_string(),
            LatestTicket {
                ticket,
                sig,
                on_chain_paid: settled_upto,
            },
        );
    persist(app_data);
    log::info!("pot settled {hash}");
    Ok(Some(hash))
}

async fn send_settle(cfg: &ChainConfig, ticket: &SpendTicket, sig: &[u8]) -> Result<String> {
    let data = encode_settle(ticket, sig)?;
    let tx = serde_json::json!([{
        "from": cfg.worker,
        "to": cfg.pot,
        "data": format!("0x{}", hex::encode(data)),
        "gas": "0x7a120"
    }]);
    let hash = rpc(&cfg.rpc, "eth_sendTransaction", tx).await?;
    let hash = hash.as_str().unwrap_or_default().to_string();
    wait_ok(cfg, &hash).await?;
    Ok(hash)
}

async fn wait_ok(cfg: &ChainConfig, hash: &str) -> Result<()> {
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(150)).await;
        let rec = rpc(&cfg.rpc, "eth_getTransactionReceipt", serde_json::json!([hash])).await?;
        if rec.is_null() {
            continue;
        }
        let status = rec.get("status").and_then(|s| s.as_str()).unwrap_or("0x0");
        if status == "0x1" {
            return Ok(());
        }
        return Err(AppError::Net("settle reverted".into()));
    }
    Err(AppError::Net("settle not mined".into()))
}

pub fn fund_url(state: &AppState) -> Result<String> {
    let cfg = load_chain_config(state).ok_or_else(|| {
        AppError::Invalid("settlement is not configured on this build".into())
    })?;
    let app_key = app_key_address(state)?;
    Ok(format!(
        "http://127.0.0.1:{FUND_PORT}/?rpc={}&pot={}&usdc={}&appKey={}&chainId={}&worker={}",
        urlencoding_lite(&cfg.rpc),
        cfg.pot,
        cfg.usdc,
        app_key,
        cfg.chain_id,
        cfg.worker
    ))
}

fn urlencoding_lite(s: &str) -> String {
    s.replace(':', "%3A").replace('/', "%2F")
}

#[derive(Clone, Serialize)]
struct PendingPublic {
    worker: String,
    cumulative: u64,
    deadline: u64,
    sig: String,
}

fn pending_public() -> Vec<PendingPublic> {
    latest()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .filter(|t| t.ticket.cumulative > t.on_chain_paid)
        .map(|t| PendingPublic {
            worker: t.ticket.worker_payout.clone(),
            cumulative: t.ticket.cumulative,
            deadline: t.ticket.deadline,
            sig: format!("0x{}", hex::encode(&t.sig)),
        })
        .collect()
}

pub fn ensure_fund_server(app_data: PathBuf) -> Result<()> {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.get().is_some() {
        return Ok(());
    }
    let html = FUND_HTML.to_string();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("fund server runtime");
        rt.block_on(async move {
            let page = html.clone();
            let data = app_data.clone();
            let db_path = app_data.join("rootmode.sqlite");
            let app = axum::Router::new()
                .route(
                    "/",
                    axum::routing::get(move || async move {
                        (
                            [(
                                axum::http::header::CONTENT_TYPE,
                                "text/html; charset=utf-8",
                            )],
                            page,
                        )
                    }),
                )
                .route(
                    "/pending",
                    axum::routing::get(|| async { axum::Json(pending_public()) }),
                )
                .route(
                    "/flush",
                    axum::routing::post(move || {
                        let data = data.clone();
                        async move {
                            match flush_all(&data).await {
                                Ok(n) => (axum::http::StatusCode::OK, n.to_string()),
                                Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
                            }
                        }
                    }),
                )
                .route(
                    "/deposit",
                    axum::routing::post({
                        let db_path = db_path.clone();
                        move |axum::Json(body): axum::Json<DepositBody>| {
                            let db_path = db_path.clone();
                            async move {
                                match save_deposit(&db_path, body) {
                                    Ok(()) => (axum::http::StatusCode::NO_CONTENT, String::new()),
                                    Err(e) => {
                                        log::warn!("record deposit: {e}");
                                        (
                                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                            e.to_string(),
                                        )
                                    }
                                }
                            }
                        }
                    }),
                );
            match tokio::net::TcpListener::bind(("127.0.0.1", FUND_PORT)).await {
                Ok(listener) => {
                    let _ = axum::serve(listener, app).await;
                }
                Err(e) => log::error!(
                    "fund page already bound ({e}) — quit the other rootmode and reopen"
                ),
            }
        });
    });
    let _ = STARTED.set(());
    // Give the listener a tick.
    std::thread::sleep(std::time::Duration::from_millis(80));
    Ok(())
}

pub async fn open_fund(app: &AppHandle) -> Result<String> {
    let state = app.state::<std::sync::Arc<AppState>>().inner().clone();
    ensure_fund_server(state.app_data.clone())?;
    ensure_flush_loop(state.app_data.clone());
    // Settle anything pending before the user can withdraw the pot.
    if let Err(e) = flush_all(&state.app_data).await {
        log::warn!("pot flush before fund: {e}");
    }
    let url = fund_url(&state)?;
    // MetaMask lives in Chrome/Brave, not Safari (often the macOS default).
    #[cfg(target_os = "macos")]
    {
        for app_name in ["Google Chrome", "Brave Browser", "Microsoft Edge"] {
            if std::process::Command::new("open")
                .args(["-a", app_name, &url])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            {
                return Ok(url);
            }
        }
    }
    tauri_plugin_opener::OpenerExt::opener(app)
        .open_url(&url, None::<&str>)
        .map_err(|e| AppError::Invalid(e.to_string()))?;
    Ok(url)
}

async fn find_client(cfg: &ChainConfig, app_key: &str) -> Option<String> {
    let logs = deposited_logs(cfg).await.ok()?;
    let want = app_key.trim_start_matches("0x").to_lowercase();
    logs.iter()
        .rev()
        .find(|l| l.app_key.trim_start_matches("0x").eq_ignore_ascii_case(&want))
        .map(|l| l.client.clone())
}

async fn deposited_logs(cfg: &ChainConfig) -> Result<Vec<DepositLog>> {
    // Deposited(address indexed client, uint256, uint256, uint256, address)
    let topic0 = format!(
        "0x{}",
        hex::encode(keccak(
            b"Deposited(address,uint256,uint256,uint256,address)"
        ))
    );
    let logs = rpc(
        &cfg.rpc,
        "eth_getLogs",
        serde_json::json!([{
            "address": cfg.pot,
            "fromBlock": "0x0",
            "toBlock": "latest",
            "topics": [topic0]
        }]),
    )
    .await?;
    let mut out = Vec::new();
    let Some(arr) = logs.as_array() else {
        return Ok(out);
    };
    for log in arr {
        if let Some(parsed) = parse_deposit_log(log) {
            out.push(parsed);
        }
    }
    Ok(out)
}

fn parse_deposit_log(log: &serde_json::Value) -> Option<DepositLog> {
    let data = log.get("data")?.as_str()?.trim_start_matches("0x");
    if data.len() < 64 * 4 {
        return None;
    }
    let t = log.get("topics")?.as_array()?.get(1)?.as_str()?;
    let hex = t.trim_start_matches("0x");
    if hex.len() < 40 {
        return None;
    }
    let key_word = &data[64 * 3..64 * 4];
    Some(DepositLog {
        client: format!("0x{}", &hex[hex.len() - 40..]),
        app_key: format!("0x{}", &key_word[key_word.len() - 40..]),
    })
}

fn explorer_tx(chain_id: u64, hash: &str) -> Option<String> {
    let hash = if hash.starts_with("0x") || hash.starts_with("0X") {
        hash.to_string()
    } else {
        format!("0x{hash}")
    };
    match chain_id {
        8453 => Some(format!("https://basescan.org/tx/{hash}")),
        84532 => Some(format!("https://sepolia.basescan.org/tx/{hash}")),
        _ => None,
    }
}

async fn account(cfg: &ChainConfig, client: &str) -> Result<(u64, u64, u64, u64)> {
    let mut data = Vec::from(hex::decode("5e5c06e2").unwrap());
    data.extend_from_slice(&word_address(client)?);
    let raw = rpc(
        &cfg.rpc,
        "eth_call",
        serde_json::json!([{ "to": cfg.pot, "data": format!("0x{}", hex::encode(data)) }, "latest"]),
    )
    .await?;
    let hex = raw.as_str().unwrap_or("0x").trim_start_matches("0x");
    if hex.len() < 64 * 4 {
        return Ok((0, 0, 0, 0));
    }
    Ok((
        read_u64(&hex[0..64]),
        read_u64(&hex[64..128]),
        read_u64(&hex[128..192]),
        read_u64(&hex[192..256]),
    ))
}

async fn channel(cfg: &ChainConfig, client: &str, worker: &str) -> Result<(u64, u64)> {
    let sel = &keccak(b"channels(address,address)")[..4];
    let mut data = Vec::from(sel);
    data.extend_from_slice(&word_address(client)?);
    data.extend_from_slice(&word_address(worker)?);
    let raw = rpc(
        &cfg.rpc,
        "eth_call",
        serde_json::json!([{ "to": cfg.pot, "data": format!("0x{}", hex::encode(data)) }, "latest"]),
    )
    .await?;
    let hex = raw.as_str().unwrap_or("0x").trim_start_matches("0x");
    if hex.len() < 128 {
        return Ok((0, 0));
    }
    Ok((read_u64(&hex[0..64]), read_u64(&hex[64..128])))
}

async fn locked(cfg: &ChainConfig, client: &str, worker: &str) -> Result<u64> {
    let sel = &keccak(b"locked(address,address)")[..4];
    let mut data = Vec::from(sel);
    data.extend_from_slice(&word_address(client)?);
    data.extend_from_slice(&word_address(worker)?);
    let raw = rpc(
        &cfg.rpc,
        "eth_call",
        serde_json::json!([{ "to": cfg.pot, "data": format!("0x{}", hex::encode(data)) }, "latest"]),
    )
    .await?;
    let hex = raw.as_str().unwrap_or("0x").trim_start_matches("0x");
    if hex.len() < 64 {
        return Ok(0);
    }
    Ok(read_u64(&hex[0..64]))
}

fn read_u64(word: &str) -> u64 {
    u64::from_str_radix(word.trim_start_matches('0').get(..).unwrap_or("0"), 16).unwrap_or(0)
}

fn word_address(addr: &str) -> Result<[u8; 32]> {
    let raw = addr.trim_start_matches("0x");
    let bytes = hex::decode(raw).map_err(|e| AppError::Invalid(e.to_string()))?;
    if bytes.len() != 20 {
        return Err(AppError::Invalid(format!("not an address: {addr}")));
    }
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(&bytes);
    Ok(word)
}

fn encode_settle(ticket: &SpendTicket, sig: &[u8]) -> Result<Vec<u8>> {
    encode_app_call(
        b"settle(address,address,uint256,uint64,bytes)",
        &ticket.client,
        &ticket.worker_payout,
        ticket.cumulative,
        ticket.deadline,
        sig,
    )
}

fn encode_app_call(
    signature: &[u8],
    client: &str,
    worker: &str,
    amount: u64,
    deadline: u64,
    sig: &[u8],
) -> Result<Vec<u8>> {
    let sel = &keccak(signature)[..4];
    let mut out = Vec::from(sel);
    out.extend_from_slice(&word_address(client)?);
    out.extend_from_slice(&word_address(worker)?);
    out.extend_from_slice(&word_u256(amount));
    out.extend_from_slice(&word_u256(deadline));
    out.extend_from_slice(&word_u256(5 * 32));
    out.extend_from_slice(&word_u256(sig.len() as u64));
    let mut padded = sig.to_vec();
    while padded.len() % 32 != 0 {
        padded.push(0);
    }
    out.extend_from_slice(&padded);
    Ok(out)
}

fn word_u256(v: u64) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[24..].copy_from_slice(&v.to_be_bytes());
    w
}

async fn rpc(url: &str, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let resp = reqwest::Client::new()
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|e| AppError::Net(e.to_string()))?;
    let v: serde_json::Value = resp.json().await.map_err(|e| AppError::Net(e.to_string()))?;
    if let Some(err) = v.get("error") {
        return Err(AppError::Net(err.to_string()));
    }
    Ok(v.get("result").cloned().unwrap_or(serde_json::Value::Null))
}

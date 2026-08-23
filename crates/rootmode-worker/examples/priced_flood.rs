//! Drive N priced chats against a local worker and print on-chain math.
//!
//! ```sh
//! cargo run -p rootmode-worker --example priced_flood -- 100
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use k256::ecdsa::{signature::hazmat::PrehashSigner, SigningKey};
use rootmode_core::{
    payments::{keccak, Domain, ReserveTicket, SpendTicket},
    protocol::{ClientMessage, JobPay, PeerHello},
    ChatMessage, Identity, JobPayload, JobStatus, JobSubmit, LlmParams, WorkerMessage,
    PROTOCOL_VERSION,
};
use serde::Deserialize;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

#[derive(Deserialize)]
struct Chain {
    rpc: String,
    #[serde(rename = "chainId")]
    chain_id: u64,
    pot: String,
    worker: String,
    client: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let n: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    let ws_url = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "ws://127.0.0.1:9944".into());

    let home = PathBuf::from(std::env::var("HOME")?);
    let chain: Chain = serde_json::from_str(&std::fs::read_to_string(
        home.join(".rootmode/local-chain.json"),
    )?)?;
    let key_path = home.join("Library/Application Support/ai.rootmode.desktop/pot-app.key");
    let key = load_key(&key_path)?;
    let domain = Domain::for_chain(chain.chain_id, chain.pot.clone());
    let identity = Identity::generate();
    let http = reqwest::Client::new();

    let before = snapshot(&http, &chain).await?;
    println!("before  earned={} paid={} reserved={} locked={}", before.earned, before.paid, before.reserved, before.locked);
    println!("        worker={} vault={} pot={}", before.worker_bal, before.vault_bal, before.pot_bal);
    println!("flood   {n} jobs → {ws_url}");

    let mut authorised = before.earned;
    let mut billed: u64 = 0;
    let mut ok = 0u32;
    let start = Instant::now();

    for i in 1..=n {
        let job_id = Uuid::new_v4();
        let ch = snapshot(&http, &chain).await?;
        let need = 500_000u64; // per-job cap; fake-vllm bills far less
        let unused = ch.reserved.saturating_sub(ch.paid);
        if unused < need {
            let new_max = ch.reserved.saturating_add(need - unused);
            reserve(&http, &chain, &key, &domain, new_max).await?;
        }

        let bond_cum = authorised.saturating_add(need);
        if bond_cum <= authorised {
            return Err("bond did not rise".into());
        }
        let bond = sign_pay(&key, &domain, &chain, job_id, bond_cum)?;

        let payload = JobPayload::Llm(LlmParams {
            model_hash: None,
            model_id: Some("local-test".into()),
            messages: vec![ChatMessage::new("user", format!("flood {i}"))],
            tools: Vec::new(),
            max_tokens: 64,
            temperature: 0.0,
        });
        let mut submit = JobSubmit::new(job_id, identity.peer_id(), payload);
        submit.payer = Some(chain.client.clone());
        submit.bond = Some(bond);
        let submit = submit.signed_by(&identity)?;

        let (mut ws, _) = tokio_tungstenite::connect_async(&ws_url).await?;
        ws.send(Message::Text(serde_json::to_string(&ClientMessage::PeerHello(
            PeerHello {
                v: PROTOCOL_VERSION,
                peer_id: identity.peer_id(),
            },
        ))?))
        .await?;
        ws.send(Message::Text(serde_json::to_string(&ClientMessage::JobSubmit(
            submit,
        ))?))
        .await?;

        let mut invoice_amount: Option<u64> = None;
        let mut done = false;
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline && !done {
            let Some(frame) = tokio::time::timeout(Duration::from_secs(5), ws.next()).await? else {
                break;
            };
            let Message::Text(text) = frame? else {
                continue;
            };
            match WorkerMessage::parse(&text) {
                Ok(WorkerMessage::JobInvoice(inv)) if !inv.top_up => {
                    invoice_amount = Some(inv.amount);
                    let pay_cum = authorised.saturating_add(inv.amount);
                    let pay = sign_pay(&key, &domain, &chain, job_id, pay_cum)?;
                    ws.send(Message::Text(serde_json::to_string(&ClientMessage::JobPay(
                        pay,
                    ))?))
                    .await?;
                }
                Ok(WorkerMessage::JobStatus(s)) if s.status == JobStatus::Failed => {
                    let err = s.error.unwrap_or_default();
                    return Err(format!("job {i} failed: {err}").into());
                }
                Ok(WorkerMessage::JobStatus(s)) if s.status == JobStatus::Done => {
                    done = true;
                }
                Ok(_) | Err(_) => {}
            }
        }
        let _ = ws.close(None).await;
        let Some(amount) = invoice_amount else {
            return Err(format!("job {i} produced no invoice").into());
        };
        if !done {
            return Err(format!("job {i} never reached done").into());
        }
        authorised += amount;
        billed += amount;
        ok += 1;
        if i == 1 || i == n || i % 10 == 0 {
            println!("  {i:>3}/{n}  invoice={amount}  billed_so_far={billed}  authorised={authorised}");
        }
    }

    // Worker settle is async; wait for the last receipt to land.
    tokio::time::sleep(Duration::from_millis(800)).await;
    let after = snapshot(&http, &chain).await?;
    let elapsed = start.elapsed();

    let d_earned = after.earned.saturating_sub(before.earned);
    let d_worker = after.worker_bal.saturating_sub(before.worker_bal);
    let d_vault = after.vault_bal.saturating_sub(before.vault_bal);
    let d_pot = before.pot_bal.saturating_sub(after.pot_bal);

    println!();
    println!("jobs     {ok}/{n} in {:.1}s", elapsed.as_secs_f64());
    println!("invoices {billed} micros  (${:.4})", billed as f64 / 1e6);
    println!("chain    earned +{d_earned}  worker +{d_worker}  vault +{d_vault}  pot -{d_pot}");
    println!(
        "after    earned={} paid={} reserved={} locked={}",
        after.earned, after.paid, after.reserved, after.locked
    );

    let fee = billed / 10;
    let to_worker = billed - fee;
    let mut failed = false;
    if d_earned != billed {
        println!("FAIL earned delta {d_earned} != billed {billed}");
        failed = true;
    }
    if after.paid != after.earned {
        println!("FAIL paid {} != earned {}", after.paid, after.earned);
        failed = true;
    }
    if d_worker != to_worker {
        println!("FAIL worker +{d_worker} != 90% {to_worker}");
        failed = true;
    }
    if d_vault != fee {
        println!("FAIL vault +{d_vault} != 10% {fee}");
        failed = true;
    }
    if d_pot != billed {
        println!("FAIL pot -{d_pot} != billed {billed}");
        failed = true;
    }
    if d_worker + d_vault != billed {
        println!("FAIL worker+vault {} != billed {billed}", d_worker + d_vault);
        failed = true;
    }
    if failed {
        std::process::exit(1);
    }
    println!("OK  90/10 split and cumulative earned match {ok} invoices");
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct Snap {
    earned: u64,
    paid: u64,
    reserved: u64,
    locked: u64,
    worker_bal: u64,
    vault_bal: u64,
    pot_bal: u64,
}

async fn snapshot(http: &reqwest::Client, chain: &Chain) -> Result<Snap, Box<dyn std::error::Error>> {
    let (reserved, paid, _d, _c, _k, earned) = channel(http, chain).await?;
    let locked = reserved.saturating_sub(paid);
    Ok(Snap {
        earned,
        paid,
        reserved,
        locked,
        worker_bal: balance(http, &chain.rpc, usdc(http, chain).await?, &chain.worker).await?,
        vault_bal: balance(http, &chain.rpc, usdc(http, chain).await?, &fee_vault(http, chain).await?).await?,
        pot_bal: balance(http, &chain.rpc, usdc(http, chain).await?, &chain.pot).await?,
    })
}

async fn usdc(http: &reqwest::Client, chain: &Chain) -> Result<String, Box<dyn std::error::Error>> {
    let raw = eth_call(http, &chain.rpc, &chain.pot, &keccak(b"usdc()")[..4]).await?;
    Ok(format!("0x{}", &raw[raw.len() - 40..]))
}

async fn fee_vault(http: &reqwest::Client, chain: &Chain) -> Result<String, Box<dyn std::error::Error>> {
    let raw = eth_call(http, &chain.rpc, &chain.pot, &keccak(b"feeVault()")[..4]).await?;
    Ok(format!("0x{}", &raw[raw.len() - 40..]))
}

async fn channel(
    http: &reqwest::Client,
    chain: &Chain,
) -> Result<(u64, u64, u64, u64, String, u64), Box<dyn std::error::Error>> {
    let mut data = keccak(b"channels(address,address)")[..4].to_vec();
    data.extend_from_slice(&word_addr(&chain.client)?);
    data.extend_from_slice(&word_addr(&chain.worker)?);
    let hex = eth_call(http, &chain.rpc, &chain.pot, &data).await?;
    if hex.len() < 64 * 6 {
        return Err("short channels() return".into());
    }
    Ok((
        u64_word(&hex[0..64]),
        u64_word(&hex[64..128]),
        u64_word(&hex[128..192]),
        u64_word(&hex[192..256]),
        format!("0x{}", &hex[256 + 24..320]),
        u64_word(&hex[320..384]),
    ))
}

async fn balance(
    http: &reqwest::Client,
    rpc: &str,
    token: String,
    who: &str,
) -> Result<u64, Box<dyn std::error::Error>> {
    let mut data = keccak(b"balanceOf(address)")[..4].to_vec();
    data.extend_from_slice(&word_addr(who)?);
    let hex = eth_call(http, rpc, &token, &data).await?;
    Ok(u64_word(&hex[hex.len().saturating_sub(64)..]))
}

async fn reserve(
    http: &reqwest::Client,
    chain: &Chain,
    key: &SigningKey,
    domain: &Domain,
    max_amount: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let ticket = ReserveTicket {
        client: chain.client.clone(),
        worker_payout: chain.worker.clone(),
        max_amount,
        deadline: now + 3600,
    };
    let sig = sign(key, &ticket.digest(domain)?)?;
    let data = encode_call(
        b"reserve(address,address,uint256,uint64,bytes)",
        &ticket.client,
        &ticket.worker_payout,
        ticket.max_amount,
        ticket.deadline,
        &sig,
    )?;
    send_tx(http, chain, data).await
}

fn sign_pay(
    key: &SigningKey,
    domain: &Domain,
    chain: &Chain,
    job_id: Uuid,
    cumulative: u64,
) -> Result<JobPay, Box<dyn std::error::Error>> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let ticket = SpendTicket {
        client: chain.client.clone(),
        worker_payout: chain.worker.clone(),
        cumulative,
        deadline: now + 3600,
    };
    let sig = sign(key, &ticket.digest(domain)?)?;
    Ok(JobPay {
        v: PROTOCOL_VERSION,
        job_id,
        ticket,
        sig: format!("0x{}", hex::encode(sig)),
    })
}

fn sign(key: &SigningKey, digest: &[u8; 32]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let (sig, rec) = key.sign_prehash(digest)?;
    let mut bytes = sig.to_bytes().to_vec();
    bytes.push(rec.to_byte() + 27);
    Ok(bytes)
}

fn load_key(path: &std::path::Path) -> Result<SigningKey, Box<dyn std::error::Error>> {
    let hex = std::fs::read_to_string(path)?;
    let bytes = hex::decode(hex.trim())?;
    if bytes.len() != 32 {
        return Err("app key is not 32 bytes".into());
    }
    Ok(SigningKey::from_bytes(bytes.as_slice().into())?)
}

fn encode_call(
    signature: &[u8],
    client: &str,
    worker: &str,
    amount: u64,
    deadline: u64,
    sig: &[u8],
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut out = keccak(signature)[..4].to_vec();
    out.extend_from_slice(&word_addr(client)?);
    out.extend_from_slice(&word_addr(worker)?);
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

async fn send_tx(
    http: &reqwest::Client,
    chain: &Chain,
    data: Vec<u8>,
) -> Result<(), Box<dyn std::error::Error>> {
    let hash = rpc(
        http,
        &chain.rpc,
        "eth_sendTransaction",
        serde_json::json!([{
            "from": chain.worker,
            "to": chain.pot,
            "data": format!("0x{}", hex::encode(data)),
            "gas": "0x7a120"
        }]),
    )
    .await?;
    let hash = hash.as_str().ok_or("no tx hash")?.to_string();
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(150)).await;
        let rec = rpc(
            http,
            &chain.rpc,
            "eth_getTransactionReceipt",
            serde_json::json!([hash]),
        )
        .await?;
        if rec.is_null() {
            continue;
        }
        if rec.get("status").and_then(|s| s.as_str()) == Some("0x1") {
            return Ok(());
        }
        return Err(format!("tx reverted {hash}").into());
    }
    Err("tx not mined".into())
}

async fn eth_call(
    http: &reqwest::Client,
    rpc_url: &str,
    to: &str,
    data: &[u8],
) -> Result<String, Box<dyn std::error::Error>> {
    let v = rpc(
        http,
        rpc_url,
        "eth_call",
        serde_json::json!([
            { "to": to, "data": format!("0x{}", hex::encode(data)) },
            "latest"
        ]),
    )
    .await?;
    Ok(v.as_str().unwrap_or("0x").trim_start_matches("0x").to_string())
}

async fn rpc(
    http: &reqwest::Client,
    url: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":method,"params":params});
    let resp: serde_json::Value = http.post(url).json(&body).send().await?.json().await?;
    if let Some(err) = resp.get("error") {
        return Err(format!("rpc {method}: {err}").into());
    }
    Ok(resp.get("result").cloned().unwrap_or(serde_json::Value::Null))
}

fn word_addr(addr: &str) -> Result<[u8; 32], Box<dyn std::error::Error>> {
    let raw = addr.trim_start_matches("0x");
    let bytes = hex::decode(raw)?;
    if bytes.len() != 20 {
        return Err("not an address".into());
    }
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(&bytes);
    Ok(w)
}

fn word_u256(v: u64) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[24..].copy_from_slice(&v.to_be_bytes());
    w
}

fn u64_word(word: &str) -> u64 {
    u64::from_str_radix(word.trim_start_matches('0').get(..).unwrap_or("0"), 16).unwrap_or(0)
}

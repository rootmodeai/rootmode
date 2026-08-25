//! On-chain lock check and settle for the pot.
//!
//! The GPU does not start against a priced job unless the payer still has
//! unused reserve. After `job.pay`, this node signs `settle` with its
//! Ethereum pay key (`payments.key` / `ROOTMODE_PAY_KEY`) and sends the raw
//! transaction, so collection does not depend on a forked client.

use k256::ecdsa::{signature::hazmat::PrehashSigner, SigningKey};
use rootmode_core::payments::{address_of, keccak, ReserveTicket, SpendTicket};

use crate::config::PaymentsConfig;
use crate::error::{Result, WorkerError};

/// The on-chain channel: remaining billable lock (`reserved - earned`) and the
/// `appKey` the pot checks `settle` signatures against. `None` when there is no
/// RPC to look; the app key is the zero address when no reserve has been mined
/// for this pair yet.
#[derive(Debug, Clone)]
pub struct ChannelState {
    pub remaining: u64,
    pub app_key: String,
}

/// True for the zero address (an empty on-chain app-key slot).
pub fn is_zero_address(addr: &str) -> bool {
    addr.trim_start_matches("0x").chars().all(|c| c == '0')
}

pub async fn channel_state(
    payments: &PaymentsConfig,
    client: &str,
    worker: &str,
) -> Result<Option<ChannelState>> {
    if payments.rpc.trim().is_empty() || payments.contract.trim().is_empty() {
        return Ok(None);
    }
    let sel = &keccak(b"channels(address,address)")[..4];
    let mut data = Vec::from(sel);
    data.extend_from_slice(&word_address(client)?);
    data.extend_from_slice(&word_address(worker)?);
    let raw = rpc(
        &payments.rpc,
        "eth_call",
        serde_json::json!([
            { "to": payments.contract, "data": format!("0x{}", hex::encode(data)) },
            "latest"
        ]),
    )
    .await?;
    let hex = raw.as_str().unwrap_or("0x").trim_start_matches("0x");
    // six words: reserved, paid, deadline, closeAt, appKey, earned
    if hex.len() < 64 * 6 {
        return Ok(Some(ChannelState {
            remaining: 0,
            app_key: String::new(),
        }));
    }
    let reserved = read_u64(&hex[0..64]);
    let close_at = read_u64(&hex[64 * 3..64 * 4]);
    // The address is the low 20 bytes (last 40 hex chars) of word 4.
    let app_key = format!("0x{}", &hex[64 * 4 + 24..64 * 5]);
    let earned = read_u64(&hex[64 * 5..64 * 6]);
    if close_at != 0 {
        return Err(WorkerError::Rejected(
            "this client is closing the channel; unused lock is returning".into(),
        ));
    }
    Ok(Some(ChannelState {
        remaining: reserved.saturating_sub(earned),
        app_key,
    }))
}

/// Post a client-signed `reserve()`. Anyone may call it; this node does
/// because it already holds ETH for `settle`.
pub async fn reserve(payments: &PaymentsConfig, ticket: &ReserveTicket, sig: &[u8]) -> Result<String> {
    let data = encode_call(
        b"reserve(address,address,uint256,uint64,bytes)",
        &ticket.client,
        &ticket.worker_payout,
        ticket.max_amount,
        ticket.deadline,
        sig,
    )?;
    let hash = send_call(payments, data).await?;
    wait_ok(&payments.rpc, &hash).await?;
    Ok(hash)
}

pub async fn settle(payments: &PaymentsConfig, ticket: &SpendTicket, sig: &[u8]) -> Result<String> {
    let data = encode_call(
        b"settle(address,address,uint256,uint64,bytes)",
        &ticket.client,
        &ticket.worker_payout,
        ticket.cumulative,
        ticket.deadline,
        sig,
    )?;
    let hash = send_call(payments, data).await?;
    wait_ok(&payments.rpc, &hash).await?;
    Ok(hash)
}

async fn send_call(payments: &PaymentsConfig, data: Vec<u8>) -> Result<String> {
    if let Some(key) = signing_key(payments)? {
        return send_signed(payments, &key, data).await;
    }
    let from = {
        let s = payments.sender.trim();
        if s.is_empty() {
            return Err(WorkerError::Rejected(
                "set payments.key (or ROOTMODE_PAY_KEY) so this node can sign chain calls".into(),
            ));
        }
        s.to_string()
    };
    let tx = serde_json::json!([{
        "from": from,
        "to": payments.contract,
        "data": format!("0x{}", hex::encode(data)),
        "gas": "0x7a120"
    }]);
    let hash = rpc(&payments.rpc, "eth_sendTransaction", tx).await?;
    Ok(hash.as_str().unwrap_or_default().to_string())
}

fn signing_key(payments: &PaymentsConfig) -> Result<Option<SigningKey>> {
    let raw = payments.key.trim().trim_start_matches("0x");
    if raw.is_empty() || raw.starts_with("${") {
        return Ok(None);
    }
    let bytes = hex::decode(raw).map_err(|e| WorkerError::Config(format!("payments.key: {e}")))?;
    if bytes.len() != 32 {
        return Err(WorkerError::Config("payments.key must be 32 bytes".into()));
    }
    SigningKey::from_bytes((&bytes[..]).into())
        .map(Some)
        .map_err(|e| WorkerError::Config(format!("payments.key: {e}")))
}

async fn send_signed(payments: &PaymentsConfig, key: &SigningKey, data: Vec<u8>) -> Result<String> {
    let from = address_of(key.verifying_key());
    let to = parse_address(&payments.contract)?;
    let nonce = parse_u64_hex(
        rpc(
            &payments.rpc,
            "eth_getTransactionCount",
            serde_json::json!([from, "pending"]),
        )
        .await?
        .as_str()
        .unwrap_or("0x0"),
    )?;
    let gas_price = parse_u64_hex(
        rpc(&payments.rpc, "eth_gasPrice", serde_json::json!([]))
            .await?
            .as_str()
            .unwrap_or("0x0"),
    )?;
    if gas_price == 0 {
        return Err(WorkerError::Net("eth_gasPrice returned 0".into()));
    }
    let tx = LegacyTx {
        nonce,
        gas_price,
        gas: 500_000,
        to,
        value: 0,
        data,
        chain_id: payments.chain_id,
    };
    let raw = sign_legacy(key, &tx)?;
    let hash = rpc(
        &payments.rpc,
        "eth_sendRawTransaction",
        serde_json::json!([format!("0x{}", hex::encode(raw))]),
    )
    .await?;
    Ok(hash.as_str().unwrap_or_default().to_string())
}

struct LegacyTx {
    nonce: u64,
    gas_price: u64,
    gas: u64,
    to: [u8; 20],
    value: u64,
    data: Vec<u8>,
    chain_id: u64,
}

fn sign_legacy(key: &SigningKey, tx: &LegacyTx) -> Result<Vec<u8>> {
    let digest = keccak(&legacy_rlp(tx, None));
    let (sig, recovery) = key
        .sign_prehash(&digest)
        .map_err(|e| WorkerError::Config(format!("cannot sign settle: {e}")))?;
    let v = tx.chain_id.saturating_mul(2).saturating_add(35) + u64::from(recovery.to_byte());
    let bytes = sig.to_bytes();
    Ok(legacy_rlp(tx, Some((v, &bytes[..32], &bytes[32..]))))
}

fn legacy_rlp(tx: &LegacyTx, sig: Option<(u64, &[u8], &[u8])>) -> Vec<u8> {
    let mut items = vec![
        rlp_u64(tx.nonce),
        rlp_u64(tx.gas_price),
        rlp_u64(tx.gas),
        rlp_bytes(&tx.to),
        rlp_u64(tx.value),
        rlp_bytes(&tx.data),
    ];
    match sig {
        None => {
            items.push(rlp_u64(tx.chain_id));
            items.push(rlp_u64(0));
            items.push(rlp_u64(0));
        }
        Some((v, r, s)) => {
            items.push(rlp_u64(v));
            items.push(rlp_bytes(strip_zeros(r)));
            items.push(rlp_bytes(strip_zeros(s)));
        }
    }
    rlp_list(&items)
}

fn rlp_list(items: &[Vec<u8>]) -> Vec<u8> {
    let mut payload = Vec::new();
    for item in items {
        payload.extend_from_slice(item);
    }
    rlp_wrap(0xc0, 0xf7, &payload)
}

fn rlp_bytes(b: &[u8]) -> Vec<u8> {
    if b.len() == 1 && b[0] < 0x80 {
        return vec![b[0]];
    }
    rlp_wrap(0x80, 0xb7, b)
}

fn rlp_u64(n: u64) -> Vec<u8> {
    if n == 0 {
        return vec![0x80];
    }
    let be = n.to_be_bytes();
    let start = be.iter().position(|&b| b != 0).unwrap_or(7);
    rlp_bytes(&be[start..])
}

fn rlp_wrap(short: u8, long: u8, b: &[u8]) -> Vec<u8> {
    let n = b.len();
    if n <= 55 {
        let mut o = Vec::with_capacity(1 + n);
        o.push(short + n as u8);
        o.extend_from_slice(b);
        o
    } else {
        let lb = u64_be(n as u64);
        let mut o = Vec::with_capacity(1 + lb.len() + n);
        o.push(long + lb.len() as u8);
        o.extend_from_slice(&lb);
        o.extend_from_slice(b);
        o
    }
}

fn u64_be(n: u64) -> Vec<u8> {
    let be = n.to_be_bytes();
    let start = be.iter().position(|&b| b != 0).unwrap_or(7);
    be[start..].to_vec()
}

fn strip_zeros(b: &[u8]) -> &[u8] {
    let i = b.iter().position(|&x| x != 0).unwrap_or(b.len());
    if i == b.len() {
        &[]
    } else {
        &b[i..]
    }
}

fn parse_address(addr: &str) -> Result<[u8; 20]> {
    let bytes = hex::decode(addr.trim_start_matches("0x"))
        .map_err(|e| WorkerError::Rejected(e.to_string()))?;
    if bytes.len() != 20 {
        return Err(WorkerError::Rejected(format!("not an address: {addr}")));
    }
    let mut out = [0u8; 20];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn parse_u64_hex(s: &str) -> Result<u64> {
    let h = s.trim().trim_start_matches("0x");
    if h.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(h, 16).map_err(|e| WorkerError::Net(format!("bad hex quantity {s}: {e}")))
}

fn encode_call(
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

fn word_address(addr: &str) -> Result<[u8; 32]> {
    let raw = addr.trim_start_matches("0x");
    let bytes = hex::decode(raw).map_err(|e| WorkerError::Rejected(e.to_string()))?;
    if bytes.len() != 20 {
        return Err(WorkerError::Rejected(format!("not an address: {addr}")));
    }
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(&bytes);
    Ok(word)
}

fn word_u256(v: u64) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[24..].copy_from_slice(&v.to_be_bytes());
    w
}

fn read_u64(word: &str) -> u64 {
    u64::from_str_radix(word.trim_start_matches('0').get(..).unwrap_or("0"), 16).unwrap_or(0)
}

async fn wait_ok(rpc_url: &str, hash: &str) -> Result<()> {
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let rec = rpc(rpc_url, "eth_getTransactionReceipt", serde_json::json!([hash])).await?;
        if rec.is_null() {
            continue;
        }
        let status = rec.get("status").and_then(|s| s.as_str()).unwrap_or("0x0");
        if status == "0x1" {
            return Ok(());
        }
        return Err(WorkerError::Rejected("settle transaction reverted".into()));
    }
    Err(WorkerError::Rejected("settle transaction was not mined".into()))
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
        .map_err(|e| WorkerError::Net(e.to_string()))?;
    let v: serde_json::Value = resp.json().await.map_err(|e| WorkerError::Net(e.to_string()))?;
    if let Some(err) = v.get("error") {
        return Err(WorkerError::Net(err.to_string()));
    }
    Ok(v.get("result").cloned().unwrap_or(serde_json::Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rlp_encodes_zero_and_small_ints() {
        assert_eq!(rlp_u64(0), vec![0x80]);
        assert_eq!(rlp_u64(1), vec![0x01]);
        assert_eq!(rlp_u64(0x7f), vec![0x7f]);
        assert_eq!(rlp_u64(0x80), vec![0x81, 0x80]);
    }

    #[test]
    fn signed_legacy_tx_is_nonempty_and_starts_as_a_list() {
        let key = SigningKey::from_bytes(&[7u8; 32].into()).unwrap();
        let tx = LegacyTx {
            nonce: 0,
            gas_price: 1_000_000_000,
            gas: 21_000,
            to: [0u8; 20],
            value: 0,
            data: vec![],
            chain_id: 8453,
        };
        let raw = sign_legacy(&key, &tx).unwrap();
        assert!(raw.len() > 16);
        assert!(raw[0] >= 0xc0, "legacy payload is an RLP list");
    }
}

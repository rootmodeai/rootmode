//! Sign an EIP-155 call and submit it with `eth_sendRawTransaction`.
//!
//! Public RPCs will not `eth_sendTransaction` for us: they have no keys.

use k256::ecdsa::{signature::hazmat::PrehashSigner, SigningKey};
use rootmode_core::payments::{address_of, keccak};

use crate::error::{AppError, Result};

pub async fn send_call(
    rpc_url: &str,
    key: &SigningKey,
    chain_id: u64,
    to: &str,
    data: Vec<u8>,
) -> Result<String> {
    let from = address_of(key.verifying_key());
    let nonce = parse_u64_hex(
        rpc(
            rpc_url,
            "eth_getTransactionCount",
            serde_json::json!([from, "pending"]),
        )
        .await?
        .as_str()
        .unwrap_or("0x0"),
    )?;
    let gas_price = parse_u64_hex(
        rpc(rpc_url, "eth_gasPrice", serde_json::json!([]))
            .await?
            .as_str()
            .unwrap_or("0x0"),
    )?;
    if gas_price == 0 {
        return Err(AppError::Net("eth_gasPrice returned 0".into()));
    }
    let tx = LegacyTx {
        nonce,
        gas_price,
        gas: 500_000,
        to: parse_address(to)?,
        value: 0,
        data,
        chain_id,
    };
    let raw = sign_legacy(key, &tx)?;
    match rpc(
        rpc_url,
        "eth_sendRawTransaction",
        serde_json::json!([format!("0x{}", hex::encode(raw))]),
    )
    .await
    {
        Ok(hash) => Ok(hash.as_str().unwrap_or_default().to_string()),
        Err(e) => {
            let msg = e.to_string();
            if msg.to_lowercase().contains("insufficient funds") {
                Err(AppError::Invalid(format!(
                    "the app key {from} needs a little ETH on this chain to submit the lock"
                )))
            } else {
                Err(e)
            }
        }
    }
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
        .map_err(|e| AppError::Invalid(e.to_string()))?;
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
        .map_err(|e| AppError::Invalid(e.to_string()))?;
    if bytes.len() != 20 {
        return Err(AppError::Invalid(format!("not an address: {addr}")));
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
    u64::from_str_radix(h, 16).map_err(|e| AppError::Net(format!("bad hex quantity {s}: {e}")))
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

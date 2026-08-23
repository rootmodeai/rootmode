//! The peer-to-peer layer: how rootmode nodes find each other.
//!
//! A worker joins through a bootstrap node, announces what it serves, and is
//! then reachable by anyone who looks it up. A client joins the same way, asks
//! "who does `llm`?", and connects **directly** to whoever answers. The
//! bootstrap node is an entry point and, when needed, a relay for nodes behind
//! NAT — it never sees a job.
//!
//! What travels over a rootmode stream is the same RootmodeProtocol v1 the
//! WebSocket transport carries, one JSON value per line.

pub mod framing;
pub mod ident;
pub mod node;
pub mod shutdown;

pub use framing::JsonStream;
pub use ident::{peer_id_from_hex, peer_id_to_hex};
pub use node::{cap_key, model_key, Node, NodeConfig, NodeEvent, PROTOCOL, PROVIDER_TTL};

/// Re-exported so dependents can drive streams without depending on libp2p.
pub use libp2p::futures;
pub use libp2p::multiaddr::Protocol;
pub use libp2p::{Multiaddr, PeerId, Stream};
pub use libp2p_stream::IncomingStreams;

/// Bootstrap nodes compiled into every build.
///
/// This is what makes an open network open: a fresh install joins it without
/// anybody being told an address. Every peer-to-peer network does this — IPFS
/// ships a list, Ethereum ships bootnodes, BitTorrent ships
/// `router.bittorrent.com` — because there is no other way to find the first
/// peer.
///
/// They are entry points, not authorities: they answer "who else is here",
/// relay for nodes behind NAT, and never see a job. List several, run by
/// different people if you can, so no single one going away closes the door.
///
/// Override at runtime with `ROOTMODE_BOOTSTRAP` (comma-separated), or in the
/// client's settings.
pub const DEFAULT_BOOTSTRAP: &[&str] = &[
    // The `/p2p/…` suffix is this node's public key. With it, libp2p refuses
    // to talk to anything else answering at that address — and, the practical
    // part, a worker behind NAT can ask this node to relay for it, which is
    // impossible without naming the relay.
    "/dns4/bootstrap.rootmode.ai/tcp/4001/p2p/12D3KooWLXbwVxwKHvEEMdbEbNCv49wVUKc2mieGZfDGw73hj3YW",
];

/// The bootstrap list to use when nothing is configured: the environment
/// first, then whatever was compiled in.
pub fn default_bootstrap() -> Vec<String> {
    if let Ok(from_env) = std::env::var("ROOTMODE_BOOTSTRAP") {
        let addrs: Vec<String> = from_env
            .split(',')
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::to_string)
            .collect();
        if !addrs.is_empty() {
            return addrs;
        }
    }
    DEFAULT_BOOTSTRAP.iter().map(|a| a.to_string()).collect()
}

#[derive(Debug, thiserror::Error)]
pub enum P2pError {
    #[error("identity: {0}")]
    Identity(String),
    #[error("startup: {0}")]
    Startup(String),
    #[error("cannot reach peer: {0}")]
    Dial(String),
    #[error("stream: {0}")]
    Stream(String),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, P2pError>;

/// Parse a bootstrap address: `/dns4/host/tcp/4001`, optionally with
/// `/p2p/<peer id>`.
///
/// The peer id is optional on purpose. Requiring it means you cannot publish a
/// bootstrap address as a DNS name alone, and it forces every operator to
/// copy a key fingerprint around before anyone can join.
///
/// What you give up without it: you cannot prove the node that answered is the
/// node you meant. What you do *not* give up: the connection is still
/// encrypted, every peer it introduces you to authenticates with its own key,
/// pinned peers are still checked, and results are still verified by hash. A
/// hostile bootstrap node can show you a biased view of the network — it
/// cannot read your jobs or forge a worker.
///
/// Include the `/p2p/…` suffix when you know it; libp2p then refuses to talk
/// to anything else at that address.
pub fn parse_bootstrap(addr: &str) -> Result<Multiaddr> {
    let parsed: Multiaddr = addr
        .trim()
        .parse()
        .map_err(|e| P2pError::Startup(format!("bootstrap address '{addr}': {e}")))?;

    let dialable = parsed.iter().any(|p| {
        matches!(
            p,
            libp2p::multiaddr::Protocol::Tcp(_) | libp2p::multiaddr::Protocol::P2pCircuit
        )
    });
    if !dialable {
        return Err(P2pError::Startup(format!(
            "bootstrap address '{addr}' has no transport — expected something like \
             /dns4/bootstrap.example.com/tcp/4001"
        )));
    }
    Ok(parsed)
}

/// Whether an address names the peer it expects to find, and can therefore be
/// verified on connect.
pub fn names_peer(addr: &Multiaddr) -> bool {
    addr.iter()
        .any(|p| matches!(p, libp2p::multiaddr::Protocol::P2p(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str =
        "/ip4/203.0.113.10/tcp/4001/p2p/12D3KooWA9hDLBd58GgxdcRTAsuMcbBqmQoK4PBFsnQrSseHNHSK";

    #[test]
    fn compiled_in_bootstrap_addresses_are_valid() {
        // A typo here would ship a build that cannot join anything, and the
        // failure would look like "the network is empty".
        for addr in DEFAULT_BOOTSTRAP {
            parse_bootstrap(addr)
                .unwrap_or_else(|e| panic!("DEFAULT_BOOTSTRAP entry {addr:?} is unusable: {e}"));
        }
    }

    #[test]
    fn a_bootstrap_address_needs_a_transport_but_not_a_peer_id() {
        // With the peer id: verified on connect.
        let with_id = parse_bootstrap(GOOD).unwrap();
        assert!(names_peer(&with_id));

        // Without: still usable, just not verifiable.
        let without_id = parse_bootstrap("/dns4/bootstrap.rootmode.ai/tcp/4001").unwrap();
        assert!(!names_peer(&without_id));

        // Nowhere to dial.
        assert!(parse_bootstrap("/dns4/bootstrap.rootmode.ai").is_err());
        assert!(parse_bootstrap("not-an-address").is_err());
    }

    #[test]
    fn dns_bootstrap_addresses_work_too() {
        let addr = GOOD.replace("/ip4/203.0.113.10", "/dns4/bootstrap.rootmode.ai");
        assert!(parse_bootstrap(&addr).is_ok());
    }
}

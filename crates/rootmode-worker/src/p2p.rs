//! Joining the network.
//!
//! The worker publishes, into the DHT, that it can do `llm` and/or `image` and
//! which models it serves. A client looking for either finds this node and
//! connects to it directly. The bootstrap node is how they meet; it carries no
//! jobs.

use std::sync::Arc;
use std::time::Duration;

use rootmode_p2p::futures::StreamExt;
use rootmode_p2p::{cap_key, model_key, Node, NodeConfig};

use crate::config::Config;
use crate::error::{Result, WorkerError};
use crate::server::Worker;

/// Wait for the bootstrap dial to land before publishing. Announcing into an
/// empty routing table just fails.
const SETTLE: Duration = Duration::from_secs(3);
/// Half the DHT record lifetime: a live worker refreshes twice per TTL, and
/// a dead one is gone once the last record expires.
const REPUBLISH: Duration = Duration::from_secs(rootmode_p2p::PROVIDER_TTL.as_secs() / 2);

/// What the node ended up doing, so callers can report it accurately rather
/// than re-deriving it from config that may say nothing.
pub struct Joined {
    pub node: Node,
    /// Entry points actually used — configured, or shipped with the build.
    pub bootstrap: Vec<String>,
    advertise: tokio::task::JoinHandle<()>,
}

impl Joined {
    /// Stop announcing and stop refreshing existing records. Call this on a
    /// clean shutdown so the node starts ageing out of the DHT immediately.
    pub async fn leave(self) {
        self.advertise.abort();
        self.node.withdraw().await;
    }
}

/// Start the node, announce what this worker serves, and keep announcing.
///
/// Returns `None` only when p2p is switched off entirely.
///
/// With no bootstrap address this worker *is* an entry point: it serves DHT
/// queries itself, so a client pointed straight at its address can find it and
/// everything it serves. That is the whole setup for one worker and one
/// client; a separate bootstrap node only earns its keep once several nodes
/// need to find each other.
pub async fn join(worker: Arc<Worker>, config: &Config) -> Result<Option<Joined>> {
    if !config.p2p.enabled {
        return Ok(None);
    }

    // Nothing configured means the network's own entry points, not "alone".
    let bootstrap = if config.p2p.bootstrap.is_empty() {
        rootmode_p2p::default_bootstrap()
    } else {
        config.p2p.bootstrap.clone()
    };
    // Truly alone: no configured address and none shipped with the build.
    let standalone = bootstrap.is_empty();

    let mut node_config = NodeConfig::new(worker.identity().clone());
    // With nowhere to join, this node has to be the entry point itself.
    node_config.dht_server = config.p2p.dht_server || standalone;
    // A relay reservation needs somebody to relay through.
    node_config.relay_reservation = config.p2p.relay && !standalone;
    node_config.local_discovery = config.p2p.local_discovery;

    for addr in &bootstrap {
        node_config.bootstrap.push(
            rootmode_p2p::parse_bootstrap(addr).map_err(|e| WorkerError::Config(e.to_string()))?,
        );
    }
    for addr in &config.p2p.listen {
        node_config.listen.push(
            addr.parse()
                .map_err(|e| WorkerError::Config(format!("p2p listen '{addr}': {e}")))?,
        );
    }
    for addr in &config.p2p.external {
        node_config.external.push(
            addr.parse()
                .map_err(|e| WorkerError::Config(format!("p2p external '{addr}': {e}")))?,
        );
    }

    let (node, incoming) = Node::start(node_config).map_err(|e| WorkerError::Net(e.to_string()))?;

    // Serve rootmode streams arriving over the network exactly as we serve
    // ones arriving over a websocket.
    {
        let worker = worker.clone();
        let mut incoming = incoming;
        tokio::spawn(async move {
            while let Some((peer, stream)) = incoming.next().await {
                tracing::info!(%peer, "peer connected");
                let worker = worker.clone();
                tokio::spawn(async move { worker.serve_stream(stream).await });
            }
        });
    }

    // Say something if the entry points never answer. "Joined the network"
    // when nothing connected is the kind of log that costs an afternoon — but
    // so is crying wolf because DNS took a moment, so keep looking for a while
    // before complaining.
    if !bootstrap.is_empty() {
        let node = node.clone();
        let attempted = bootstrap.clone();
        tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            loop {
                let connected = node.connected_peers().await;
                if connected > 0 {
                    tracing::info!("on the network — connected to {connected} peer(s)");
                    return;
                }
                if tokio::time::Instant::now() >= deadline {
                    tracing::warn!(
                        "nothing answered in 30s — this node is not on the wider network. \
                         tried: {}",
                        attempted.join(", ")
                    );
                    tracing::warn!(
                        "check the address resolves and the port is open; peers on this \
                         local network can still find it"
                    );
                    return;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
    }

    // Publish what we serve, and keep publishing. `provide` replaces the
    // advertised set, so a model that disappeared is withdrawn rather than
    // lingering under its old key until the TTL.
    let advertise = {
        let node = node.clone();
        let worker = worker.clone();
        tokio::spawn(async move {
            tokio::time::sleep(SETTLE).await;
            loop {
                let announce = worker.announce();
                let mut keys: Vec<_> = announce.caps.iter().map(|c| cap_key(c)).collect();
                keys.extend(announce.models.iter().map(|m| model_key(&m.id)));
                tracing::info!(
                    "announcing [{}] and {} model(s) to the network",
                    announce.caps.join(", "),
                    announce.models.len()
                );
                node.provide(keys).await;
                tokio::time::sleep(REPUBLISH).await;
            }
        })
    };

    Ok(Some(Joined {
        node,
        bootstrap,
        advertise,
    }))
}

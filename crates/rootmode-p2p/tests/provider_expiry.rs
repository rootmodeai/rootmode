//! Dead workers must not linger in the DHT.
//!
//! `stop_providing` is a local operation — remote records live until their
//! TTL. These tests pin both halves: we withdraw immediately from our own
//! store, and a record we published to someone else is gone once it expires.

use std::time::{Duration, Instant};

use rootmode_core::Identity;
use rootmode_p2p::{cap_key, Node, NodeConfig};

const TTL: Duration = Duration::from_secs(3);
const PATIENCE: Duration = Duration::from_secs(15);

fn config(dht_server: bool, listen: bool) -> NodeConfig {
    let mut config = NodeConfig::new(Identity::generate());
    config.dht_server = dht_server;
    config.local_discovery = false;
    config.provider_ttl = TTL;
    if listen {
        config.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    }
    config
}

async fn wait_for_listener(node: &Node) -> rootmode_p2p::Multiaddr {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(addr) = node.listeners().await.into_iter().next() {
            return format!("{addr}/p2p/{}", node.peer_id()).parse().unwrap();
        }
        assert!(Instant::now() < deadline, "node never bound a listener");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn withdrawing_drops_us_from_our_own_store() {
    let (node, incoming) = Node::start(config(true, true)).unwrap();
    drop(incoming);

    node.provide(vec![cap_key("llm"), cap_key("image")]).await;
    let found = node.find_providers(cap_key("llm")).await;
    assert!(
        found.contains(&node.peer_id()),
        "a node that just announced should find itself, got {found:?}"
    );

    // A later announce replaces the set: the dropped capability is gone now,
    // not after the TTL.
    node.provide(vec![cap_key("llm")]).await;
    let found = node.find_providers(cap_key("image")).await;
    assert!(
        !found.contains(&node.peer_id()),
        "dropped capability still advertised: {found:?}"
    );

    node.withdraw().await;
    let found = node.find_providers(cap_key("llm")).await;
    assert!(
        !found.contains(&node.peer_id()),
        "withdrawn, but still listed: {found:?}"
    );
}

#[tokio::test]
async fn a_dead_provider_disappears_once_its_record_expires() {
    let (bootstrap, incoming) = Node::start(config(true, true)).unwrap();
    drop(incoming);
    let bootstrap_addr = wait_for_listener(&bootstrap).await;

    let mut provider_cfg = config(false, true);
    provider_cfg.bootstrap = vec![bootstrap_addr.clone()];
    let (provider, incoming) = Node::start(provider_cfg).unwrap();
    drop(incoming);
    let provider_id = provider.peer_id();

    let mut finder_cfg = config(false, false);
    finder_cfg.bootstrap = vec![bootstrap_addr];
    let (finder, incoming) = Node::start(finder_cfg).unwrap();
    drop(incoming);

    let deadline = Instant::now() + Duration::from_secs(10);
    while bootstrap.connected_peers().await == 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    provider.bootstrap().await;
    finder.bootstrap().await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    provider.provide(vec![cap_key("llm")]).await;

    let deadline = Instant::now() + PATIENCE;
    loop {
        let found = finder.find_providers(cap_key("llm")).await;
        if found.contains(&provider_id) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "finder never saw the provider (last: {found:?})"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Process gone, no more republish. The record on the bootstrap lives
    // until TTL, then a lookup must not return it.
    drop(provider);
    tokio::time::sleep(TTL + Duration::from_secs(1)).await;

    let found = finder.find_providers(cap_key("llm")).await;
    assert!(
        !found.contains(&provider_id),
        "dead provider still in the DHT after TTL: {found:?}"
    );
}

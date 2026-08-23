//! Three nodes in one process, with logs: bootstrap, provider, finder.
use rootmode_p2p::{cap_key, Node, NodeConfig};
use std::time::Duration;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,libp2p_kad=debug,rootmode_p2p=debug".into()),
        )
        .init();

    let mut boot = NodeConfig::new(rootmode_core::Identity::generate());
    boot.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    boot.dht_server = true;
    boot.relay_server = true;
    let (bootnode, inc) = Node::start(boot).unwrap();
    drop(inc);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let addr: rootmode_p2p::Multiaddr = format!(
        "{}/p2p/{}",
        bootnode.listeners().await[0],
        bootnode.peer_id()
    )
    .parse()
    .unwrap();
    println!("bootstrap at {addr}");

    let mut prov = NodeConfig::new(rootmode_core::Identity::generate());
    prov.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    prov.bootstrap = vec![addr.clone()];
    let (provider, inc2) = Node::start(prov).unwrap();
    drop(inc2);
    println!("provider is {}", provider.peer_id());

    tokio::time::sleep(Duration::from_secs(3)).await;
    provider.provide(vec![cap_key("llm")]).await;

    let mut find = NodeConfig::new(rootmode_core::Identity::generate());
    find.bootstrap = vec![addr.clone()];
    let (finder, inc3) = Node::start(find).unwrap();
    drop(inc3);
    finder.bootstrap().await;

    for attempt in 1..=8 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let found = finder.find_providers(cap_key("llm")).await;
        println!("attempt {attempt}: providers = {found:?}");
        if !found.is_empty() {
            println!(
                "addresses known: {:?}",
                finder.known_addresses(found[0]).await
            );
            println!("RESULT OK");
            return;
        }
    }
    println!("RESULT FAILED");
}

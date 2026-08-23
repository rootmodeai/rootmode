//! Can this machine find, and reach, the workers on the real network?
//!
//! The question a client is really asking when it shows "nobody is online",
//! split into the three things that can independently be false:
//!
//! 1. **Reachable** — did we get to an entry point at all?
//! 2. **Findable** — does the DHT return a provider for the capability?
//! 3. **Dialable** — can we actually open a connection to that provider?
//!
//! On a local network mDNS answers before any of this matters, which is why
//! all three can be broken for months without anyone noticing. Run it from
//! somewhere else — a phone hotspot, another building — and it says which of
//! the three is the problem instead of leaving you guessing.
//!
//! ```sh
//! cargo run -p rootmode-p2p --example find_workers
//! cargo run -p rootmode-p2p --example find_workers -- llm
//! ```

use std::time::Duration;

use rootmode_core::Identity;
use rootmode_p2p::{cap_key, Node, NodeConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rootmode_p2p=info,warn".into()),
        )
        .with_target(false)
        .init();

    let caps: Vec<String> = {
        let given: Vec<String> = std::env::args().skip(1).collect();
        if given.is_empty() {
            vec!["llm".into(), "image".into()]
        } else {
            given
        }
    };

    let mut config = NodeConfig::new(Identity::generate());
    // Exactly a desktop client: no listener, and no local discovery, so
    // nothing can be found by being in the same room as it.
    config.listen.clear();
    config.local_discovery = false;
    for addr in rootmode_p2p::default_bootstrap() {
        config.bootstrap.push(rootmode_p2p::parse_bootstrap(&addr)?);
    }
    println!("entry points: {:?}", rootmode_p2p::default_bootstrap());

    let (node, incoming) = Node::start(config)?;
    drop(incoming);

    // 1. Reachable.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while node.connected_peers().await == 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let connected = node.connected_peers().await;
    println!("\n1. reachable   {connected} peer(s) connected");
    if connected == 0 {
        println!("   Nothing answered. Every entry point is down or blocked from here.");
        std::process::exit(1);
    }

    // Let the routing table fill before asking it anything.
    node.bootstrap().await;
    tokio::time::sleep(Duration::from_secs(3)).await;

    let mut any_found = false;
    let mut any_dialed = false;

    for cap in &caps {
        println!("\n--- {cap} ---");

        // 2. Findable.
        let providers = node.find_providers(cap_key(cap)).await;
        let providers: Vec<_> = providers
            .into_iter()
            .filter(|p| *p != node.peer_id())
            .collect();

        if providers.is_empty() {
            println!("2. findable    nobody advertises {cap}");
            continue;
        }
        any_found = true;
        println!("2. findable    {} provider(s)", providers.len());

        // 3. Dialable — the part a provider record cannot tell you. A worker
        //    behind NAT with no relay reservation is findable and unreachable,
        //    which looks identical from the outside until you try.
        for peer in providers {
            let addrs = node.known_addresses(peer).await;
            println!("   {peer}");
            for addr in &addrs {
                println!("     addr {addr}");
            }
            if addrs.is_empty() {
                println!("     (no addresses known — the DHT record carried none)");
            }

            match tokio::time::timeout(Duration::from_secs(20), node.open(peer)).await {
                Ok(Ok(_stream)) => {
                    any_dialed = true;
                    println!("3. dialable    yes — a job could run here");
                }
                Ok(Err(e)) => println!("3. dialable    NO: {e}"),
                Err(_) => println!("3. dialable    NO: timed out"),
            }
        }
    }

    println!();
    match (any_found, any_dialed) {
        (false, _) => {
            println!(
                "Found nobody. The entry point answered, so the network is up — but no\n\
                 provider record came back. Either no worker is running, or its records\n\
                 have not propagated."
            );
            std::process::exit(1);
        }
        (true, false) => {
            println!(
                "Found providers and could not reach any of them. That is the NAT case:\n\
                 the worker published a record naming addresses nobody outside its network\n\
                 can dial. Check that it holds a relay reservation (examples/relay_check)."
            );
            std::process::exit(1);
        }
        (true, true) => {
            println!("Found and reachable. A client here can run jobs.");
            Ok(())
        }
    }
}

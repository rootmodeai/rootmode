//! Does the network's entry point actually relay for us?
//!
//! A worker behind NAT is reachable from the outside world only if a bootstrap
//! node holds a **relay reservation** on its behalf. That reservation is the
//! difference between an open network and a very good LAN protocol, and its
//! absence is invisible from the worker's own logs unless you know to look:
//! everything else — the DHT connection, the provider records, local
//! discovery — carries on working perfectly while nobody outside can reach it.
//!
//! ```sh
//! cargo run -p rootmode-p2p --example relay_check
//! cargo run -p rootmode-p2p --example relay_check -- /dns4/host/tcp/4001/p2p/12D3Koo…
//! ```
//!
//! Exits non-zero if no reservation was granted, so it can gate a deploy.

use std::time::Duration;

use rootmode_core::Identity;
use rootmode_p2p::{Node, NodeConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rootmode_p2p=info,warn".into()),
        )
        .with_target(false)
        .init();

    let bootstrap: Vec<String> = {
        let given: Vec<String> = std::env::args().skip(1).collect();
        if given.is_empty() {
            rootmode_p2p::default_bootstrap()
        } else {
            given
        }
    };

    println!("asking for a relay reservation from:");
    for addr in &bootstrap {
        println!("  {addr}");
    }

    let mut config = NodeConfig::new(Identity::generate());
    // Listen on nothing: this is exactly the position a NAT'd worker is in
    // when it cannot accept an inbound connection.
    config.listen.clear();
    config.local_discovery = false;
    config.relay_reservation = true;
    for addr in &bootstrap {
        config.bootstrap.push(rootmode_p2p::parse_bootstrap(addr)?);
    }

    let (node, incoming) = Node::start(config)?;
    drop(incoming);

    // A reservation shows up as a listen address containing /p2p-circuit.
    // Poll rather than guess: it lands after the relay is dialled and the hop
    // protocol negotiated, which takes a moment on a cold connection.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    let mut circuits: Vec<String> = Vec::new();
    while tokio::time::Instant::now() < deadline {
        circuits = node
            .listeners()
            .await
            .into_iter()
            .map(|a| a.to_string())
            .filter(|a| a.contains("p2p-circuit"))
            .collect();
        if !circuits.is_empty() {
            // A moment for the rest of the relay's addresses to arrive.
            tokio::time::sleep(Duration::from_secs(2)).await;
            circuits = node
                .listeners()
                .await
                .into_iter()
                .map(|a| a.to_string())
                .filter(|a| a.contains("p2p-circuit"))
                .collect();
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let circuit = circuits.first().cloned();

    println!("\npeers connected: {}", node.connected_peers().await);

    match circuit {
        Some(_) => {
            for addr in &circuits {
                println!("RESERVED  {addr}");
            }
            // A reservation that names an address nobody outside can dial is
            // no reservation at all — it is the same unreachability, wearing a
            // circuit address.
            let routable = circuits.iter().any(|a| {
                !a.contains("/ip4/127.")
                    && !a.contains("/ip4/172.1")
                    && !a.contains("/ip4/10.")
                    && !a.contains("/ip4/192.168.")
            });
            if routable {
                println!("\nA node behind NAT is reachable at that address.");
                Ok(())
            } else {
                println!(
                    "\nEvery address above is private, so nothing outside that network can\n\
                     dial them. The relay is advertising where it is bound rather than where\n\
                     it can be reached: start it with --announce /dns4/<host>/tcp/<port>."
                );
                std::process::exit(1);
            }
        }
        None => {
            println!("NO RESERVATION");
            println!(
                "\nNothing granted one within 25s. A worker behind NAT is unreachable\n\
                 from outside its own network: peers can find its record in the DHT and\n\
                 then fail to dial it. Check that the entry point runs the relay service\n\
                 and that its address names its peer id (/p2p/12D3Koo…)."
            );
            std::process::exit(1);
        }
    }
}

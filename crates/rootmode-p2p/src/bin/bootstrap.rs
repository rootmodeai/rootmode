//! `rootmode-bootstrap` — the address new nodes dial to find everyone else.
//!
//! It is an ordinary peer with a stable address. It serves DHT queries and
//! relays connections for nodes behind NAT. It does **not** hold a directory of
//! members, approve anyone, or ever see a job: once two peers have found each
//! other they talk directly, and if this node disappears, everything already
//! running keeps working.
//!
//! Run more than one. Any node can be a bootstrap node.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use rootmode_p2p::{Multiaddr, Node, NodeConfig};

#[derive(Parser)]
#[command(
    name = "rootmode-bootstrap",
    version,
    about = "Entry point and relay for the rootmode network"
)]
struct Cli {
    /// Addresses to listen on. Repeat for several.
    #[arg(long, default_value = "/ip4/0.0.0.0/tcp/4001")]
    listen: Vec<Multiaddr>,

    /// ed25519 seed file. Created on first run; keep it, or the node's address
    /// changes and every configured client has to be updated.
    #[arg(long, default_value = "bootstrap.key")]
    key: PathBuf,

    /// The address others can reach this node on, e.g.
    /// `/ip4/192.168.1.50/tcp/4001`. Only needed when that differs from what
    /// the process binds — in a container it always does.
    #[arg(long)]
    announce: Option<Multiaddr>,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rootmode_p2p=info,rootmode_bootstrap=info,warn".into()),
        )
        .with_target(false)
        .init();

    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let identity = rootmode_core::keyfile::load_or_create(&cli.key)?;

    let mut config = NodeConfig::new(identity);
    config.listen = cli.listen.clone();
    // The two jobs of a bootstrap node: answer DHT queries, and relay for peers
    // that cannot accept a direct connection.
    config.dht_server = true;
    config.relay_server = true;
    // Tell peers where to actually find us. Without this the node advertises
    // whatever it bound to — inside a container, its private bridge address —
    // and every relay reservation it grants points somewhere unroutable, so
    // the workers it is relaying for are unreachable from the internet.
    if let Some(addr) = &cli.announce {
        config.external.push(addr.clone());
    }

    let (node, incoming) = Node::start(config)?;
    // It serves no jobs, so it does not accept rootmode streams at all.
    drop(incoming);

    // Listeners are bound asynchronously; give them a moment so the printed
    // address is the real one.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    println!("rootmode-bootstrap");
    println!("peer id   {}", node.peer_id());
    println!("rootmode  {}", node.hex_peer_id());
    println!();
    println!("give this to workers and clients:");

    let listeners = node.listeners().await;
    match &cli.announce {
        Some(addr) => println!("  {}/p2p/{}", addr, node.peer_id()),
        None => {
            // Whatever it bound to is often not what others can reach — a
            // container sees only its own private address. Show the shape and
            // let the operator fill in the host they know.
            let port = listeners
                .iter()
                .find_map(|a| {
                    a.iter().find_map(|p| match p {
                        libp2p::multiaddr::Protocol::Tcp(port) => Some(port),
                        _ => None,
                    })
                })
                .unwrap_or(4001);
            println!(
                "  /ip4/<this host's address>/tcp/{port}/p2p/{}",
                node.peer_id()
            );
            println!();
            println!("substitute the address others reach this machine on, or restart");
            println!("with --announce /ip4/<address>/tcp/{port} to have it printed for you.");
            if listeners.is_empty() {
                println!("(nothing bound yet — check --listen)");
            }
        }
    }
    println!();

    tracing::info!("running — ctrl-c to stop");
    rootmode_p2p::shutdown::signal().await;
    tracing::info!("stopping");
    Ok(())
}

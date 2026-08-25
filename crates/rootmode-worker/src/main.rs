//! `rootmode-worker` — run this on the box with the GPUs.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use rootmode_worker::{config::EXAMPLE_CONFIG, Config, Worker};

#[derive(Parser)]
#[command(
    name = "rootmode-worker",
    version,
    about = "Advertise this node's models and run rootmode jobs on local backends"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write an example config you can edit.
    Init {
        /// Where to write it. `-` prints to stdout.
        #[arg(default_value = "worker.toml")]
        path: PathBuf,
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
    },
    /// Serve jobs until interrupted.
    Run {
        #[arg(short, long, default_value = "worker.toml")]
        config: PathBuf,
        /// Override the configured listen address.
        #[arg(long)]
        listen: Option<String>,
    },
    /// Print this worker's peer id (creates the key if absent).
    Id {
        #[arg(short, long, default_value = "worker.toml")]
        config: PathBuf,
    },
    /// Check the config and that every backend is reachable.
    Check {
        #[arg(short, long, default_value = "worker.toml")]
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rootmode_worker=info,rootmode_p2p=info,warn".into()),
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

async fn run(cli: Cli) -> rootmode_worker::Result<()> {
    match cli.command {
        Command::Init { path, force } => {
            if path == PathBuf::from("-") {
                print!("{EXAMPLE_CONFIG}");
                return Ok(());
            }
            if path.exists() && !force {
                return Err(rootmode_worker::WorkerError::Config(format!(
                    "{} already exists (use --force to overwrite)",
                    path.display()
                )));
            }
            std::fs::write(&path, EXAMPLE_CONFIG)?;
            println!("wrote {}", path.display());
            println!(
                "edit it, then: rootmode-worker run --config {}",
                path.display()
            );
            Ok(())
        }

        Command::Id { config } => {
            let config = Config::load(&config)?;
            let identity = rootmode_core::keyfile::load_or_create(&config.worker.identity_file)?;
            println!("{}", identity.peer_id());
            Ok(())
        }

        Command::Check { config } => {
            let path = config;
            let config = Config::load(&path)?;
            println!("config   ok  {}", path.display());
            println!("listen       {}", config.worker.listen);
            println!(
                "signing      {}",
                if config.require_signature() {
                    "required"
                } else {
                    "optional"
                }
            );
            println!(
                "allowlist    {}",
                if config.worker.allow_peers.is_empty() {
                    "open — anyone who can reach the port".to_string()
                } else {
                    format!("{} peer(s)", config.worker.allow_peers.len())
                }
            );

            let identity = rootmode_core::keyfile::load_or_create(&config.worker.identity_file)?;
            println!("peer_id      {}", identity.peer_id());

            let mut failures = 0;
            let mut total = 0;
            let registry = rootmode_worker::backends::Registry::build(&config.backends).await?;
            for backend in registry.backends() {
                total += 1;
                match backend.health().await {
                    Ok(detail) => println!("{:<8} ok  {detail}", backend.name()),
                    Err(e) => {
                        failures += 1;
                        println!("{:<8} DOWN {e}", backend.name());
                    }
                }
            }

            // A box with two servers, one of them off, is still a worker.
            // Unhealthy only when nothing at all is answering — same rule
            // `run` already uses, so docker does not kill a node that is
            // serving.
            if failures == total {
                return Err(rootmode_worker::WorkerError::Config(
                    "no backend is reachable".into(),
                ));
            }
            Ok(())
        }

        Command::Run { config, listen } => {
            let mut config = Config::load(&config)?;
            if let Some(listen) = listen {
                config.worker.listen = listen;
                config.validate()?;
            }

            let worker = Arc::new(Worker::from_config(config).await?);
            let listener = worker.bind().await?;
            let addr = listener
                .local_addr()
                .map(|a| a.to_string())
                .unwrap_or_else(|_| worker.config().worker.listen.clone());

            let models = worker.models();
            tracing::info!(peer_id = %worker.peer_id(), "rootmode worker v{}", env!("CARGO_PKG_VERSION"));
            tracing::info!("listening on ws://{addr}");
            tracing::info!(
                "caps: [{}]  max_concurrent: {}",
                worker.registry().caps().join(", "),
                worker.config().worker.max_concurrent
            );
            if models.is_empty() {
                tracing::warn!("advertising no models — is the backend running?");
            } else {
                for model in &models {
                    tracing::info!("  {} ({})", model.id, model.kind.as_str());
                }
            }
            tracing::info!(
                "direct address for the client: ws://<this-host>:{}",
                port_of(&addr)
            );

            // Held for the life of the process: dropping it would leave the
            // network, silently.
            let p2p = match rootmode_worker::p2p::join(worker.clone(), worker.config()).await? {
                Some(joined) => {
                    // Listeners bind asynchronously; give them a moment so the
                    // addresses we print are the real ones.
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

                    let addresses = dialable(&joined.node).await;
                    if joined.bootstrap.is_empty() {
                        tracing::info!(
                            "no entry points known — this node is one. give a client one of:"
                        );
                    } else {
                        tracing::info!(
                            "using {} entry point(s): {}",
                            joined.bootstrap.len(),
                            joined.bootstrap.join(", ")
                        );
                        tracing::info!("reachable at:");
                    }
                    for addr in &addresses {
                        tracing::info!("  {addr}");
                    }
                    Some(joined)
                }
                None => {
                    tracing::info!("p2p disabled — clients need the ws:// address");
                    None
                }
            };

            // Usage reporting, when the operator asked for it. Its own task,
            // so a collector that hangs cannot delay a job.
            if worker.config().stats.enabled() {
                tokio::spawn(
                    worker
                        .clone()
                        .report_stats(rootmode_p2p::shutdown::signal()),
                );
            }

            // Settlement sweeper: collects channels once they are worth a
            // transaction, and any ticket before it expires.
            {
                let worker = worker.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                        worker.settle_due().await;
                    }
                });
            }

            let result = worker
                .serve(listener, rootmode_p2p::shutdown::signal())
                .await;
            // Stop refreshing DHT records so this node starts ageing out
            // instead of looking alive until the TTL runs down on its own.
            if let Some(joined) = p2p {
                joined.leave().await;
            }
            result
        }
    }
}

/// Listen addresses with the peer id attached, which is the form a client can
/// actually paste. Loopback last: it is never the one you want to hand out.
async fn dialable(node: &rootmode_p2p::Node) -> Vec<String> {
    let mut addresses: Vec<String> = node
        .listeners()
        .await
        .into_iter()
        .map(|addr| format!("{addr}/p2p/{}", node.peer_id()))
        .collect();
    addresses.sort_by_key(|a| a.contains("/127.0.0.1/") || a.contains("/::1/"));
    addresses
}

fn port_of(addr: &str) -> &str {
    addr.rsplit(':').next().unwrap_or("9944")
}

//! Join the network, find who is serving, and send one job — from a terminal.
//!
//! ```sh
//! # what is out there?
//! cargo run -p rootmode-worker --example network
//!
//! # ask the first llm peer something
//! cargo run -p rootmode-worker --example network -- "what is a peer?"
//! ```
//!
//! It uses the entry points compiled into the build, the same as the desktop
//! client, so if this works and the app does not, the difference is the app.
//! Override with `ROOTMODE_BOOTSTRAP=/ip4/…/tcp/4001/p2p/…`.

use std::time::{Duration, Instant};

use rootmode_core::{
    protocol::{ClientMessage, PeerHello},
    ChatMessage, Identity, JobPayload, JobStatus, JobSubmit, LlmParams, WorkerMessage,
    PROTOCOL_VERSION,
};
use rootmode_p2p::{cap_key, peer_id_to_hex, JsonStream, Node, NodeConfig, PeerId};
use uuid::Uuid;

const PATIENCE: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rootmode_p2p=info,warn".into()),
        )
        .with_target(false)
        .init();

    let prompt = std::env::args().nth(1);

    // A throwaway identity: this is a probe, not a node anyone should trust.
    let identity = Identity::generate();
    let entry_points = rootmode_p2p::default_bootstrap();

    println!("me         {}", identity.peer_id());
    if entry_points.is_empty() {
        println!("entry      none compiled in — only this network can be searched");
    } else {
        for addr in &entry_points {
            println!("entry      {addr}");
        }
    }

    let mut config = NodeConfig::new(identity.clone());
    // ROOTMODE_LOCAL_DISCOVERY=false proves the wider network specifically:
    // without it, a peer on your own LAN is found by mDNS and you learn
    // nothing about whether the entry points work.
    config.local_discovery = !matches!(
        std::env::var("ROOTMODE_LOCAL_DISCOVERY").as_deref(),
        Ok("false")
    );
    if !config.local_discovery {
        println!("local     off (network only)");
    }
    for addr in &entry_points {
        config.bootstrap.push(rootmode_p2p::parse_bootstrap(addr)?);
    }
    let (node, incoming) = Node::start(config)?;
    drop(incoming);
    node.bootstrap().await;

    // Did we actually get in? "Found nobody" and "never connected" look the
    // same from the outside and have completely different fixes.
    let connect_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let connected = node.connected_peers().await;
        if connected > 0 {
            println!("joined    connected to {connected} peer(s)");
            for peer in node.connected_peer_ids().await {
                // The full dialable form, for pinning an entry point.
                println!("entry id  {peer}");
            }
            break;
        }
        if Instant::now() >= connect_deadline {
            println!("joined    NO — nothing answered. the entry point is unreachable from here.");
            break;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }

    // Give the network a moment to answer before declaring it empty.
    println!("\nlooking for peers…");
    let deadline = Instant::now() + PATIENCE;
    let mut peers: Vec<PeerId> = Vec::new();
    while Instant::now() < deadline {
        peers = candidates(&node).await;
        peers.retain(|p| *p != node.peer_id());
        if !peers.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    if peers.is_empty() {
        println!("nothing found.");
        println!("  · is a worker running, and did it log 'announcing […] to the network'?");
        println!("  · are you both using the same entry points?");
        return Ok(());
    }

    // Ask each what it actually serves. The announce is the truth; a DHT
    // record is only a claim by whoever wrote it.
    let mut workers = Vec::new();
    for peer in peers {
        match describe(&node, peer, &identity).await {
            Ok(Some(announce)) => {
                println!(
                    "\npeer       {}\ncaps       [{}]\nmodels     {}",
                    announce.peer_id,
                    announce.caps.join(", "),
                    if announce.models.is_empty() {
                        "(none)".to_string()
                    } else {
                        announce
                            .models
                            .iter()
                            .map(|m| m.id.clone())
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                );
                workers.push((peer, announce));
            }
            Ok(None) => {}
            Err(e) => println!("\npeer       {peer}\n           unreachable: {e}"),
        }
    }

    let Some(prompt) = prompt else {
        println!(
            "\n{} worker(s). pass a prompt to send one a job.",
            workers.len()
        );
        return Ok(());
    };

    let Some((peer, announce)) = workers
        .into_iter()
        .find(|(_, a)| a.caps.iter().any(|c| c == "llm"))
    else {
        println!("\nnobody here is serving llm.");
        return Ok(());
    };

    println!("\nsending to {}…\n", announce.peer_id);
    run_job(
        &node,
        peer,
        &identity,
        prompt,
        announce.models.first().map(|m| m.id.clone()),
    )
    .await?;
    Ok(())
}

/// Everything worth asking: the DHT's answer, plus this network's.
async fn candidates(node: &Node) -> Vec<PeerId> {
    let mut found = node.local_peers().await;
    for cap in ["llm", "image"] {
        for peer in node.find_providers(cap_key(cap)).await {
            if !found.contains(&peer) {
                found.push(peer);
            }
        }
    }
    found
}

fn hello(identity: &Identity) -> ClientMessage {
    ClientMessage::PeerHello(PeerHello {
        v: PROTOCOL_VERSION,
        peer_id: identity.peer_id(),
    })
}

async fn describe(
    node: &Node,
    peer: PeerId,
    identity: &Identity,
) -> Result<Option<rootmode_core::PeerAnnounce>, Box<dyn std::error::Error>> {
    let mut stream = JsonStream::new(node.open(peer).await?);
    stream.send(&hello(identity)).await?;

    let announce = match stream.recv().await? {
        Some(line) => match WorkerMessage::parse(&line)? {
            WorkerMessage::PeerAnnounce(a) => Some(a),
            _ => None,
        },
        None => None,
    };
    stream.close().await;
    Ok(announce)
}

async fn run_job(
    node: &Node,
    peer: PeerId,
    identity: &Identity,
    prompt: String,
    model: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut stream = JsonStream::new(node.open(peer).await?);
    stream.send(&hello(identity)).await?;

    let submit = JobSubmit::new(
        Uuid::new_v4(),
        identity.peer_id(),
        JobPayload::Llm(LlmParams {
            model_hash: None,
            model_id: model,
            messages: vec![ChatMessage::new("user", prompt)],
            tools: Vec::new(),
            max_tokens: 2048,
            temperature: 0.7,
        }),
    )
    .signed_by(identity)?;

    println!("job        {}", submit.job_id);
    stream.send(&ClientMessage::JobSubmit(submit)).await?;

    while let Some(line) = stream.recv().await? {
        match WorkerMessage::parse(&line)? {
            WorkerMessage::JobStatus(s) => {
                match &s.error {
                    Some(error) => println!("status     {} — {error}", s.status.as_str()),
                    None => println!(
                        "status     {} {:.0}%",
                        s.status.as_str(),
                        s.progress * 100.0
                    ),
                }
                if s.status == JobStatus::Failed {
                    break;
                }
                if s.status == JobStatus::Done {
                    break;
                }
            }
            WorkerMessage::JobResult(r) => {
                println!("sha256     {}", r.sha256);
                println!("---");
                match (&r.text, &r.image_path_or_b64) {
                    (Some(text), _) => println!("{text}"),
                    (_, Some(image)) => println!("<{} bytes of base64 image>", image.len()),
                    _ => println!("<empty>"),
                }
                println!("---");
            }
            _ => {}
        }
    }

    stream.close().await;
    Ok(())
}

/// The peer id a discovered peer reports, for eyeballing against a pin.
#[allow(dead_code)]
fn hex_of(peer: &PeerId) -> String {
    peer_id_to_hex(peer).unwrap_or_else(|| peer.to_string())
}

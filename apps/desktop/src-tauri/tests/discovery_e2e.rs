//! Discovery, end to end, with three real nodes in one process:
//!
//! ```text
//! bootstrap  ←  worker announces "I do llm"
//!     ↑
//!   client asks "who does llm?" → dials the worker directly → runs a job
//! ```
//!
//! Only the inference server is a stub. The bootstrap node, the DHT, the
//! worker and the client transport are the shipped code, so this is the same
//! path a Spark box takes when it joins.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rootmode_core::{sha256_hex, ChatMessage, Identity, JobPayload, JobSubmit, LlmParams};
use rootmode_desktop_lib::net::{no_replies, Transport};
use rootmode_desktop_lib::p2p::Libp2pTransport;
use rootmode_p2p::{peer_id_to_hex, Multiaddr, Node, NodeConfig, PeerId};
use rootmode_worker::config::{BackendConfig, Config, P2pConfig, VllmConfig, WorkerConfig};
use rootmode_worker::testutil::StubHttp;
use rootmode_worker::Worker;
use uuid::Uuid;

const REPLY: &str = "found you over the network.";
const MODEL: &str = "llama-3.1-8b-instruct";

/// Discovery is eventually-consistent: the worker has to reach the bootstrap
/// node, publish, and the client has to query. Poll rather than guess.
const PATIENCE: Duration = Duration::from_secs(45);

/// `RUST_LOG=... cargo test --test discovery_e2e -- --nocapture` to watch the
/// DHT do its thing.
fn logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .try_init();
}

async fn stub_vllm() -> StubHttp {
    let sse = format!(
        concat!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{}\"}}}}]}}\n\n",
            "data: [DONE]\n\n",
        ),
        REPLY
    );
    StubHttp::start(vec![
        StubHttp::json(200, &format!(r#"{{"data":[{{"id":"{MODEL}"}}]}}"#)),
        StubHttp::sse(&sse),
    ])
    .await
}

/// The bootstrap node: DHT server and relay, nothing else.
///
/// Returns the handle as well as the address — the node stops the moment the
/// last handle is dropped.
async fn start_bootstrap() -> (Node, Multiaddr) {
    let mut config = NodeConfig::new(Identity::generate());
    config.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    config.dht_server = true;
    config.relay_server = true;

    let (node, incoming) = Node::start(config).unwrap();
    drop(incoming); // serves no jobs

    // Wait for the listener to bind so we can hand out a real address.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(addr) = node.listeners().await.into_iter().next() {
            let dialable = format!("{addr}/p2p/{}", node.peer_id()).parse().unwrap();
            return (node, dialable);
        }
        assert!(
            Instant::now() < deadline,
            "bootstrap never bound a listener"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn worker_config(vllm_endpoint: String, bootstrap: &Multiaddr) -> Config {
    Config {
        payments: Default::default(),
        worker: WorkerConfig {
            label: "spark".into(),
            // The p2p path is what is under test; no websocket listener.
            listen: "127.0.0.1:0".into(),
            max_concurrent: 1,
            require_signature: false,
            allow_peers: vec![],
            identity_file: std::env::temp_dir()
                .join(format!("rootmode-disc-{}", Uuid::new_v4()))
                .join("worker.key"),
            country: String::new(),
            refresh_secs: 0,
            payout_address: String::new(),
        },
        stats: Default::default(),
        p2p: P2pConfig {
            enabled: true,
            bootstrap: vec![bootstrap.to_string()],
            listen: vec!["/ip4/127.0.0.1/tcp/0".into()],
            // Loopback needs no relay, and asking for one just adds noise.
            relay: false,
            dht_server: false,
            // Off on purpose: these tests are about finding a peer through the
            // DHT. With mDNS on they would pass on a LAN shortcut and prove
            // nothing about the open-network path.
            local_discovery: false,
            external: vec![],
        },
        backends: vec![BackendConfig::Vllm(VllmConfig {
            endpoint: vllm_endpoint,
            api_key: None,
            models: vec![],
            model_hashes: Default::default(),
            price: None,
            prices: Default::default(),
            currency: "USD".into(),
            timeout_secs: 30,
        })],
    }
}

/// A client that can only use the DHT — no local-network shortcut.
fn client_node(identity: &Identity, bootstrap: &[String]) -> Node {
    let mut config = NodeConfig::new(identity.clone());
    config.local_discovery = false;
    for addr in bootstrap {
        config
            .bootstrap
            .push(rootmode_p2p::parse_bootstrap(addr).unwrap());
    }
    let (node, incoming) = Node::start(config).unwrap();
    drop(incoming);
    node
}

/// Poll until the DHT gives up the worker, or fail loudly.
async fn discover_worker(node: &Node, worker_peer_id: &str) -> PeerId {
    let deadline = Instant::now() + PATIENCE;
    loop {
        let peers = rootmode_desktop_lib::p2p::discover(node).await;
        if let Some(peer) = peers
            .iter()
            .find(|p| peer_id_to_hex(p).as_deref() == Some(worker_peer_id))
        {
            return *peer;
        }
        assert!(
            Instant::now() < deadline,
            "the client never discovered the worker (saw {peers:?})"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn llm_payload(model: Option<&str>) -> JobPayload {
    JobPayload::Llm(LlmParams {
        model_hash: None,
        model_id: model.map(str::to_string),
        messages: vec![ChatMessage::new("user", "are you there")],
        tools: Vec::new(),
        max_tokens: 32,
        temperature: 0.0,
        reasoning_effort: None,
    })
}

#[tokio::test]
async fn a_worker_announces_itself_and_a_client_finds_and_uses_it() {
    logging();
    let (_bootstrap_node, bootstrap) = start_bootstrap().await;
    let stub = stub_vllm().await;

    // The worker joins and announces what it serves.
    let worker = Arc::new(
        Worker::from_config(worker_config(stub.base_url(), &bootstrap))
            .await
            .unwrap(),
    );
    let worker_peer_id = worker.peer_id();
    rootmode_worker::p2p::join(worker.clone(), worker.config())
        .await
        .unwrap()
        .expect("the worker joined the network");

    // The client joins and asks who does `llm`. mDNS is off, so the DHT is the
    // only thing that can answer.
    let client_identity = Identity::generate();
    let node = client_node(&client_identity, &[bootstrap.to_string()]);
    let found = discover_worker(&node, &worker_peer_id).await;

    // Discovery hands over a key; the network resolved it to a route.
    assert_eq!(
        peer_id_to_hex(&found).as_deref(),
        Some(worker_peer_id.as_str())
    );

    let transport = Libp2pTransport::new(
        node,
        found,
        client_identity.clone(),
        Some(worker_peer_id.clone()),
        true,
    );

    // What it says it serves comes from the worker itself, not the DHT record.
    let announce = transport
        .probe()
        .await
        .expect("dialled the discovered peer")
        .announce
        .expect("it announced on connect");
    assert_eq!(announce.peer_id, worker_peer_id);
    assert_eq!(announce.caps, vec!["llm"]);
    assert_eq!(
        announce
            .models
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        vec![MODEL],
        "the model list came from the inference server on the worker"
    );

    // And a real job runs over that connection.
    let job_id = Uuid::new_v4();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    transport
        .run_job(
            JobSubmit::new(job_id, client_identity.peer_id(), llm_payload(Some(MODEL))),
            tx,
            std::sync::Arc::new(tokio::sync::Notify::new()),
            no_replies(),
        )
        .await
        .expect("the job ran");

    let mut result = None;
    let mut statuses = vec![];
    while let Ok(msg) = rx.try_recv() {
        match msg {
            rootmode_core::WorkerMessage::JobResult(r) => result = Some(r),
            rootmode_core::WorkerMessage::JobStatus(s) => statuses.push(s.status),
            _ => {}
        }
    }

    let result = result.expect("a result came back over the network");
    assert_eq!(result.job_id, job_id);
    assert_eq!(result.text.as_deref(), Some(REPLY));
    assert_eq!(result.sha256, sha256_hex(REPLY.as_bytes()));
    assert_eq!(statuses.last(), Some(&rootmode_core::JobStatus::Done));
}

#[tokio::test]
async fn one_worker_needs_no_bootstrap_node_at_all() {
    // The smallest possible network: one worker, one client, nothing else.
    // With no bootstrap configured the worker serves DHT queries itself, so a
    // client pointed straight at it can discover what it serves.
    logging();
    let stub = stub_vllm().await;

    let mut config = worker_config(stub.base_url(), &throwaway_addr());
    config.p2p.bootstrap.clear();

    let worker = Arc::new(Worker::from_config(config).await.unwrap());
    let worker_peer_id = worker.peer_id();
    let node = rootmode_worker::p2p::join(worker.clone(), worker.config())
        .await
        .unwrap()
        .expect("p2p started even without a bootstrap address")
        .node;

    // The address an operator would paste out of the worker's logs.
    let deadline = Instant::now() + Duration::from_secs(10);
    let entry = loop {
        if let Some(addr) = node.listeners().await.into_iter().next() {
            break format!("{addr}/p2p/{}", node.peer_id());
        }
        assert!(
            Instant::now() < deadline,
            "the worker never bound a p2p listener"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let client_identity = Identity::generate();
    let client = client_node(&client_identity, &[entry]);
    let found = discover_worker(&client, &worker_peer_id).await;

    // And it is usable, not merely visible.
    let transport = Libp2pTransport::new(
        client,
        found,
        client_identity.clone(),
        Some(worker_peer_id.clone()),
        true,
    );
    let announce = transport
        .probe()
        .await
        .expect("dialled it")
        .announce
        .expect("it announced");
    assert_eq!(announce.peer_id, worker_peer_id);
    assert_eq!(announce.models[0].id, MODEL);
}

#[tokio::test]
async fn a_pinned_key_still_protects_a_discovered_peer() {
    // Discovery is not trust. A peer can be genuinely on the network, online,
    // and serving — and still not be the node you meant.
    logging();
    let (_bootstrap_node, bootstrap) = start_bootstrap().await;
    let stub = stub_vllm().await;

    let worker = Arc::new(
        Worker::from_config(worker_config(stub.base_url(), &bootstrap))
            .await
            .unwrap(),
    );
    rootmode_worker::p2p::join(worker.clone(), worker.config())
        .await
        .unwrap()
        .unwrap();

    // mDNS off and a named target: with several tests running at once, "the
    // first peer discovered" can be somebody else's node entirely.
    let client_identity = Identity::generate();
    let node = client_node(&client_identity, &[bootstrap.to_string()]);
    let found = discover_worker(&node, &worker.peer_id()).await;

    let transport = Libp2pTransport::new(
        node,
        found,
        client_identity,
        Some(Identity::generate().peer_id()), // somebody else's key
        true,
    );

    let error = transport.probe().await.err().unwrap().to_string();
    assert!(error.contains("key mismatch"), "got: {error}");
}

/// A syntactically valid bootstrap address pointing at nothing, for tests that
/// clear the list straight afterwards.
fn throwaway_addr() -> Multiaddr {
    let peer = rootmode_p2p::peer_id_from_hex(&Identity::generate().peer_id()).unwrap();
    format!("/ip4/127.0.0.1/tcp/1/p2p/{peer}").parse().unwrap()
}

//! The open-network story, with nothing configured anywhere:
//!
//! 1. someone runs a bootstrap node
//! 2. a worker starts with an empty config and joins
//! 3. a client starts with no settings at all and finds it
//!
//! No addresses typed, no LAN shortcut — mDNS is off in this test so the only
//! thing that can make it work is the DHT. This is the path a stranger's
//! worker and a stranger's client take.
//!
//! One test in this file on purpose: it sets `ROOTMODE_BOOTSTRAP`, which is
//! process-wide, and a second test running beside it could see it change.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rootmode_core::Identity;
use rootmode_desktop_lib::state::AppState;
use rootmode_p2p::{peer_id_to_hex, Node, NodeConfig};
use rootmode_worker::config::{BackendConfig, Config, P2pConfig, VllmConfig, WorkerConfig};
use rootmode_worker::testutil::StubHttp;
use rootmode_worker::Worker;
use uuid::Uuid;

const MODEL: &str = "llama-3.1-8b-instruct";
const PATIENCE: Duration = Duration::from_secs(45);

#[tokio::test]
async fn a_worker_and_a_client_with_no_configuration_find_each_other() {
    // --- somebody runs a bootstrap node, and its address ships in the build.
    let mut boot = NodeConfig::new(Identity::generate());
    boot.listen = vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()];
    boot.dht_server = true;
    boot.relay_server = true;
    boot.local_discovery = false;
    let (bootstrap_node, incoming) = Node::start(boot).unwrap();
    drop(incoming);

    let deadline = Instant::now() + Duration::from_secs(10);
    let bootstrap_addr = loop {
        if let Some(addr) = bootstrap_node.listeners().await.into_iter().next() {
            break format!("{addr}/p2p/{}", bootstrap_node.peer_id());
        }
        assert!(Instant::now() < deadline, "bootstrap never bound");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    // Stands in for `DEFAULT_BOOTSTRAP` being compiled into the binary.
    std::env::set_var("ROOTMODE_BOOTSTRAP", &bootstrap_addr);

    // --- a worker starts. Its config names no bootstrap address at all.
    let stub = StubHttp::start(vec![StubHttp::json(
        200,
        &format!(r#"{{"data":[{{"id":"{MODEL}"}}]}}"#),
    )])
    .await;

    let worker = Arc::new(
        Worker::from_config(Config {
        payments: Default::default(),
            worker: WorkerConfig {
                label: "a stranger's gpu".into(),
                listen: "127.0.0.1:0".into(),
                max_concurrent: 1,
                require_signature: false,
                allow_peers: vec![],
                identity_file: std::env::temp_dir()
                    .join(format!("rootmode-open-{}", Uuid::new_v4()))
                    .join("worker.key"),
                country: String::new(),
                refresh_secs: 0,
                payout_address: String::new(),
            },
            stats: Default::default(),
            p2p: P2pConfig {
                enabled: true,
                bootstrap: vec![], // nothing configured
                listen: vec!["/ip4/127.0.0.1/tcp/0".into()],
                relay: false,
                dht_server: false,
                local_discovery: false, // prove it is the DHT, not the LAN
                external: vec![],
            },
            backends: vec![BackendConfig::Vllm(VllmConfig {
                endpoint: stub.base_url(),
                api_key: None,
                models: vec![],
                model_hashes: Default::default(),
                price: None,
                prices: Default::default(),
                currency: "USD".into(),
                timeout_secs: 30,
            })],
        })
        .await
        .unwrap(),
    );
    let worker_peer_id = worker.peer_id();
    let _worker_node = rootmode_worker::p2p::join(worker.clone(), worker.config())
        .await
        .unwrap()
        .expect("the worker joined using the shipped bootstrap list")
        .node;

    // --- a client starts. Its settings are empty: a fresh install.
    let app_data = std::env::temp_dir().join(format!("rootmode-open-client-{}", Uuid::new_v4()));
    let state = AppState::new(app_data.clone(), app_data.join("downloads")).unwrap();
    assert!(
        state.discovery_enabled(),
        "discovery is on out of the box, without being switched on"
    );
    assert_eq!(
        state.bootstrap_addrs(),
        vec![bootstrap_addr.clone()],
        "with nothing configured the client uses the shipped entry points"
    );

    let mut client_config = NodeConfig::new(state.identity());
    client_config.local_discovery = false;
    for addr in state.bootstrap_addrs() {
        client_config
            .bootstrap
            .push(rootmode_p2p::parse_bootstrap(&addr).unwrap());
    }
    let (client, incoming) = Node::start(client_config).unwrap();
    drop(incoming);
    client.bootstrap().await;

    // --- and it finds the worker, having been told nothing about it.
    let deadline = Instant::now() + PATIENCE;
    let found = loop {
        let peers = rootmode_desktop_lib::p2p::discover(&client).await;
        if let Some(peer) = peers
            .iter()
            .find(|p| peer_id_to_hex(p).as_deref() == Some(worker_peer_id.as_str()))
        {
            break *peer;
        }
        assert!(
            Instant::now() < deadline,
            "a fresh client never found the worker (saw {peers:?})"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };

    // And knows what it serves, from the worker itself.
    let transport = rootmode_desktop_lib::p2p::Libp2pTransport::new(
        client,
        found,
        state.identity(),
        None,
        true,
    );
    use rootmode_desktop_lib::net::Transport;
    let announce = transport
        .probe()
        .await
        .expect("dialled a peer it discovered")
        .announce
        .expect("which announced itself");

    assert_eq!(announce.peer_id, worker_peer_id);
    assert_eq!(announce.caps, vec!["llm"]);
    assert_eq!(announce.models[0].id, MODEL);
}

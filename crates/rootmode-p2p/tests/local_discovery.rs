//! Two nodes on one network, nothing configured, finding each other.
//!
//! This is the case that should need no bootstrap address, no pasting and no
//! DHT: a laptop and a GPU box on the same LAN.

use std::time::{Duration, Instant};

use rootmode_core::Identity;
use rootmode_p2p::{Node, NodeConfig, NodeEvent};

/// mDNS answers on its own schedule; poll rather than guess.
const PATIENCE: Duration = Duration::from_secs(30);

fn node(listen: bool) -> Node {
    let mut config = NodeConfig::new(Identity::generate());
    if listen {
        config.listen = vec!["/ip4/0.0.0.0/tcp/0".parse().unwrap()];
    }
    let (node, incoming) = Node::start(config).unwrap();
    drop(incoming);
    node
}

#[tokio::test]
async fn peers_on_the_same_network_find_each_other_with_no_configuration() {
    let worker = node(true);
    let client = node(false);

    let deadline = Instant::now() + PATIENCE;
    loop {
        if client.local_peers().await.contains(&worker.peer_id()) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the client never saw the worker on the local network"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::test]
async fn discovery_is_announced_as_an_event_not_only_polled() {
    // Waiting for the next poll is what made this feel broken; a peer that
    // appears should be reported the moment it does.
    let client = node(false);
    let mut events = client.events();

    let worker = node(true);

    let seen = tokio::time::timeout(PATIENCE, async {
        loop {
            match events.recv().await {
                Ok(NodeEvent::PeerDiscovered(peer)) if peer == worker.peer_id() => return peer,
                Ok(_) => continue,
                Err(e) => panic!("event stream ended: {e}"),
            }
        }
    })
    .await
    .expect("a discovery event arrived");

    assert_eq!(seen, worker.peer_id());
}

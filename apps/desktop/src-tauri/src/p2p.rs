//! The client's side of the network: join, find who serves what, connect
//! directly.
//!
//! Discovered peers land in the same peers table as ones typed by hand and are
//! used the same way. The only difference is where the address came from, which
//! the UI shows — a peer you found is not a peer you trust.

use std::sync::Arc;

use async_trait::async_trait;
use rootmode_core::{
    protocol::{ClientMessage, PeerHello},
    Identity, JobSubmit, PeerAnnounce, WorkerMessage, PROTOCOL_VERSION,
};
use rootmode_p2p::{cap_key, peer_id_to_hex, JsonStream, Node, NodeConfig, PeerId};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::error::{AppError, Result};
use crate::net::{timeout_for, Probe, Transport};

/// Endpoint scheme for a peer reachable only through the network.
pub const P2P_SCHEME: &str = "p2p://";

/// `p2p://<hex peer id>` — no host, because the address is looked up.
pub fn p2p_endpoint(hex_peer_id: &str) -> String {
    format!("{P2P_SCHEME}{hex_peer_id}")
}

/// Peer ids come in two written forms and one pasted one:
/// `p2p://<hex>`, a bare hex key, or the multiaddr a worker logs.
pub fn peer_id_of_endpoint(endpoint: &str) -> Result<PeerId> {
    if let Some(hex) = endpoint.strip_prefix(P2P_SCHEME) {
        return rootmode_p2p::peer_id_from_hex(hex).map_err(|e| AppError::Invalid(e.to_string()));
    }
    if endpoint.starts_with('/') {
        let (peer, _) = split_multiaddr(endpoint)?;
        return Ok(peer);
    }
    Err(AppError::Invalid(format!("not a peer address: {endpoint}")))
}

/// Split `/ip4/…/tcp/4101/p2p/12D3Koo…` into the peer and the route to it.
pub fn split_multiaddr(endpoint: &str) -> Result<(PeerId, rootmode_p2p::Multiaddr)> {
    let addr: rootmode_p2p::Multiaddr = endpoint
        .parse()
        .map_err(|e| AppError::Invalid(format!("not a valid address: {e}")))?;

    let peer = addr
        .iter()
        .find_map(|p| match p {
            rootmode_p2p::Protocol::P2p(peer) => Some(peer),
            _ => None,
        })
        .ok_or_else(|| {
            AppError::Invalid(
                "that address does not say which peer it is — it should end with /p2p/12D3Koo…"
                    .into(),
            )
        })?;

    Ok((peer, addr))
}

/// Start a node for the client: no listeners, just an outbound member of the
/// network that can dial who it finds.
///
/// A bootstrap address is optional. Without one the client still finds workers
/// on the same network by mDNS, which is the case that should need no
/// configuration at all.
pub async fn start(identity: Identity, bootstrap: &[String]) -> Result<Node> {
    let mut config = NodeConfig::new(identity);
    for addr in bootstrap {
        config.bootstrap.push(
            rootmode_p2p::parse_bootstrap(addr).map_err(|e| AppError::Invalid(e.to_string()))?,
        );
    }

    let (node, incoming) = Node::start(config).map_err(|e| AppError::Net(e.to_string()))?;
    // A client serves nothing, so it does not accept inbound rootmode streams.
    drop(incoming);

    node.bootstrap().await;
    Ok(node)
}

/// Everyone worth asking: peers advertising `llm`, `image` or `video` in the
/// DHT, plus anything on this network that announced itself.
///
/// The local half matters more than it looks. A worker on your LAN is found
/// with nothing configured, and it is found even if the DHT side has nobody to
/// answer queries — which is the normal state of a network with two machines
/// in it.
pub async fn discover(node: &Node) -> Vec<PeerId> {
    let mut found: Vec<PeerId> = Vec::new();

    for peer in node.local_peers().await {
        if peer != node.peer_id() {
            found.push(peer);
        }
    }

    for cap in ["llm", "image", "video"] {
        for peer in node.find_providers(cap_key(cap)).await {
            if peer != node.peer_id() && !found.contains(&peer) {
                found.push(peer);
            }
        }
    }

    found
}

// ------------------------------------------------------------------ transport

/// Talks to a peer over the network instead of a typed-in address. The
/// protocol on the wire is identical; only the pipe differs.
pub struct Libp2pTransport {
    node: Node,
    peer: PeerId,
    identity: Identity,
    expect_public_key: Option<String>,
    sign: bool,
}

impl Libp2pTransport {
    pub fn new(
        node: Node,
        peer: PeerId,
        identity: Identity,
        expect_public_key: Option<String>,
        sign: bool,
    ) -> Self {
        Self {
            node,
            peer,
            identity,
            expect_public_key,
            sign,
        }
    }

    async fn open(&self) -> Result<JsonStream<rootmode_p2p::Stream>> {
        let stream = self
            .node
            .open(self.peer)
            .await
            .map_err(|e| AppError::Net(e.to_string()))?;
        Ok(JsonStream::new(stream))
    }

    fn hello(&self) -> ClientMessage {
        ClientMessage::PeerHello(PeerHello {
            v: PROTOCOL_VERSION,
            peer_id: self.identity.peer_id(),
        })
    }

    /// A libp2p connection is already authenticated against the peer id, so a
    /// pinned key is checked against the address we dialled as well as the
    /// announce — either mismatch means this is not the node you meant.
    fn check_announce(&self, announce: &PeerAnnounce) -> Result<()> {
        let dialled = peer_id_to_hex(&self.peer);
        for (what, actual) in [
            ("connection", dialled.as_deref()),
            ("announce", Some(announce.peer_id.as_str())),
        ] {
            if let (Some(expected), Some(actual)) = (&self.expect_public_key, actual) {
                if !expected.eq_ignore_ascii_case(actual) {
                    return Err(AppError::Net(format!(
                        "peer key mismatch: pinned {expected}, {what} says {actual}"
                    )));
                }
            }
        }
        Ok(())
    }

    async fn next_message(
        &self,
        stream: &mut JsonStream<rootmode_p2p::Stream>,
    ) -> Result<Option<WorkerMessage>> {
        loop {
            let Some(line) = stream
                .recv()
                .await
                .map_err(|e| AppError::Net(e.to_string()))?
            else {
                return Ok(None);
            };
            match WorkerMessage::parse(&line) {
                Ok(WorkerMessage::Unknown) => continue,
                Ok(message) => return Ok(Some(message)),
                Err(e) => {
                    log::warn!("ignoring unparseable frame from {}: {e}", self.peer);
                    continue;
                }
            }
        }
    }
}

#[async_trait]
impl Transport for Libp2pTransport {
    async fn probe(&self) -> Result<Probe> {
        let started = std::time::Instant::now();
        let mut stream = self.open().await?;
        let latency_ms = started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;

        stream
            .send(&self.hello())
            .await
            .map_err(|e| AppError::Net(e.to_string()))?;

        let announce = match self.next_message(&mut stream).await? {
            Some(WorkerMessage::PeerAnnounce(a)) => {
                self.check_announce(&a)?;
                Some(a)
            }
            _ => None,
        };

        stream.close().await;
        Ok(Probe {
            latency_ms,
            announce,
        })
    }

    async fn run_job(
        &self,
        submit: JobSubmit,
        sink: UnboundedSender<WorkerMessage>,
        stop: std::sync::Arc<tokio::sync::Notify>,
        mut replies: UnboundedReceiver<rootmode_core::ClientMessage>,
    ) -> Result<()> {
        let mut stream = self.open().await?;
        stream
            .send(&self.hello())
            .await
            .map_err(|e| AppError::Net(e.to_string()))?;

        let submit = if self.sign {
            let signed = submit.signed_by(&self.identity)?;
            signed
                .verify()
                .map_err(|e| AppError::Invalid(format!("local signature check failed: {e}")))?;
            signed
        } else {
            submit
        };
        stream
            .send(&ClientMessage::JobSubmit(submit.clone()))
            .await
            .map_err(|e| AppError::Net(e.to_string()))?;

        let deadline = tokio::time::Instant::now() + timeout_for(submit.payload.kind());
        let mut got_result = false;
        // A peer that says *why* it failed did not hang up on us. Conflating
        // the two throws the reason away and reports a transport problem for
        // what was an ordinary rejection.
        let mut said_why = false;
        let mut asked_to_stop = false;
        let mut replies_open = true;

        loop {
            let next = tokio::select! {
                biased;
                _ = stop.notified(), if !asked_to_stop => {
                    asked_to_stop = true;
                    let cancel = ClientMessage::JobCancel(rootmode_core::protocol::JobCancel {
                        job_id: submit.job_id,
                    });
                    let _ = stream.send(&cancel).await;
                    continue;
                }
                reply = replies.recv(), if replies_open => {
                    match reply {
                        Some(msg) => {
                            let _ = stream.send(&msg).await;
                        }
                        None => replies_open = false,
                    }
                    continue;
                }
                next = tokio::time::timeout_at(deadline, self.next_message(&mut stream)) => {
                    next.map_err(|_| AppError::Net("job timed out waiting for the peer".into()))??
                }
            };

            let Some(message) = next else { break };
            match &message {
                WorkerMessage::PeerAnnounce(a) => {
                    self.check_announce(a)?;
                    let _ = sink.send(message);
                }
                WorkerMessage::JobResult(r) if r.job_id == submit.job_id => {
                    got_result = true;
                    let _ = sink.send(message);
                }
                WorkerMessage::JobStatus(s) if s.job_id == submit.job_id => {
                    let terminal = s.status.is_terminal();
                    said_why |= s.status == rootmode_core::JobStatus::Failed;
                    let _ = sink.send(message);
                    if terminal {
                        break;
                    }
                }
                WorkerMessage::JobDelta(d) if d.job_id == submit.job_id => {
                    let _ = sink.send(message);
                }
                WorkerMessage::JobInvoice(i) if i.job_id == submit.job_id => {
                    let _ = sink.send(message);
                }
                // Not ours — a shared connection can carry other jobs.
                _ => continue,
            }
        }

        stream.close().await;
        if !got_result && !said_why {
            return Err(AppError::Net(
                "peer closed the stream without a result".into(),
            ));
        }
        Ok(())
    }
}

/// Convenience for the job manager.
pub fn transport(
    node: Node,
    endpoint: &str,
    identity: Identity,
    expect_public_key: Option<String>,
    sign: bool,
) -> Result<Arc<dyn Transport>> {
    let peer = peer_id_of_endpoint(endpoint)?;
    Ok(Arc::new(Libp2pTransport::new(
        node,
        peer,
        identity,
        expect_public_key,
        sign,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_round_trip_to_peer_ids() {
        let identity = Identity::generate();
        let endpoint = p2p_endpoint(&identity.peer_id());
        assert!(endpoint.starts_with("p2p://"));

        let peer = peer_id_of_endpoint(&endpoint).unwrap();
        assert_eq!(
            peer_id_to_hex(&peer).as_deref(),
            Some(identity.peer_id().as_str())
        );
    }

    #[test]
    fn rejects_endpoints_that_are_not_peers() {
        assert!(peer_id_of_endpoint("ws://10.0.0.1:9944").is_err());
        assert!(peer_id_of_endpoint("p2p://not-hex").is_err());
    }
}

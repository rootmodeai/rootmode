//! Transport layer.
//!
//! Everything above this module deals in [`rootmode_core::protocol`] messages,
//! not sockets. Adding libp2p later means adding a [`Transport`] impl — the
//! job manager and the UI do not change.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use rootmode_core::{
    protocol::{ClientMessage, JobCancel, PeerHello, MAX_MESSAGE_BYTES},
    Identity, JobKind, JobSubmit, PeerAnnounce, WorkerMessage,
};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio_tungstenite::tungstenite::Message;

use crate::error::{AppError, Result};

/// A reply channel nobody will write to. Tests and free jobs use this.
pub fn no_replies() -> UnboundedReceiver<ClientMessage> {
    mpsc::unbounded_channel().1
}

/// How long we wait for a peer to say anything at all.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
/// How long a single job may run before we give up on the peer.
pub const JOB_TIMEOUT: Duration = Duration::from_secs(600);
/// Video graphs run for minutes. Ten minutes is a hang for chat and a
/// timeout for MiniMax.
pub const VIDEO_JOB_TIMEOUT: Duration = Duration::from_secs(2_100);

pub fn timeout_for(kind: JobKind) -> Duration {
    match kind {
        JobKind::Video => VIDEO_JOB_TIMEOUT,
        _ => JOB_TIMEOUT,
    }
}

/// What a probe learned about an endpoint.
#[derive(Debug, Clone)]
pub struct Probe {
    pub latency_ms: u32,
    pub announce: Option<PeerAnnounce>,
}

#[async_trait]
pub trait Transport: Send + Sync {
    /// Check reachability and collect the peer's announce, if it sends one.
    async fn probe(&self) -> Result<Probe>;

    /// Run one job to completion, streaming every worker message into `sink`.
    /// Returns when the peer reports a terminal status or the socket closes.
    ///
    /// `stop`, notified, asks the peer to end the job early — `job.cancel`
    /// over the same connection, not a hangup: the peer still gets to answer
    /// with a real terminal status, which is what lets the ordinary
    /// terminal-handling below run unchanged whether a job finished, failed,
    /// or was told to stop.
    async fn run_job(
        &self,
        submit: JobSubmit,
        sink: UnboundedSender<WorkerMessage>,
        stop: Arc<tokio::sync::Notify>,
        replies: UnboundedReceiver<ClientMessage>,
    ) -> Result<()>;
}

// ------------------------------------------------------------------ websocket

pub struct WsTransport {
    endpoint: String,
    identity: Identity,
    /// Pinned peer key. When set, an announce from another key is refused.
    expect_public_key: Option<String>,
    /// Whether to sign submissions. Off is legal in v1; some workers refuse it.
    sign: bool,
}

impl WsTransport {
    pub fn new(
        endpoint: impl Into<String>,
        identity: Identity,
        expect_public_key: Option<String>,
        sign: bool,
    ) -> Result<Self> {
        let endpoint = endpoint.into();
        validate_endpoint(&endpoint)?;
        Ok(Self {
            endpoint,
            identity,
            expect_public_key,
            sign,
        })
    }

    async fn connect(
        &self,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    > {
        let fut = tokio_tungstenite::connect_async(&self.endpoint);
        let (stream, _resp) = tokio::time::timeout(CONNECT_TIMEOUT, fut)
            .await
            .map_err(|_| AppError::Net(format!("timed out connecting to {}", self.endpoint)))?
            .map_err(|e| AppError::Net(format!("{}: {e}", self.endpoint)))?;
        Ok(stream)
    }

    fn check_announce(&self, a: &PeerAnnounce) -> Result<()> {
        match &self.expect_public_key {
            Some(expected) if !expected.eq_ignore_ascii_case(&a.peer_id) => {
                Err(AppError::Net(format!(
                    "peer key mismatch: pinned {expected}, endpoint announced {}",
                    a.peer_id
                )))
            }
            _ => Ok(()),
        }
    }
}

#[async_trait]
impl Transport for WsTransport {
    async fn probe(&self) -> Result<Probe> {
        let started = Instant::now();
        let mut ws = self.connect().await?;
        let latency_ms = started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;

        let hello = ClientMessage::PeerHello(PeerHello {
            v: rootmode_core::PROTOCOL_VERSION,
            peer_id: self.identity.peer_id(),
        });
        ws.send(Message::Text(serde_json::to_string(&hello)?))
            .await
            .map_err(|e| AppError::Net(e.to_string()))?;

        // An announce is optional: a peer that connects but stays quiet is
        // still "online", just uncharacterised.
        let announce =
            match tokio::time::timeout(CONNECT_TIMEOUT, next_worker_message(&mut ws)).await {
                Ok(Ok(Some(WorkerMessage::PeerAnnounce(a)))) => {
                    self.check_announce(&a)?;
                    Some(a)
                }
                Ok(Err(e)) => return Err(e),
                _ => None,
            };

        let _ = ws.close(None).await;
        Ok(Probe {
            latency_ms,
            announce,
        })
    }

    async fn run_job(
        &self,
        submit: JobSubmit,
        sink: UnboundedSender<WorkerMessage>,
        stop: Arc<tokio::sync::Notify>,
        mut replies: UnboundedReceiver<ClientMessage>,
    ) -> Result<()> {
        let mut ws = self.connect().await?;

        let hello = ClientMessage::PeerHello(PeerHello {
            v: rootmode_core::PROTOCOL_VERSION,
            peer_id: self.identity.peer_id(),
        });
        ws.send(Message::Text(serde_json::to_string(&hello)?))
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
        let frame = rootmode_core::canonical::wire_json(&ClientMessage::JobSubmit(submit.clone()))?;
        ws.send(Message::Text(frame))
            .await
            .map_err(|e| AppError::Net(e.to_string()))?;

        let deadline = tokio::time::Instant::now() + timeout_for(submit.payload.kind());
        let mut got_result = false;
        // See the p2p transport: a reported failure is not a hangup.
        let mut said_why = false;
        // Sent at most once — a second stop click while the peer is already
        // being asked has nothing new to say.
        let mut asked_to_stop = false;
        let mut replies_open = true;

        loop {
            let msg = tokio::select! {
                biased;
                _ = stop.notified(), if !asked_to_stop => {
                    asked_to_stop = true;
                    let cancel = ClientMessage::JobCancel(JobCancel { job_id: submit.job_id });
                    if let Ok(frame) = serde_json::to_string(&cancel) {
                        let _ = ws.send(Message::Text(frame)).await;
                    }
                    continue;
                }
                reply = replies.recv(), if replies_open => {
                    match reply {
                        Some(msg) => {
                            if let Ok(frame) = serde_json::to_string(&msg) {
                                let _ = ws.send(Message::Text(frame)).await;
                            }
                        }
                        None => replies_open = false,
                    }
                    continue;
                }
                msg = tokio::time::timeout_at(deadline, next_worker_message(&mut ws)) => {
                    msg.map_err(|_| AppError::Net("job timed out waiting for the peer".into()))?
                }
            };

            match msg {
                Ok(Some(WorkerMessage::PeerAnnounce(a))) => {
                    self.check_announce(&a)?;
                    let _ = sink.send(WorkerMessage::PeerAnnounce(a));
                }
                // Messages for other jobs on a shared socket are not ours.
                Ok(Some(m)) if job_id_of(&m).is_some_and(|id| id != submit.job_id) => continue,
                Ok(Some(m)) => {
                    let terminal = match &m {
                        WorkerMessage::JobResult(_) => {
                            got_result = true;
                            false
                        }
                        WorkerMessage::JobStatus(s) => {
                            said_why |= s.status == rootmode_core::JobStatus::Failed;
                            s.status.is_terminal()
                        }
                        _ => false,
                    };
                    let _ = sink.send(m);
                    if terminal {
                        break;
                    }
                }
                Ok(None) => break, // socket closed
                Err(e) => return Err(e),
            }
        }

        let _ = ws.close(None).await;
        if !got_result && !said_why {
            // The peer hung up without delivering anything; let the caller
            // decide whether a terminal status already covered it.
            return Err(AppError::Net(
                "peer closed the connection without a result".into(),
            ));
        }
        Ok(())
    }
}

fn job_id_of(m: &WorkerMessage) -> Option<uuid::Uuid> {
    match m {
        WorkerMessage::JobStatus(s) => Some(s.job_id),
        WorkerMessage::JobResult(r) => Some(r.job_id),
        WorkerMessage::JobDelta(d) => Some(d.job_id),
        WorkerMessage::JobInvoice(i) => Some(i.job_id),
        _ => None,
    }
}

/// Read the next frame we understand. Binary/ping/pong frames and unknown
/// message types are skipped; `None` means the socket closed.
async fn next_worker_message<S>(ws: &mut S) -> Result<Option<WorkerMessage>>
where
    S: StreamExt<Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    while let Some(frame) = ws.next().await {
        let frame = frame.map_err(|e| AppError::Net(e.to_string()))?;
        match frame {
            Message::Text(txt) => {
                if txt.len() > MAX_MESSAGE_BYTES {
                    return Err(AppError::Net("peer sent an oversized frame".into()));
                }
                match WorkerMessage::parse(&txt) {
                    Ok(WorkerMessage::Unknown) => continue,
                    Ok(m) => return Ok(Some(m)),
                    // A single malformed frame is the peer's problem, not a
                    // reason to drop a job that may still complete.
                    Err(e) => {
                        log::warn!("ignoring unparseable frame: {e}");
                        continue;
                    }
                }
            }
            Message::Close(_) => return Ok(None),
            _ => continue,
        }
    }
    Ok(None)
}

/// Only `ws://`, `wss://` and `p2p://` are accepted. This is the single place
/// a user-supplied string becomes a network target.
pub fn validate_endpoint(endpoint: &str) -> Result<()> {
    if endpoint.starts_with('/') {
        // Must name the peer: without /p2p/<id> there is nothing to
        // authenticate the far end against.
        crate::p2p::split_multiaddr(endpoint)?;
        return Ok(());
    }

    if let Some(hex) = endpoint.strip_prefix(crate::p2p::P2P_SCHEME) {
        // A p2p endpoint names a key, not a host — there is no address to
        // check, because the network resolves it.
        if hex.len() != 64 || hex::decode(hex).is_err() {
            return Err(AppError::Invalid(
                "a p2p endpoint must be p2p://<64 hex characters>".into(),
            ));
        }
        return Ok(());
    }

    let url = url::Url::parse(endpoint)
        .map_err(|e| AppError::Invalid(format!("not a valid URL: {e}")))?;
    match url.scheme() {
        "ws" | "wss" => {}
        other => {
            return Err(AppError::Invalid(format!(
                "endpoint scheme must be ws:// or wss:// (got {other}://)"
            )))
        }
    }
    if url.host_str().unwrap_or("").is_empty() {
        return Err(AppError::Invalid("endpoint has no host".into()));
    }
    Ok(())
}

/// Accept the shorthand users actually type: `host:port` becomes `ws://host:port`.
pub fn normalize_endpoint(input: &str) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(AppError::Invalid("endpoint is empty".into()));
    }
    let candidate = if trimmed.starts_with('/') {
        // A libp2p address, which is what a worker prints and what people
        // will paste. Kept whole: it names both the peer and the route to it.
        trimmed.to_string()
    } else if trimmed.contains("://") {
        trimmed.to_string()
    } else if trimmed.len() == 64 && hex::decode(trimmed).is_ok() {
        // A bare peer id is unambiguous: it is a key, so it is a p2p endpoint.
        format!("{}{}", crate::p2p::P2P_SCHEME, trimmed.to_lowercase())
    } else {
        format!("ws://{trimmed}")
    };
    validate_endpoint(&candidate)?;
    Ok(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_shorthand() {
        assert_eq!(
            normalize_endpoint("127.0.0.1:9944").unwrap(),
            "ws://127.0.0.1:9944"
        );
        assert_eq!(
            normalize_endpoint(" wss://peer.example/rm ").unwrap(),
            "wss://peer.example/rm"
        );
    }

    #[test]
    fn endpoint_rejects_other_schemes() {
        for bad in ["http://peer.example", "file:///etc/passwd", "ws://", ""] {
            assert!(normalize_endpoint(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn the_address_a_worker_prints_can_be_pasted_in() {
        let addr =
            "/ip4/192.168.1.50/tcp/4101/p2p/12D3KooWA9hDLBd58GgxdcRTAsuMcbBqmQoK4PBFsnQrSseHNHSK";
        assert_eq!(normalize_endpoint(addr).unwrap(), addr, "kept whole");
        assert!(validate_endpoint(addr).is_ok());

        // Without the peer id there is nothing to authenticate against.
        assert!(validate_endpoint("/ip4/192.168.1.50/tcp/4101").is_err());
    }

    #[test]
    fn a_bare_peer_id_is_a_network_endpoint() {
        let peer = "ab".repeat(32);
        assert_eq!(normalize_endpoint(&peer).unwrap(), format!("p2p://{peer}"));
        assert!(validate_endpoint(&format!("p2p://{peer}")).is_ok());
        assert!(validate_endpoint("p2p://short").is_err());
    }
}

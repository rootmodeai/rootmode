//! The websocket transport against a throwaway worker on localhost.
//!
//! This is the other half of the acceptance path: a *real* peer endpoint, a
//! real socket, and the wire format from `docs/PROTOCOL.md`. The test worker is
//! written against the document, not against our serde types, so a drift
//! between the two shows up here.

use futures_util::{SinkExt, StreamExt};
use rootmode_core::{sha256_hex, ChatMessage, Identity, JobPayload, JobSubmit, LlmParams};
use rootmode_desktop_lib::net::{no_replies, Transport, WsTransport};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

/// Behaviour of the fake worker for one connection.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Announce, then answer a job properly.
    Good,
    /// Announce under a different key than the client pinned.
    WrongKey,
    /// Accept the job and then hang up without a result.
    HangUp,
    /// Refuse to answer unless the submit arrived *without* a signature.
    ExpectUnsigned,
}

/// Start a worker on an ephemeral port; returns its `ws://` endpoint.
async fn spawn_worker(mode: Mode, announce_peer_id: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let peer_id = announce_peer_id.clone();
            tokio::spawn(async move {
                let mut ws = match tokio_tungstenite::accept_async(stream).await {
                    Ok(ws) => ws,
                    Err(_) => return,
                };

                let announce = format!(
                    r#"{{"v":1,"type":"peer.announce","peer_id":"{peer_id}","caps":["llm"],"models":[{{"id":"tiny-1","sha256":null,"kind":"llm"}}],"max_concurrent":1}}"#
                );
                let _ = ws.send(Message::Text(announce)).await;

                while let Some(Ok(frame)) = ws.next().await {
                    let Message::Text(txt) = frame else { continue };
                    let v: serde_json::Value = match serde_json::from_str(&txt) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    if v["type"] != "job.submit" {
                        continue;
                    }
                    assert_eq!(v["v"], 1, "worker only speaks v1");
                    let job_id = v["job_id"].as_str().unwrap().to_string();

                    match mode {
                        Mode::HangUp => {
                            let _ = ws.close(None).await;
                            return;
                        }
                        Mode::ExpectUnsigned if v.get("sig").is_some() => {
                            let _ = ws.close(None).await;
                            return;
                        }
                        // A worker that requires signatures checks `sig` here.
                        Mode::Good | Mode::WrongKey => {
                            assert!(v.get("sig").is_some(), "signing is on by default");
                        }
                        Mode::ExpectUnsigned => {}
                    }

                    // A message type this client version does not know: it must
                    // be ignored, not fatal.
                    let _ = ws
                        .send(Message::Text(
                            r#"{"v":1,"type":"peer.gossip","note":"from the future"}"#.into(),
                        ))
                        .await;

                    let _ = ws
                        .send(Message::Text(format!(
                            r#"{{"v":1,"type":"job.status","job_id":"{job_id}","status":"running","progress":0.5,"error":null}}"#
                        )))
                        .await;

                    let text = "pong";
                    let _ = ws
                        .send(Message::Text(format!(
                            r#"{{"v":1,"type":"job.result","job_id":"{job_id}","kind":"llm","sha256":"{}","text":"{text}","meta":{{"model":"tiny-1"}}}}"#,
                            sha256_hex(text.as_bytes())
                        )))
                        .await;

                    let _ = ws
                        .send(Message::Text(format!(
                            r#"{{"v":1,"type":"job.status","job_id":"{job_id}","status":"done","progress":1.0,"error":null}}"#
                        )))
                        .await;
                }
            });
        }
    });

    format!("ws://{addr}")
}

fn llm_payload() -> JobPayload {
    JobPayload::Llm(LlmParams {
        model_hash: None,
        model_id: Some("tiny-1".into()),
        messages: vec![ChatMessage::new("user", "ping")],
        tools: Vec::new(),
        max_tokens: 32,
        temperature: 0.0,
        reasoning_effort: None,
    })
}

#[tokio::test]
async fn probe_collects_the_announce() {
    let worker = Identity::generate();
    let endpoint = spawn_worker(Mode::Good, worker.peer_id()).await;
    let transport = WsTransport::new(endpoint, Identity::generate(), None, true).unwrap();

    let probe = transport.probe().await.unwrap();
    let announce = probe.announce.expect("worker announced");
    assert_eq!(announce.peer_id, worker.peer_id());
    assert_eq!(announce.caps, vec!["llm"]);
    assert_eq!(announce.models[0].id, "tiny-1");
}

#[tokio::test]
async fn run_job_over_a_real_socket() {
    let worker = Identity::generate();
    let endpoint = spawn_worker(Mode::Good, worker.peer_id()).await;
    let client = Identity::generate();
    let transport =
        WsTransport::new(endpoint, client.clone(), Some(worker.peer_id()), true).unwrap();

    let job_id = Uuid::new_v4();
    let (tx, mut rx) = mpsc::unbounded_channel();
    transport
        .run_job(
            JobSubmit::new(job_id, client.peer_id(), llm_payload()),
            tx,
            std::sync::Arc::new(tokio::sync::Notify::new()),
            no_replies(),
        )
        .await
        .unwrap();

    let mut statuses = vec![];
    let mut text = None;
    while let Ok(msg) = rx.try_recv() {
        match msg {
            rootmode_core::WorkerMessage::JobStatus(s) => statuses.push(s.status),
            rootmode_core::WorkerMessage::JobResult(r) => text = r.text,
            _ => {}
        }
    }
    assert_eq!(text.as_deref(), Some("pong"));
    assert!(statuses.contains(&rootmode_core::JobStatus::Done));
}

#[tokio::test]
async fn submitted_jobs_are_signed_and_verifiable() {
    let client = Identity::generate();
    let submit = JobSubmit::new(Uuid::new_v4(), "placeholder", llm_payload())
        .signed_by(&client)
        .unwrap();
    assert_eq!(submit.from, client.peer_id());
    submit.verify().expect("a worker can verify what we send");
}

#[tokio::test]
async fn a_pinned_key_mismatch_refuses_the_peer() {
    let actual = Identity::generate();
    let expected = Identity::generate();
    let endpoint = spawn_worker(Mode::WrongKey, actual.peer_id()).await;
    let transport = WsTransport::new(
        endpoint,
        Identity::generate(),
        Some(expected.peer_id()),
        true,
    )
    .unwrap();

    let err = transport.probe().await.unwrap_err().to_string();
    assert!(err.contains("key mismatch"), "got: {err}");
}

#[tokio::test]
async fn a_peer_that_hangs_up_is_a_clear_error() {
    let worker = Identity::generate();
    let endpoint = spawn_worker(Mode::HangUp, worker.peer_id()).await;
    let client = Identity::generate();
    let transport = WsTransport::new(endpoint, client.clone(), None, true).unwrap();

    let (tx, _rx) = mpsc::unbounded_channel();
    let err = transport
        .run_job(
            JobSubmit::new(Uuid::new_v4(), client.peer_id(), llm_payload()),
            tx,
            std::sync::Arc::new(tokio::sync::Notify::new()),
            no_replies(),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("without a result"), "got: {err}");
}

#[tokio::test]
async fn an_unreachable_endpoint_is_a_clear_error() {
    // Port 1 on loopback: nothing listens there.
    let transport = WsTransport::new("ws://127.0.0.1:1", Identity::generate(), None, true).unwrap();
    let err = transport.probe().await.unwrap_err().to_string();
    assert!(err.starts_with("network:"), "got: {err}");
}

#[tokio::test]
async fn signing_can_be_turned_off() {
    // With `sign_jobs` off the submit must carry no `sig` at all — this worker
    // hangs up if it sees one.
    let worker = Identity::generate();
    let endpoint = spawn_worker(Mode::ExpectUnsigned, worker.peer_id()).await;
    let client = Identity::generate();
    let transport = WsTransport::new(endpoint, client.clone(), None, false).unwrap();

    let (tx, mut rx) = mpsc::unbounded_channel();
    transport
        .run_job(
            JobSubmit::new(Uuid::new_v4(), client.peer_id(), llm_payload()),
            tx,
            std::sync::Arc::new(tokio::sync::Notify::new()),
            no_replies(),
        )
        .await
        .expect("unsigned submit accepted");

    let got_result = std::iter::from_fn(|| rx.try_recv().ok())
        .any(|m| matches!(m, rootmode_core::WorkerMessage::JobResult(_)));
    assert!(got_result);
}

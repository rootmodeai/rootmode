//! The whole path, end to end, with nothing hand-rolled in the middle:
//!
//! ```text
//! desktop WsTransport → rootmode-worker → vLLM-shaped HTTP server
//! ```
//!
//! The only stub is the inference server itself. Everything between the client
//! transport and the worker's backend adapter is the real code, so a change
//! that breaks the client/worker contract fails here rather than on someone's
//! DGX box.

use std::path::PathBuf;
use std::sync::Arc;

use k256::ecdsa::{signature::hazmat::PrehashSigner, SigningKey};
use rootmode_core::payments::{address_of, channel_id, metadata_hash, Domain, SpendingAuth};
use rootmode_core::{sha256_hex, ChatMessage, Identity, JobPayload, JobSubmit, LlmParams};
use rootmode_worker::channels::Channels;
use rootmode_desktop_lib::net::{no_replies, Transport, WsTransport};
use rootmode_worker::config::{BackendConfig, Config, VllmConfig, WorkerConfig};
use rootmode_worker::testutil::StubHttp;
use rootmode_worker::Worker;
use tokio::sync::mpsc;
use uuid::Uuid;

const REPLY: &str = "a peer is a node you can name and reach.";

/// A stub that answers `/v1/models` once, then streams the same completion for
/// every subsequent request.
async fn stub_vllm() -> StubHttp {
    let sse = format!(
        concat!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{}\"}}}}]}}\n\n",
            "data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n",
            "data: [DONE]\n\n",
        ),
        REPLY
    );
    StubHttp::start(vec![
        StubHttp::json(
            200,
            r#"{"object":"list","data":[{"id":"llama-3.1-8b-instruct"}]}"#,
        ),
        StubHttp::sse(&sse),
    ])
    .await
}

fn temp_key() -> PathBuf {
    std::env::temp_dir()
        .join(format!("rootmode-e2e-{}", Uuid::new_v4()))
        .join("worker.key")
}

fn worker_config(vllm_endpoint: String, allow_peers: Vec<String>) -> Config {
    Config {
        payments: Default::default(),
        p2p: Default::default(),
        worker: WorkerConfig {
            label: "test worker".into(),
            listen: "127.0.0.1:0".into(),
            max_concurrent: 2,
            require_signature: false,
            allow_peers,
            identity_file: temp_key(),
            // A test worker declares nothing about where it is, polls no
            // backends, and reports to nobody.
            country: String::new(),
            refresh_secs: 0,
            payout_address: String::new(),
        },
        stats: Default::default(),
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

/// Start a worker on an ephemeral port and return its `ws://` endpoint.
async fn start_worker(config: Config) -> (String, String) {
    let worker = Arc::new(Worker::from_config(config).await.unwrap());
    let listener = worker.bind().await.unwrap();
    let addr = listener.local_addr().unwrap();
    let peer_id = worker.peer_id();

    tokio::spawn(async move {
        worker
            .serve(listener, std::future::pending::<()>())
            .await
            .unwrap();
    });

    (format!("ws://{addr}"), peer_id)
}

fn llm_payload(model: Option<&str>) -> JobPayload {
    JobPayload::Llm(LlmParams {
        model_hash: None,
        model_id: model.map(str::to_string),
        messages: vec![ChatMessage::new("user", "what is a peer")],
        tools: Vec::new(),
        max_tokens: 64,
        temperature: 0.0,
    })
}

#[tokio::test]
async fn the_client_probes_a_worker_and_sees_its_real_models() {
    let stub = stub_vllm().await;
    let (endpoint, peer_id) = start_worker(worker_config(stub.base_url(), vec![])).await;

    let transport = WsTransport::new(endpoint, Identity::generate(), None, true).unwrap();
    let probe = transport.probe().await.unwrap();

    let announce = probe.announce.expect("worker announced on connect");
    assert_eq!(announce.peer_id, peer_id);
    assert_eq!(announce.caps, vec!["llm"]);
    assert_eq!(announce.max_concurrent, 2);
    assert_eq!(
        announce
            .models
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        vec!["llama-3.1-8b-instruct"],
        "the model list came from the inference server, not the config"
    );
}

#[tokio::test]
async fn a_job_travels_client_to_worker_to_vllm_and_back() {
    let stub = stub_vllm().await;
    let (endpoint, worker_peer_id) = start_worker(worker_config(stub.base_url(), vec![])).await;

    let client = Identity::generate();
    let transport = WsTransport::new(endpoint, client.clone(), Some(worker_peer_id), true).unwrap();

    let job_id = Uuid::new_v4();
    let (tx, mut rx) = mpsc::unbounded_channel();
    transport
        .run_job(
            JobSubmit::new(
                job_id,
                client.peer_id(),
                llm_payload(Some("llama-3.1-8b-instruct")),
            ),
            tx,
            std::sync::Arc::new(tokio::sync::Notify::new()),
            no_replies(),
        )
        .await
        .unwrap();

    let mut statuses = vec![];
    let mut result = None;
    while let Ok(msg) = rx.try_recv() {
        match msg {
            rootmode_core::WorkerMessage::JobStatus(s) => statuses.push(s.status),
            rootmode_core::WorkerMessage::JobResult(r) => result = Some(r),
            _ => {}
        }
    }

    let result = result.expect("the worker returned a result");
    assert_eq!(result.job_id, job_id, "the job id round-tripped unchanged");
    assert_eq!(result.text.as_deref(), Some(REPLY));
    assert_eq!(result.sha256, sha256_hex(REPLY.as_bytes()));
    assert_eq!(result.meta["backend"], "vllm");

    use rootmode_core::JobStatus::*;
    assert!(statuses.contains(&Queued));
    assert!(statuses.contains(&Running));
    assert_eq!(statuses.last(), Some(&Done));

    // The worker actually asked the inference server for the right thing.
    let requests = stub.requests();
    let completion = requests
        .iter()
        .find(|r| r.contains("/v1/chat/completions"))
        .expect("the worker called the completions endpoint");
    assert!(completion.contains("llama-3.1-8b-instruct"));
    assert!(completion.contains("what is a peer"));
    assert!(
        completion.contains("\"stream\":true"),
        "streaming is used for progress"
    );
}

#[tokio::test]
async fn asking_for_a_model_the_node_does_not_serve_fails_with_a_useful_error() {
    let stub = stub_vllm().await;
    let (endpoint, _) = start_worker(worker_config(stub.base_url(), vec![])).await;

    let client = Identity::generate();
    let transport = WsTransport::new(endpoint, client.clone(), None, true).unwrap();

    let (tx, mut rx) = mpsc::unbounded_channel();
    // The worker reports the failure as a terminal status, so the transport
    // itself errors only because no result arrived — both are fine, we care
    // about the message that reached the client.
    let _ = transport
        .run_job(
            JobSubmit::new(
                Uuid::new_v4(),
                client.peer_id(),
                llm_payload(Some("mixtral-8x22b")),
            ),
            tx,
            std::sync::Arc::new(tokio::sync::Notify::new()),
            no_replies(),
        )
        .await;

    let error = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|m| match m {
            rootmode_core::WorkerMessage::JobStatus(s) => s.error,
            _ => None,
        })
        .next()
        .expect("the worker explained itself");

    assert!(error.contains("mixtral-8x22b"), "got: {error}");
    assert!(error.contains("not served here"), "got: {error}");
    assert!(
        error.contains("llama-3.1-8b-instruct"),
        "the error lists what is available"
    );
}

#[tokio::test]
async fn a_worker_with_an_allowlist_refuses_an_unknown_client() {
    let stub = stub_vllm().await;
    let allowed = Identity::generate();
    let (endpoint, _) = start_worker(worker_config(stub.base_url(), vec![allowed.peer_id()])).await;

    let stranger = Identity::generate();
    let transport = WsTransport::new(endpoint.clone(), stranger.clone(), None, true).unwrap();

    let (tx, mut rx) = mpsc::unbounded_channel();
    let _ = transport
        .run_job(
            JobSubmit::new(Uuid::new_v4(), stranger.peer_id(), llm_payload(None)),
            tx,
            std::sync::Arc::new(tokio::sync::Notify::new()),
            no_replies(),
        )
        .await;

    let error = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|m| match m {
            rootmode_core::WorkerMessage::JobStatus(s) => s.error,
            _ => None,
        })
        .next()
        .expect("the worker refused and said why");
    assert!(error.contains("does not accept jobs"), "got: {error}");

    // The allowed client gets through on the same worker.
    let transport = WsTransport::new(endpoint, allowed.clone(), None, true).unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    transport
        .run_job(
            JobSubmit::new(Uuid::new_v4(), allowed.peer_id(), llm_payload(None)),
            tx,
            std::sync::Arc::new(tokio::sync::Notify::new()),
            no_replies(),
        )
        .await
        .unwrap();
    assert!(std::iter::from_fn(|| rx.try_recv().ok())
        .any(|m| matches!(m, rootmode_core::WorkerMessage::JobResult(_))));
}

/// Sign a spending authorisation the way a client's wallet would.
fn authorise(key: &SigningKey, channel_id: &str, cumulative: u64) -> SpendingAuth {
    let mut auth = SpendingAuth {
        channel_id: channel_id.to_string(),
        client: address_of(key.verifying_key()),
        cumulative,
        metadata_hash: metadata_hash("llama-3.1-8b-instruct", 1000, 500, 0, &sha256_hex(REPLY.as_bytes())),
        sig: None,
    };
    let digest = auth.digest(&test_domain()).unwrap();
    let (sig, recovery) = key.sign_prehash(&digest).unwrap();
    auth.sig = Some(format!(
        "0x{}{}",
        hex::encode(sig.to_bytes()),
        hex::encode([recovery.to_byte() + 27])
    ));
    auth
}

fn test_domain() -> Domain {
    Domain::base(CONTRACT)
}

const CONTRACT: &str = "0x1234567890abcdef1234567890abcdef12345678";

#[tokio::test]
async fn a_paying_client_is_served_and_the_worker_banks_the_authorisation() {
    // The settlement primitive over a real socket: the client attaches a
    // signed authorisation to its job, the worker checks it before spending
    // any GPU time, and what it can redeem afterwards survives a restart.
    let stub = stub_vllm().await;
    let mut config = worker_config(stub.base_url(), vec![]);
    let state_dir = std::env::temp_dir().join(format!("rootmode-pay-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&state_dir).unwrap();
    config.worker.identity_file = state_dir.join("worker.key");
    config.worker.payout_address = CONTRACT.into();
    config.payments.contract = CONTRACT.into();
    config.payments.channels_file = state_dir.join("channels.json");
    let ledger = config.payments.channels_file.clone();

    let (endpoint, worker_peer_id) = start_worker(config).await;
    let client = Identity::generate();
    let wallet = SigningKey::from_bytes(&[7u8; 32].into()).unwrap();
    let channel = channel_id(&client.peer_id(), &worker_peer_id, "session");
    let transport =
        WsTransport::new(endpoint, client.clone(), Some(worker_peer_id), true).unwrap();

    let mut submit = JobSubmit::new(
        Uuid::new_v4(),
        client.peer_id(),
        llm_payload(Some("llama-3.1-8b-instruct")),
    );
    submit.spend = Some(authorise(&wallet, &channel, 900_000));
    let (tx, _rx) = mpsc::unbounded_channel();
    transport
        .run_job(
            submit,
            tx,
            std::sync::Arc::new(tokio::sync::Notify::new()),
            no_replies(),
        )
        .await
        .unwrap();

    // Read back the way a settlement job would: from the file, not from the
    // process that wrote it.
    let mut reopened = None;
    for _ in 0..40 {
        let channels = Channels::load(&ledger, CONTRACT);
        if channels.owed() > 0 {
            reopened = Some(channels);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let reopened = reopened.expect("the worker banked what it was authorised");

    assert_eq!(reopened.owed(), 900_000, "$0.90, in USDC micros");
    let open = reopened.redeemable();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].channel_id, channel);
    assert_eq!(open[0].client, address_of(wallet.verifying_key()));
    // The signature is what the contract will check, so it has to be what was
    // stored — a number without it redeems nothing.
    assert_eq!(
        open[0].latest.recover(&test_domain()).unwrap(),
        address_of(wallet.verifying_key())
    );

    std::fs::remove_dir_all(&state_dir).ok();
}

#[tokio::test]
async fn a_worker_that_requires_payment_refuses_a_job_that_arrives_without_it() {
    // An operator who has turned billing on should not be quietly serving for
    // free, and the client should be told why rather than left waiting.
    let stub = stub_vllm().await;
    let mut config = worker_config(stub.base_url(), vec![]);
    let state_dir = std::env::temp_dir().join(format!("rootmode-nopay-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&state_dir).unwrap();
    config.worker.identity_file = state_dir.join("worker.key");
    config.payments.contract = CONTRACT.into();
    config.payments.require_auth = true;
    config.payments.channels_file = state_dir.join("channels.json");

    let (endpoint, worker_peer_id) = start_worker(config).await;
    let client = Identity::generate();
    let transport =
        WsTransport::new(endpoint, client.clone(), Some(worker_peer_id), true).unwrap();

    // The refusal reaches the client as a terminal status, which is what a
    // person actually sees; the transport erroring afterwards is incidental.
    let (tx, mut rx) = mpsc::unbounded_channel();
    let _ = transport
        .run_job(
            JobSubmit::new(
                Uuid::new_v4(),
                client.peer_id(),
                llm_payload(Some("llama-3.1-8b-instruct")),
            ),
            tx,
            std::sync::Arc::new(tokio::sync::Notify::new()),
            no_replies(),
        )
        .await;

    let err = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|m| match m {
            rootmode_core::WorkerMessage::JobStatus(s) => s.error,
            _ => None,
        })
        .next()
        .expect("the worker explained itself");
    assert!(err.contains("authorisation"), "{err}");

    std::fs::remove_dir_all(&state_dir).ok();
}

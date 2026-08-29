//! The local endpoint, end to end:
//!
//! ```text
//! curl-shaped HTTP → rootmode gateway → WsTransport → worker → vLLM stub
//! ```
//!
//! Everything but the inference server is the shipped code, including the
//! real TCP listener — so a change that breaks Claude Code or Cursor fails
//! here rather than in somebody's editor.

use std::path::PathBuf;
use std::sync::Arc;

use rootmode_core::{JobKind, ModelDescriptor};
use rootmode_desktop_lib::gateway::{self, Gateway};
use rootmode_desktop_lib::state::AppState;
use rootmode_worker::config::{BackendConfig, Config, VllmConfig, WorkerConfig};
use rootmode_worker::testutil::StubHttp;
use rootmode_worker::Worker;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

const REPLY: &str = "the network answered.";
const MODEL: &str = "llama-3.1-8b-instruct";

async fn stub_vllm() -> StubHttp {
    // One `/v1/models` answer, then a completion for every job. Enough turns
    // queued for every request this file makes.
    let sse = format!(
        concat!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{}\"}}}}]}}\n\n",
            "data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}],",
            "\"usage\":{{\"prompt_tokens\":11,\"completion_tokens\":4,\"total_tokens\":15}}}}\n\n",
            "data: [DONE]\n\n",
        ),
        REPLY
    );
    let mut turns = vec![StubHttp::json(
        200,
        &format!(r#"{{"object":"list","data":[{{"id":"{MODEL}"}}]}}"#),
    )];
    for _ in 0..6 {
        turns.push(StubHttp::sse(&sse));
    }
    StubHttp::start(turns).await
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rootmode-gw-{tag}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn start_worker(endpoint: String) -> String {
    let config = Config {
        payments: Default::default(),
        p2p: Default::default(),
        stats: Default::default(),
        worker: WorkerConfig {
            label: "gateway test worker".into(),
            listen: "127.0.0.1:0".into(),
            max_concurrent: 2,
            require_signature: false,
            allow_peers: vec![],
            identity_file: temp_dir("worker").join("worker.key"),
            country: String::new(),
            refresh_secs: 0,
            payout_address: String::new(),
        },
        backends: vec![BackendConfig::Vllm(VllmConfig {
            endpoint,
            api_key: None,
            models: vec![],
            model_hashes: Default::default(),
            price: None,
            prices: Default::default(),
            currency: "USD".into(),
            timeout_secs: 30,
        })],
    };

    let worker = Arc::new(Worker::from_config(config).await.unwrap());
    let listener = worker.bind().await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        worker
            .serve(listener, std::future::pending::<()>())
            .await
            .unwrap();
    });
    format!("ws://{addr}")
}

/// An app with the gateway on and one online provider serving `MODEL`.
async fn app_with_provider() -> (Arc<AppState>, Arc<Gateway>, gateway::GatewayStatus) {
    let stub = stub_vllm().await;
    let worker_endpoint = start_worker(stub.base_url()).await;
    // The stub's listener dies with the value; the worker needs it for the
    // whole test, and there is no test-scoped place to park it.
    std::mem::forget(stub);
    app_at(worker_endpoint).await
}

/// Like `app_with_provider`, but keeps the stub so a test can read back what
/// the worker actually asked the inference server for.
async fn app_with_visible_backend() -> (
    Arc<AppState>,
    Arc<Gateway>,
    gateway::GatewayStatus,
    StubHttp,
) {
    let stub = stub_vllm().await;
    let worker_endpoint = start_worker(stub.base_url()).await;
    let (state, gw, status) = app_at(worker_endpoint).await;
    (state, gw, status, stub)
}

/// The same, against a worker someone else started.
async fn app_at(worker_endpoint: String) -> (Arc<AppState>, Arc<Gateway>, gateway::GatewayStatus) {
    let state = Arc::new(AppState::new(temp_dir("app"), temp_dir("dl")).unwrap());
    let peer = state
        .db
        .add_peer("test provider", &worker_endpoint, None)
        .unwrap();
    state
        .db
        .update_peer_status(
            &peer.id,
            "online",
            Some(5),
            None,
            Some(&["llm".to_string()]),
            Some(&[ModelDescriptor {
                id: MODEL.into(),
                sha256: None,
                kind: JobKind::Llm,
                price: None,
                video: None,
            }]),
            Some(2),
            // No country announced by this stub worker.
            None,
            None,
            None,
        )
        .unwrap();

    // The endpoint binds a fixed port because it is an address people paste
    // into a config, so the test cannot use port 0. Ask the OS for a free one
    // and hand that over — arithmetic on the pid collides once tests run
    // concurrently, and a flaky port is a flaky suite.
    let port = free_port();
    state
        .set_setting("gateway_port", &port.to_string())
        .unwrap();
    state.set_setting("gateway", "true").unwrap();

    let gw = Arc::new(Gateway::new());
    let status = gateway::reconcile(gw.clone(), state.clone()).await;
    assert!(status.running, "gateway did not bind: {:?}", status.error);
    (state, gw, status)
}

/// A port nothing is listening on. Racy in principle — something could take
/// it between the probe and the bind — but only this suite is allocating in
/// this range, and the alternative is a hardcoded number that collides.
fn free_port() -> u16 {
    let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    port
}

/// A minimal HTTP/1.1 client: no dependency, and it exercises the real socket
/// rather than the router in isolation.
async fn request(
    port: u16,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> (u16, String) {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();

    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
    if let Some(t) = token {
        head.push_str(&format!("x-api-key: {t}\r\n"));
    }
    if let Some(b) = body {
        head.push_str("content-type: application/json\r\n");
        head.push_str(&format!("content-length: {}\r\n", b.len()));
    }
    head.push_str("\r\n");
    if let Some(b) = body {
        head.push_str(b);
    }

    stream.write_all(head.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();

    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (status, body.to_string())
}

#[tokio::test]
async fn claude_code_gets_an_anthropic_response_from_a_real_provider() {
    let (_state, _gw, status) = app_with_provider().await;

    let (code, body) = request(
        status.port,
        "POST",
        "/v1/messages",
        Some(&status.token),
        Some(&format!(
            r#"{{"model":"{MODEL}","max_tokens":64,"system":"be brief",
                 "messages":[{{"role":"user","content":"hello"}}]}}"#
        )),
    )
    .await;

    assert_eq!(code, 200, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["type"], "message");
    assert_eq!(json["role"], "assistant");
    assert_eq!(json["content"][0]["text"], REPLY);
    assert_eq!(json["stop_reason"], "end_turn");
    // OpenAI tokenizer, raised to the worker's figure when that is higher.
    let input = json["usage"]["input_tokens"].as_u64().unwrap();
    let output = json["usage"]["output_tokens"].as_u64().unwrap();
    assert!(input >= 11, "input was {input}");
    assert!(output >= 4, "output was {output}");
}

#[tokio::test]
async fn cursor_and_friends_get_an_openai_response() {
    let (_state, _gw, status) = app_with_provider().await;

    let (code, body) = request(
        status.port,
        "POST",
        "/v1/chat/completions",
        Some(&status.token),
        Some(&format!(
            r#"{{"model":"{MODEL}","messages":[{{"role":"user","content":"hello"}}]}}"#
        )),
    )
    .await;

    assert_eq!(code, 200, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["object"], "chat.completion");
    assert_eq!(json["choices"][0]["message"]["content"], REPLY);
    assert_eq!(json["choices"][0]["finish_reason"], "stop");
    let total = json["usage"]["total_tokens"].as_u64().unwrap();
    assert!(total >= 15, "total was {total}");
}

#[tokio::test]
async fn a_streaming_request_gets_the_event_sequence_clients_expect() {
    let (_state, _gw, status) = app_with_provider().await;

    let (code, body) = request(
        status.port,
        "POST",
        "/v1/messages",
        Some(&status.token),
        Some(&format!(
            r#"{{"model":"{MODEL}","max_tokens":64,"stream":true,
                 "messages":[{{"role":"user","content":"hello"}}]}}"#
        )),
    )
    .await;

    assert_eq!(code, 200, "{body}");
    for event in [
        "event: message_start",
        "event: content_block_start",
        "event: content_block_delta",
        "event: message_stop",
    ] {
        assert!(body.contains(event), "missing {event} in:\n{body}");
    }
    assert!(body.contains(REPLY));
}

#[tokio::test]
async fn the_model_list_is_what_the_network_is_actually_serving() {
    let (_state, _gw, status) = app_with_provider().await;

    let (code, body) = request(status.port, "GET", "/v1/models", Some(&status.token), None).await;
    assert_eq!(code, 200, "{body}");

    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["data"][0]["id"], MODEL);
    // Both catalogue dialects at once, so either kind of client can read it.
    assert_eq!(json["data"][0]["object"], "model");
    assert_eq!(json["data"][0]["type"], "model");
}

#[tokio::test]
async fn without_the_token_nothing_on_this_machine_can_spend_your_providers() {
    let (_state, _gw, status) = app_with_provider().await;

    let payload = format!(
        r#"{{"model":"{MODEL}","max_tokens":16,"messages":[{{"role":"user","content":"hi"}}]}}"#
    );

    let (code, _) = request(status.port, "POST", "/v1/messages", None, Some(&payload)).await;
    assert_eq!(code, 401, "an unauthenticated request must be refused");

    let (code, _) = request(
        status.port,
        "POST",
        "/v1/messages",
        Some("rm-wrong"),
        Some(&payload),
    )
    .await;
    assert_eq!(code, 401, "a wrong token must be refused");
}

#[tokio::test]
async fn asking_for_a_model_nobody_serves_says_what_is_on_offer() {
    let (state, gw, status) = app_with_provider().await;
    state.set_setting("gateway_substitute", "false").unwrap();
    gateway::reconcile(gw, state.clone()).await;

    let (code, body) = request(
        status.port,
        "POST",
        "/v1/messages",
        Some(&status.token),
        Some(r#"{"model":"gpt-9","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#),
    )
    .await;

    assert_eq!(code, 404, "{body}");
    // A bare "not found" would leave someone guessing at the model name.
    assert!(
        body.contains(MODEL),
        "error should list what is available: {body}"
    );
}

#[tokio::test]
async fn turning_it_off_closes_the_door() {
    let (state, gw, status) = app_with_provider().await;

    state.set_setting("gateway", "false").unwrap();
    let after = gateway::reconcile(gw.clone(), state.clone()).await;
    assert!(!after.running);

    // Graceful shutdown drains in-flight connections, so the socket closes
    // shortly after the call returns rather than during it. Poll rather than
    // race it.
    let mut closed = false;
    for _ in 0..40 {
        if tokio::net::TcpStream::connect(("127.0.0.1", status.port))
            .await
            .is_err()
        {
            closed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        closed,
        "the listener should be gone once the setting is off"
    );
}

#[tokio::test]
async fn an_unknown_model_name_is_answered_by_what_is_on_offer() {
    // Claude Code asks for `claude-opus-5`, and for its own small model on
    // background work. Neither will ever be served by a rootmode provider, so
    // refusing would make the editor unusable for a reason the user cannot
    // act on.
    let (_state, _gw, status) = app_with_provider().await;

    let (code, body) = request(
        status.port,
        "POST",
        "/v1/messages",
        Some(&status.token),
        Some(
            r#"{"model":"claude-opus-5","max_tokens":64,
                "messages":[{"role":"user","content":"hello"}]}"#,
        ),
    )
    .await;

    assert_eq!(code, 200, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["content"][0]["text"], REPLY);
    // The reply names the model that actually answered, not the one asked
    // for — hiding the substitution would hide the only surprising thing
    // about the response.
    assert_eq!(json["model"], MODEL);
}

#[tokio::test]
async fn substitution_can_be_turned_off_for_a_strict_setup() {
    let (state, gw, status) = app_with_provider().await;
    state.set_setting("gateway_substitute", "false").unwrap();
    assert!(!gateway::reconcile(gw, state.clone()).await.substitute);

    let (code, body) = request(
        status.port,
        "POST",
        "/v1/messages",
        Some(&status.token),
        Some(
            r#"{"model":"claude-opus-5","max_tokens":64,
                "messages":[{"role":"user","content":"hi"}]}"#,
        ),
    )
    .await;

    assert_eq!(code, 404, "{body}");
    assert!(body.contains(MODEL), "should still say what is available");
}

#[tokio::test]
async fn a_providers_own_reason_for_failing_reaches_the_client() {
    // The provider explains itself (context length, bad request, out of
    // memory). That explanation is the whole value of the error, and it used
    // to be replaced by a generic "peer closed the stream without a result"
    // — which tells the user nothing they can act on.
    let stub = StubHttp::start(vec![
        StubHttp::json(
            200,
            &format!(r#"{{"object":"list","data":[{{"id":"{MODEL}"}}]}}"#),
        ),
        StubHttp::json(
            400,
            r#"{"error":{"message":"This model's maximum context length is 8192 tokens."}}"#,
        ),
    ])
    .await;
    let worker_endpoint = start_worker(stub.base_url()).await;
    std::mem::forget(stub);

    let (state, gw, status) = app_at(worker_endpoint).await;
    let _ = (state, gw);

    let (code, body) = request(
        status.port,
        "POST",
        "/v1/messages",
        Some(&status.token),
        Some(&format!(
            r#"{{"model":"{MODEL}","max_tokens":64,
                 "messages":[{{"role":"user","content":"hello"}}]}}"#
        )),
    )
    .await;

    assert_eq!(code, 502, "{body}");
    assert!(
        body.contains("8192"),
        "the provider's own words should survive, got: {body}"
    );
    assert!(
        !body.contains("closed the stream"),
        "the transport's guess must not mask the real reason: {body}"
    );
}

#[tokio::test]
async fn a_tool_call_survives_the_whole_chain() {
    // The bug this guards: the model answers by calling a tool, vLLM parses
    // it into `tool_calls`, and nothing downstream reads that field — so the
    // job comes back "empty" and Claude Code cannot work at all.
    let sse = concat!(
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_ab","#,
        r#""function":{"name":"Read","arguments":"{\"path\":"}}]}}]}"#,
        "\n\n",
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"#,
        r#""function":{"arguments":"\"a.txt\"}"}}]}}]}"#,
        "\n\n",
        r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        "\n\n",
        "data: [DONE]\n\n",
    );
    let stub = StubHttp::start(vec![
        StubHttp::json(
            200,
            &format!(r#"{{"object":"list","data":[{{"id":"{MODEL}"}}]}}"#),
        ),
        StubHttp::sse(sse),
        StubHttp::sse(sse),
    ])
    .await;
    let worker_endpoint = start_worker(stub.base_url()).await;
    std::mem::forget(stub);

    let (_state, _gw, status) = app_at(worker_endpoint).await;

    let body_json = format!(
        r#"{{"model":"{MODEL}","max_tokens":256,
             "messages":[{{"role":"user","content":"read a.txt"}}],
             "tools":[{{"name":"Read","description":"Read a file",
                        "input_schema":{{"type":"object",
                          "properties":{{"path":{{"type":"string"}}}}}}}}]}}"#
    );

    let (code, body) = request(
        status.port,
        "POST",
        "/v1/messages",
        Some(&status.token),
        Some(&body_json),
    )
    .await;

    assert_eq!(code, 200, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["stop_reason"], "tool_use");
    assert_eq!(json["content"][0]["type"], "tool_use");
    assert_eq!(json["content"][0]["name"], "Read");
    assert_eq!(json["content"][0]["id"], "call_ab");
    // Reassembled from two stream fragments and parsed into an object.
    assert_eq!(json["content"][0]["input"]["path"], "a.txt");

    // The same call, streamed, is what Claude Code actually consumes.
    let (code, body) = request(
        status.port,
        "POST",
        "/v1/messages",
        Some(&status.token),
        Some(&body_json.replace("\"max_tokens\":256", "\"max_tokens\":256,\"stream\":true")),
    )
    .await;
    assert_eq!(code, 200, "{body}");
    assert!(body.contains(r#""type":"tool_use""#), "{body}");
    assert!(body.contains("input_json_delta"), "{body}");
    assert!(body.contains(r#""stop_reason":"tool_use""#), "{body}");
}

#[tokio::test]
async fn a_tiny_ceiling_from_a_client_is_raised_before_it_reaches_a_provider() {
    // The gateway builds its own submission instead of going through
    // `jobs::submit`, so it does not inherit that path's floor for free — and
    // a client asking for 512 gets nothing at all from a model that thinks
    // before it answers.
    let (_state, _gw, status, stub) = app_with_visible_backend().await;

    let (code, body) = request(
        status.port,
        "POST",
        "/v1/messages",
        Some(&status.token),
        Some(&format!(
            r#"{{"model":"{MODEL}","max_tokens":512,
                 "messages":[{{"role":"user","content":"hello"}}]}}"#
        )),
    )
    .await;
    assert_eq!(code, 200, "{body}");

    // Read back what the inference server was actually asked for, rather than
    // trusting that the floor was applied somewhere along the way.
    let completion = stub
        .requests()
        .into_iter()
        .find(|r| r.contains("\"messages\""))
        .expect("the worker called the inference server");
    let asked: serde_json::Value = serde_json::from_str(
        completion
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .unwrap_or(&completion),
    )
    .expect("a JSON completion request");

    assert!(
        asked["max_tokens"].as_u64().unwrap_or(0) >= 8192,
        "the provider was asked for {}, too low a ceiling to think under",
        asked["max_tokens"]
    );
}

#[tokio::test]
async fn the_chosen_model_is_what_an_unknown_name_falls_through_to() {
    // The Connect screen shows one model in its setup snippets and routes
    // unknown names to it. If those two disagreed, a tool configured from the
    // screen would quietly be served by something else.
    let (state, gw, status) = app_with_provider().await;
    state.set_setting("gateway_model", MODEL).unwrap();
    let after = gateway::reconcile(gw, state.clone()).await;
    assert_eq!(after.model.as_deref(), Some(MODEL));

    let (code, body) = request(
        status.port,
        "POST",
        "/v1/messages",
        Some(&status.token),
        Some(
            r#"{"model":"claude-opus-5","max_tokens":64,
                "messages":[{"role":"user","content":"hi"}]}"#,
        ),
    )
    .await;

    assert_eq!(code, 200, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        json["model"], MODEL,
        "the reply names the model that answered"
    );
}

#[tokio::test]
async fn no_choice_means_the_cheapest_one() {
    // The default is a sensible answer, not an unset field the user must fill
    // in before anything works.
    let (_state, _gw, status) = app_with_provider().await;
    assert_eq!(status.model, None);

    let (code, body) = request(
        status.port,
        "POST",
        "/v1/messages",
        Some(&status.token),
        Some(
            r#"{"model":"something-else","max_tokens":64,
                "messages":[{"role":"user","content":"hi"}]}"#,
        ),
    )
    .await;
    assert_eq!(code, 200, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["model"], MODEL);
}

#[tokio::test]
async fn codex_gets_a_responses_api_reply() {
    let (_state, _gw, status) = app_with_provider().await;

    let (code, body) = request(
        status.port,
        "POST",
        "/v1/responses",
        Some(&status.token),
        Some(&format!(
            r#"{{"model":"{MODEL}","input":"hello"}}"#
        )),
    )
    .await;

    assert_eq!(code, 200, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["object"], "response");
    assert_eq!(json["output_text"], REPLY);
    assert_eq!(json["status"], "completed");
}

#[tokio::test]
async fn a_streamed_responses_request_opens_an_output_item_before_any_delta() {
    // The bug this guards: Codex logs "OutputTextDelta without active item"
    // and discards the answer if the stream jumps from response.created to
    // a text delta. The Spark still ran the job; the editor just never
    // saw it.
    let (_state, _gw, status) = app_with_provider().await;

    let (code, body) = request(
        status.port,
        "POST",
        "/v1/responses",
        Some(&status.token),
        Some(&format!(
            r#"{{"model":"{MODEL}","input":"hello","stream":true}}"#
        )),
    )
    .await;

    assert_eq!(code, 200, "{body}");
    for event in [
        "event: response.created",
        "event: response.in_progress",
        "event: response.output_item.added",
        "event: response.content_part.added",
        "event: response.output_text.delta",
        "event: response.output_text.done",
        "event: response.content_part.done",
        "event: response.output_item.done",
        "event: response.completed",
    ] {
        assert!(body.contains(event), "missing {event} in:\n{body}");
    }
    assert!(body.contains(REPLY), "{body}");

    let created = body.find("event: response.output_item.added").unwrap();
    let part = body.find("event: response.content_part.added").unwrap();
    let delta = body.find("event: response.output_text.delta").unwrap();
    assert!(created < part && part < delta, "{body}");
}

#[tokio::test]
async fn a_responses_tool_call_stream_closes_the_item() {
    let sse = concat!(
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_ab","#,
        r#""function":{"name":"Read","arguments":"{\"path\":"}}]}}]}"#,
        "\n\n",
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"#,
        r#""function":{"arguments":"\"a.txt\"}"}}]}}]}"#,
        "\n\n",
        r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        "\n\n",
        "data: [DONE]\n\n",
    );
    let stub = StubHttp::start(vec![
        StubHttp::json(
            200,
            &format!(r#"{{"object":"list","data":[{{"id":"{MODEL}"}}]}}"#),
        ),
        StubHttp::sse(sse),
    ])
    .await;
    let worker_endpoint = start_worker(stub.base_url()).await;
    std::mem::forget(stub);

    let (_state, _gw, status) = app_at(worker_endpoint).await;

    let (code, body) = request(
        status.port,
        "POST",
        "/v1/responses",
        Some(&status.token),
        Some(&format!(
            r#"{{"model":"{MODEL}","input":"read a.txt","stream":true,
                 "tools":[{{"type":"function","name":"Read","description":"Read a file",
                            "parameters":{{"type":"object","properties":{{"path":{{"type":"string"}}}}}}}}]}}"#
        )),
    )
    .await;

    assert_eq!(code, 200, "{body}");
    assert!(body.contains("event: response.output_item.added"), "{body}");
    assert!(body.contains(r#""type":"function_call""#), "{body}");
    assert!(body.contains("event: response.function_call_arguments.delta"), "{body}");
    assert!(body.contains("event: response.function_call_arguments.done"), "{body}");
    assert!(body.contains("event: response.output_item.done"), "{body}");
    assert!(body.contains("event: response.completed"), "{body}");
    assert!(body.contains("a.txt"), "{body}");
}

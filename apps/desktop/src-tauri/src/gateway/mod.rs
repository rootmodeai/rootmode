//! A local HTTP endpoint that makes the network usable from the tools people
//! already have open.
//!
//! Claude Code, Cursor, Continue, Cline, Zed and Aider can all be pointed at a
//! different base URL. None of them knows what libp2p is, and none of them
//! should have to. So the app speaks the two API shapes they do know —
//! Anthropic Messages and OpenAI chat completions — and translates each
//! request into an ordinary rootmode job, routed to the cheapest provider
//! serving that model, exactly as the chat window does.
//!
//! Three deliberate limits:
//!
//! * **Loopback only.** The listener binds `127.0.0.1`. This is a door for
//!   programs on your machine, not a service you are accidentally hosting.
//! * **A token is always required.** Anything running as your user could
//!   otherwise spend your providers' time by guessing a port number.
//! * **Text only.** A rootmode worker serves chat completions. Tool calls
//!   arriving in a request are rendered into the prompt so their content is
//!   not lost, but they are not returned as structured tool calls, and image
//!   generation is not exposed here — it is a different job kind with no
//!   equivalent in either API.

pub mod translate;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::sse::{Event, KeepAlive, Sse},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rootmode_core::{
    protocol::ClientMessage, JobKind, JobPayload, JobStatus, JobSubmit, LlmParams, WorkerMessage,
};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{mpsc, Mutex};
use uuid::Uuid;

use crate::error::AppError;
use crate::state::AppState;
use crate::store::now;
use translate::{peel_thinking, Answer, AnthropicRequest, ChatRequest, OpenAiRequest, Usage};

pub const SETTING_GATEWAY: &str = "gateway";
pub const SETTING_GATEWAY_PORT: &str = "gateway_port";
/// Generated, not user-set: it exists so other programs on this machine
/// cannot use your providers without being told the token.
pub const SETTING_GATEWAY_TOKEN: &str = "gateway_token";
/// Whether an unknown model name falls through to the cheapest one on offer.
/// On by default — see [`run`] for why refusing is worse.
pub const SETTING_GATEWAY_SUBSTITUTE: &str = "gateway_substitute";
/// The model outside tools should get. Empty means "whichever is cheapest",
/// which is the right default but a poor answer when you have a preference.
pub const SETTING_GATEWAY_MODEL: &str = "gateway_model";

pub const DEFAULT_PORT: u16 = 11435;

/// What the UI shows on the Connect screen.
#[derive(Debug, Clone, Serialize)]
pub struct GatewayStatus {
    pub enabled: bool,
    /// True only when a listener is actually bound right now.
    pub running: bool,
    pub port: u16,
    pub base_url: String,
    pub token: String,
    /// Set when the last attempt to bind failed — usually the port is taken.
    pub error: Option<String>,
    /// Whether an unknown model name is answered by another model on offer
    /// instead of being refused.
    pub substitute: bool,
    /// The model outside tools are told to ask for, and the one an unknown
    /// name falls through to. `None` means whichever is cheapest.
    pub model: Option<String>,
    /// Requests served since the app started. Resets on restart; it is a sign
    /// of life, not an accounting record.
    pub requests: u64,
}

/// The running listener, if any.
#[derive(Default)]
pub struct Gateway {
    inner: Mutex<Option<Running>>,
    requests: std::sync::atomic::AtomicU64,
    last_error: Mutex<Option<String>>,
}

struct Running {
    port: u16,
    shutdown: tokio::sync::oneshot::Sender<()>,
}

#[derive(Clone)]
struct Ctx {
    state: Arc<AppState>,
    gateway: Arc<Gateway>,
}

impl Gateway {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn status(&self, state: &AppState) -> GatewayStatus {
        let running = self.inner.lock().await;
        let port = running
            .as_ref()
            .map(|r| r.port)
            .unwrap_or_else(|| configured_port(state));
        GatewayStatus {
            enabled: enabled(state),
            running: running.is_some(),
            port,
            base_url: format!("http://127.0.0.1:{port}"),
            token: token(state),
            substitute: substitutes(state),
            model: chosen_model(state),
            error: self.last_error.lock().await.clone(),
            requests: self.requests.load(std::sync::atomic::Ordering::Relaxed),
        }
    }
}

fn enabled(state: &AppState) -> bool {
    matches!(
        state
            .db
            .get_setting(SETTING_GATEWAY)
            .ok()
            .flatten()
            .as_deref(),
        Some("true")
    )
}

/// The model the user picked for outside tools, if they picked one.
fn chosen_model(state: &AppState) -> Option<String> {
    state
        .db
        .get_setting(SETTING_GATEWAY_MODEL)
        .ok()
        .flatten()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn substitutes(state: &AppState) -> bool {
    !matches!(
        state
            .db
            .get_setting(SETTING_GATEWAY_SUBSTITUTE)
            .ok()
            .flatten()
            .as_deref(),
        Some("false")
    )
}

fn configured_port(state: &AppState) -> u16 {
    state
        .db
        .get_setting(SETTING_GATEWAY_PORT)
        .ok()
        .flatten()
        .and_then(|s| s.trim().parse().ok())
        .filter(|p| *p > 0)
        .unwrap_or(DEFAULT_PORT)
}

/// The token, generated on first use so it exists before anyone turns the
/// gateway on and there is never a window where the door is open without one.
fn token(state: &AppState) -> String {
    if let Ok(Some(existing)) = state.db.get_setting(SETTING_GATEWAY_TOKEN) {
        if !existing.trim().is_empty() {
            return existing;
        }
    }
    use rand::Rng;
    let bytes: [u8; 24] = rand::thread_rng().gen();
    let fresh = format!("rm-{}", hex::encode(bytes));
    let _ = state.db.set_setting(SETTING_GATEWAY_TOKEN, &fresh);
    fresh
}

pub fn rotate_token(state: &AppState) -> crate::error::Result<String> {
    state.db.set_setting(SETTING_GATEWAY_TOKEN, "")?;
    Ok(token(state))
}

/// Bring the listener in line with the settings. Safe to call repeatedly.
pub async fn reconcile(gateway: Arc<Gateway>, state: Arc<AppState>) -> GatewayStatus {
    let want = enabled(&state);
    let port = configured_port(&state);

    {
        let mut guard = gateway.inner.lock().await;
        match guard.take() {
            // Already listening where we want to be listening.
            Some(running) if want && running.port == port => {
                *guard = Some(running);
                drop(guard);
                return gateway.status(&state).await;
            }
            Some(running) => {
                let _ = running.shutdown.send(());
            }
            None => {}
        }
    }

    *gateway.last_error.lock().await = None;
    if !want {
        return gateway.status(&state).await;
    }

    // Ensure the token exists before the door opens.
    let _ = token(&state);

    let ctx = Ctx {
        state: state.clone(),
        gateway: gateway.clone(),
    };
    let app = router(ctx);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));

    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => {
            let (tx, rx) = tokio::sync::oneshot::channel();
            tauri::async_runtime::spawn(async move {
                let served = axum::serve(listener, app)
                    .with_graceful_shutdown(async {
                        let _ = rx.await;
                    })
                    .await;
                if let Err(e) = served {
                    log::warn!("local endpoint stopped: {e}");
                }
            });
            *gateway.inner.lock().await = Some(Running { port, shutdown: tx });
            log::info!("local endpoint listening on http://{addr}");
        }
        Err(e) => {
            let msg = if e.kind() == std::io::ErrorKind::AddrInUse {
                format!("port {port} is already in use — pick another one")
            } else {
                format!("cannot listen on {addr}: {e}")
            };
            log::warn!("{msg}");
            *gateway.last_error.lock().await = Some(msg);
        }
    }

    gateway.status(&state).await
}

fn router(ctx: Ctx) -> Router {
    Router::new()
        // Anthropic — Claude Code.
        .route("/v1/messages", post(anthropic_messages))
        // OpenAI — Cursor, Continue, Cline, Zed, Aider, and the rest.
        .route("/v1/chat/completions", post(openai_completions))
        // OpenAI Responses — Codex, which dropped chat.completions support.
        .route("/v1/responses", post(openai_responses))
        .route("/v1/models", get(list_models))
        // Some clients probe the root before trusting a base URL.
        .route("/", get(|| async { "rootmode local endpoint" }))
        .with_state(ctx)
}

// ------------------------------------------------------------------ handlers

/// Which API shape a failure should be phrased in. A client parses errors as
/// strictly as it parses successes.
#[derive(Clone, Copy, PartialEq)]
enum Shape {
    Anthropic,
    OpenAi,
    Responses,
}

struct Failure {
    status: StatusCode,
    kind: &'static str,
    message: String,
    shape: Shape,
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let body = match self.shape {
            Shape::Anthropic => translate::anthropic_error(self.kind, &self.message),
            Shape::OpenAi => translate::openai_error(self.kind, &self.message),
            Shape::Responses => translate::responses_error(self.kind, &self.message),
        };
        (self.status, Json(body)).into_response()
    }
}

fn fail(
    shape: Shape,
    status: StatusCode,
    kind: &'static str,
    message: impl Into<String>,
) -> Failure {
    Failure {
        status,
        kind,
        message: message.into(),
        shape,
    }
}

/// Accept the token however the client habitually sends it: `x-api-key` is
/// what Anthropic clients use, `Authorization: Bearer` what OpenAI ones do.
fn authorized(ctx: &Ctx, headers: &HeaderMap) -> bool {
    let expected = token(&ctx.state);
    let presented = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| {
            headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(str::to_string)
        });

    presented.is_some_and(|p| {
        // Constant-time-ish: compare full length, do not early-exit on the
        // first differing byte. Overkill for loopback, cheap to do right.
        p.len() == expected.len()
            && p.bytes()
                .zip(expected.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
    })
}

async fn anthropic_messages(
    State(ctx): State<Ctx>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    serve(ctx, headers, body, Shape::Anthropic).await
}

async fn openai_completions(
    State(ctx): State<Ctx>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    serve(ctx, headers, body, Shape::OpenAi).await
}

async fn openai_responses(
    State(ctx): State<Ctx>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    serve(ctx, headers, body, Shape::Responses).await
}

async fn serve(ctx: Ctx, headers: HeaderMap, body: axum::body::Bytes, shape: Shape) -> Response {
    if !authorized(&ctx, &headers) {
        return fail(
            shape,
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "missing or wrong token — copy it from rootmode's Connect screen",
        )
        .into_response();
    }

    let parsed: Result<ChatRequest, Failure> = match shape {
        Shape::Anthropic => serde_json::from_slice::<AnthropicRequest>(&body)
            .map_err(|e| {
                fail(
                    shape,
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    e.to_string(),
                )
            })
            .and_then(|r| {
                r.into_chat()
                    .map_err(|e| fail(shape, StatusCode::BAD_REQUEST, "invalid_request_error", e.0))
            }),
        Shape::OpenAi => serde_json::from_slice::<OpenAiRequest>(&body)
            .map_err(|e| {
                fail(
                    shape,
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    e.to_string(),
                )
            })
            .and_then(|r| {
                r.into_chat()
                    .map_err(|e| fail(shape, StatusCode::BAD_REQUEST, "invalid_request_error", e.0))
            }),
        Shape::Responses => serde_json::from_slice::<translate::ResponsesRequest>(&body)
            .map_err(|e| {
                fail(
                    shape,
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    e.to_string(),
                )
            })
            .and_then(|r| {
                r.into_chat()
                    .map_err(|e| fail(shape, StatusCode::BAD_REQUEST, "invalid_request_error", e.0))
            }),
    };

    let request = match parsed {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };

    ctx.gateway
        .requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // A streamed request gets its own path: `stream_answer` starts sending
    // frames the instant the job is accepted, rather than waiting for the
    // whole answer and only then producing what looks like a stream. See its
    // doc comment for why that distinction is not cosmetic — a slow or
    // cold-starting provider needs those early bytes to keep the client from
    // giving up before an answer ever arrives.
    if request.stream {
        let params = request.params.clone();
        return match submit(&ctx.state, request).await {
            Ok((served, rx, runner)) => {
                stream_answer(shape, served, rx, runner, params).into_response()
            }
            Err(e) => {
                trace(&body, &e.to_string());
                let (status, kind) = classify(&e);
                fail(shape, status, kind, e.to_string()).into_response()
            }
        };
    }

    match run(&ctx.state, request).await {
        // The reply names the model that answered, which after a substitution
        // is not the one that was asked for. Saying otherwise would hide the
        // one thing worth knowing about the response.
        Ok((answer, served)) => respond(shape, &served, &answer),
        Err(e) => {
            // A failure is exactly when the request that caused it is worth
            // having. Opt-in, because prompts are the most sensitive thing
            // this app handles and nobody should have to discover later that
            // they were on disk.
            trace(&body, &e.to_string());
            let (status, kind) = classify(&e);
            fail(shape, status, kind, e.to_string()).into_response()
        }
    }
}

/// Append a failed request and its error to the file named by
/// `ROOTMODE_GATEWAY_TRACE`. Does nothing when the variable is unset.
///
/// Diagnosing a client that fails only in its own hands means seeing what it
/// actually sent — guessing at the shape from the outside is how an afternoon
/// disappears.
fn trace(body: &[u8], error: &str) {
    let Ok(path) = std::env::var("ROOTMODE_GATEWAY_TRACE") else {
        return;
    };
    use std::io::Write;
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        log::warn!("cannot write the request trace to {path}");
        return;
    };
    let _ = writeln!(
        file,
        "--- {} failed: {error}\n{}",
        now(),
        String::from_utf8_lossy(body)
    );
}

fn classify(e: &AppError) -> (StatusCode, &'static str) {
    match e {
        AppError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found_error"),
        AppError::Invalid(_) => (StatusCode::BAD_REQUEST, "invalid_request_error"),
        _ => (StatusCode::BAD_GATEWAY, "api_error"),
    }
}

/// The non-streaming reply: a request submitted with `stream: false` still
/// waits for the whole answer, same as it always did — there is no client
/// expecting early bytes on that path, so nothing about the fix above
/// applies here. Streamed requests go through `stream_answer` instead, well
/// before this is ever reached.
fn respond(shape: Shape, model: &str, answer: &Answer) -> Response {
    let id = format!("msg_{}", Uuid::new_v4().simple());
    match shape {
        Shape::Anthropic => Json(translate::anthropic_message(&id, model, answer)).into_response(),
        Shape::OpenAi => Json(translate::openai_completion(&id, model, answer, now())).into_response(),
        Shape::Responses => Json(translate::openai_response(&id, model, answer, now())).into_response(),
    }
}

/// Every text model the network is currently serving, in both shapes at once
/// — the two catalogue formats differ only in field names, and a client reads
/// whichever one it understands.
async fn list_models(State(ctx): State<Ctx>, headers: HeaderMap) -> Response {
    if !authorized(&ctx, &headers) {
        return fail(
            Shape::OpenAi,
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "missing or wrong token",
        )
        .into_response();
    }

    let peers = match ctx.state.db.list_peers() {
        Ok(p) => p,
        Err(e) => {
            return fail(
                Shape::OpenAi,
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                e.to_string(),
            )
            .into_response()
        }
    };

    let created = now();
    let models: Vec<Value> = crate::routing::model_options(&peers, JobKind::Llm)
        .into_iter()
        .map(|o| {
            serde_json::json!({
                "id": o.model,
                "object": "model",
                "type": "model",
                "created": created,
                "created_at": created,
                "display_name": o.model,
                "owned_by": "rootmode",
            })
        })
        .collect();

    Json(serde_json::json!({
        "object": "list",
        "data": models,
        "has_more": false,
    }))
    .into_response()
}

// ----------------------------------------------------------------- execution

type Runner = tauri::async_runtime::JoinHandle<crate::error::Result<()>>;

/// Choose a provider and hand it the job, without waiting for anything back
/// — shared by the buffered path ([`run`]) and the streamed one
/// ([`stream_answer`]), which differ only in what they do with the channel
/// this returns.
async fn submit(
    state: &Arc<AppState>,
    request: ChatRequest,
) -> crate::error::Result<(String, mpsc::UnboundedReceiver<WorkerMessage>, Runner)> {
    let peers = state.db.list_peers()?;

    let choice = match crate::routing::provider_for(&peers, JobKind::Llm, &request.model) {
        Some(exact) => exact,
        None => {
            let offered = crate::routing::model_options(&peers, JobKind::Llm);
            if offered.is_empty() {
                return Err(AppError::NotFound(
                    "no provider is online right now — open rootmode and wait for one to appear"
                        .into(),
                ));
            }
            if !substitutes(state) {
                let names: Vec<String> = offered.into_iter().map(|o| o.model).collect();
                return Err(AppError::NotFound(format!(
                    "no provider serves '{}'. Currently on offer: {}",
                    request.model,
                    names.join(", ")
                )));
            }
            // The user's choice if they made one and it is still being served,
            // otherwise the cheapest — `model_options` is sorted that way.
            let preferred = chosen_model(state);
            let pick = preferred
                .as_deref()
                .and_then(|want| offered.iter().find(|o| o.model == want).cloned())
                .or_else(|| offered.into_iter().next())
                .expect("checked non-empty");
            log::info!(
                "no provider serves '{}'; using '{}' instead",
                request.model,
                pick.model
            );
            pick
        }
    };
    let served = choice.model.clone();

    let peer = state
        .db
        .get_peer(&choice.peer_id)?
        .ok_or_else(|| AppError::NotFound(format!("peer {}", choice.peer_id)))?;

    // Ask for the model that was actually chosen, not the name the client
    // sent — after a substitution those differ, and the worker would reject a
    // model it has never heard of.
    let mut params = request.params;
    params.model_id = Some(served.clone());

    // The gateway builds its own submission rather than going through
    // `jobs::submit`, so it has to apply the same floor — sharing the function
    // rather than the code path.
    let payload = crate::jobs::with_workable_ceiling(JobPayload::Llm(params));
    payload.validate()?;

    let transport = crate::jobs::transport_for(state, &peer).await?;
    let job_id = Uuid::new_v4();
    let mut submit = JobSubmit::new(job_id, state.identity().peer_id(), payload.clone());
    if let Some(price) = peer
        .models
        .iter()
        .find(|m| m.id == served)
        .and_then(|m| m.price.clone())
        .filter(|p| p.amount > 0.0)
    {
        if let Ok(st) = crate::pot::status(state).await {
            if let (Some(client), Some(_cfg)) = (st.client.as_deref(), crate::pot::load_chain_config(state)) {
                submit.payer = Some(client.to_string());
                let (bond, reserve) = crate::pot::issue_ticket(
                    state,
                    job_id,
                    price,
                    JobKind::Llm,
                    &payload,
                    client,
                    &crate::pot::named_payout(peer.payout.as_deref())?,
                    &peer.label,
                )
                .await?;
                submit.bond = Some(bond);
                submit.reserve = reserve;
            }
        }
    }

    let (worker_tx, mut worker_rx) = mpsc::unbounded_channel::<WorkerMessage>();
    let (client_tx, client_rx) = mpsc::unbounded_channel::<WorkerMessage>();
    let (pay_tx, pay_rx) = mpsc::unbounded_channel::<ClientMessage>();
    let runner = {
        let transport = transport.clone();
        let state = state.clone();
        // An HTTP client has no way to press Stop, so nothing here ever
        // notifies this — it exists only because `run_job` takes one.
        let stop = std::sync::Arc::new(tokio::sync::Notify::new());
        tauri::async_runtime::spawn(async move {
            let drive = tauri::async_runtime::spawn(async move {
                transport
                    .run_job(submit, worker_tx, stop, pay_rx)
                    .await
            });
            while let Some(msg) = worker_rx.recv().await {
                if let WorkerMessage::JobInvoice(inv) = &msg {
                    match crate::pot::pay_invoice(&state, job_id, inv).await {
                        Ok(pay) => {
                            let _ = pay_tx.send(ClientMessage::JobPay(pay));
                        }
                        Err(e) => log::warn!("gateway pay: {e}"),
                    }
                }
                if client_tx.send(msg).is_err() {
                    break;
                }
            }
            let out = match drive.await {
                Ok(r) => r,
                Err(e) => Err(AppError::Net(format!("provider task failed: {e}"))),
            };
            // The chat pipeline clears its lock in `settle_job`; the gateway
            // has no settle step, so clear it here or priced gateway jobs
            // accumulate in the pending map forever.
            crate::pot::drop_job(job_id);
            out
        })
    };

    Ok((served, client_rx, runner))
}

/// Route the request and run it, returning the answer, what it cost, and the
/// model that actually served it.
///
/// This is the same routing the chat window uses — cheapest provider serving
/// that model, latency breaking ties — so a model chosen in Cursor behaves
/// exactly like the same model chosen in the app.
///
/// The one difference is what happens when the name is unknown. Editors do
/// not only send the model you configured: Claude Code names its own small
/// model for background work like naming a conversation, and no rootmode
/// provider will ever serve anything by that name. Refusing those requests
/// makes the app look broken for a reason the user cannot act on, so by
/// default an unknown name falls through to the cheapest model on offer. The
/// substitution is never hidden — it is announced on the Connect screen, and
/// the reply says which model answered.
/// The peer's claimed hash is checked the same way it is for a job run from
/// the app — a result nobody verified is a result nobody should act on, and
/// here it flows straight into an editor. Shared by the buffered and live
/// paths, so a mid-stream result is held to the same standard as one that
/// arrived all at once.
fn build_answer(r: rootmode_core::JobResult, params: &LlmParams) -> crate::error::Result<Answer> {
    // A result with no text but a tool call is a model choosing to act; only
    // both being absent is a broken answer.
    let raw = r.text.clone().unwrap_or_default();
    if raw.is_empty() && r.tool_calls.is_empty() {
        return Err(AppError::Invalid("provider returned no answer".into()));
    }
    let actual = rootmode_core::sha256_hex(raw.as_bytes());
    if !r.sha256.is_empty() && !actual.eq_ignore_ascii_case(&r.sha256) {
        return Err(AppError::Invalid(format!(
            "hash mismatch: provider claimed {}, bytes hash to {actual}",
            r.sha256
        )));
    }
    let text = peel_thinking(&raw);
    if text.is_empty() && r.tool_calls.is_empty() {
        return Err(AppError::Invalid(
            "provider returned only reasoning, no answer".into(),
        ));
    }
    Ok(Answer {
        text,
        tool_calls: r.tool_calls.clone(),
        usage: Usage::billed(
            params,
            &raw,
            r.thinking.as_deref(),
            &r.tool_calls,
            &r.meta,
        ),
        finish: r
            .meta
            .get("finish_reason")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

async fn run(
    state: &Arc<AppState>,
    request: ChatRequest,
) -> crate::error::Result<(Answer, String)> {
    let params = request.params.clone();
    let (served, mut rx, runner) = submit(state, request).await?;

    let mut answer: Option<Answer> = None;
    let mut failure: Option<String> = None;

    while let Some(msg) = rx.recv().await {
        match msg {
            WorkerMessage::JobResult(r) => {
                answer = Some(build_answer(r, &params)?);
            }
            WorkerMessage::JobStatus(s) if s.status == JobStatus::Failed => {
                failure = Some(s.error.unwrap_or_else(|| "provider failed the job".into()));
            }
            _ => {}
        }
    }

    match runner.await {
        Ok(Ok(())) => {}
        // A reason the provider gave beats one the transport inferred: "the
        // model's context is 8192 tokens" is actionable, "the stream ended"
        // is not.
        Ok(Err(e)) if answer.is_none() && failure.is_none() => return Err(e),
        Ok(Err(_)) => {}
        Err(e) if answer.is_none() && failure.is_none() => {
            return Err(AppError::Net(format!("provider task failed: {e}")))
        }
        Err(_) => {}
    }

    match (answer, failure) {
        (Some(a), _) => Ok((a, served)),
        (None, Some(e)) => Err(AppError::Net(e)),
        (None, None) => Err(AppError::Net(
            "provider closed the connection without answering".into(),
        )),
    }
}

/// An SSE frame's `data:` payload — JSON for every real event, except the
/// OpenAI dialect's closing `[DONE]`, which is the literal text, not a JSON
/// string (that would send `"[DONE]"`, quotes and all, which is not what any
/// client is looking for).
enum FrameData {
    Json(Value),
    Raw(&'static str),
}

/// One SSE frame: `event:` is set only when the dialect uses named events
/// (Anthropic, Responses) — OpenAI's chunks carry no event name, just `data:`.
type Frame = (Option<String>, FrameData);

fn wrap(named: (String, Value)) -> Frame {
    (Some(named.0), FrameData::Json(named.1))
}

/// State carried across `stream::unfold` iterations. One `Frame` goes out
/// per poll; a `WorkerMessage` can expand into several (a job result closes
/// out a text block, opens N tool-call blocks, then closes the message), so
/// they queue in `pending` and drain before the next message is awaited.
struct LiveState {
    rx: mpsc::UnboundedReceiver<WorkerMessage>,
    runner: Option<Runner>,
    pending: std::collections::VecDeque<Frame>,
    /// Raw text seen so far, thinking tags and all — `peel_thinking` needs
    /// the whole run to correctly drop a tag split across two deltas, so this
    /// is not the incremental piece itself, just what it is diffed against.
    raw: String,
    /// How much of the *peeled* text has already gone out as a delta.
    emitted: usize,
    /// Set once a text content block has been opened (Anthropic only, which
    /// frames content in blocks the other two dialects don't have).
    text_index: Option<usize>,
    started: bool,
    done: bool,
    served: String,
    id: String,
    created: i64,
    shape: Shape,
    /// Monotonic Responses API `sequence_number`. Codex does not strictly
    /// require it, but the spec puts one on every event and some parsers
    /// drop frames that skip.
    seq: u64,
    /// The request that produced this stream, so a finished result can be
    /// counted with the OpenAI tokenizer rather than trusted from the worker.
    params: LlmParams,
}

impl LiveState {
    fn push_responses(&mut self, events: impl IntoIterator<Item = (String, Value)>) {
        for (name, mut data) in events {
            self.seq += 1;
            if let Some(obj) = data.as_object_mut() {
                obj.insert("sequence_number".into(), self.seq.into());
            }
            self.pending.push_back((Some(name), FrameData::Json(data)));
        }
    }
}

impl Drop for LiveState {
    /// The one signal a dropped `unfold` state ever gets: nothing calls this
    /// on purpose, `stream::unfold` just stops polling and lets it fall out
    /// of scope — which is exactly what happens when the HTTP client hangs
    /// up before an answer arrives. The job itself keeps running regardless
    /// (`runner` is a detached task, not aborted here), and whatever it sends
    /// after this point lands in a channel nobody is reading — silently, by
    /// design, everywhere a worker message is forwarded with `let _ =
    /// tx.send(...)`. Logging here is what turns that silence into something
    /// visible: without it, "the client gave up early" and "the gateway
    /// swallowed an error" look identical from the outside.
    fn drop(&mut self) {
        if !self.done {
            log::warn!(
                "gateway stream to {} for {} was dropped before it finished — \
                 the client most likely disconnected before the answer arrived; \
                 the job itself is still running and its result will be discarded \
                 when it lands",
                self.served,
                self.id,
            );
        }
    }
}

fn push_error(state: &mut LiveState, message: &str) {
    // A live request's failure has nowhere else to surface — there is no
    // final non-2xx HTTP response for it the way a buffered request gets;
    // the SSE stream already answered 200 OK when it opened. Logging here is
    // what makes a stream that dies mid-answer visible at all instead of
    // just going quiet.
    log::warn!("gateway stream to {} failed: {message}", state.served);
    match state.shape {
        Shape::Anthropic => state
            .pending
            .push_back(wrap(translate::anthropic_live_error(message))),
        Shape::OpenAi => state
            .pending
            .push_back((None, FrameData::Json(translate::openai_live_error(message)))),
        Shape::Responses => state.push_responses([translate::responses_live_error(message)]),
    }
}

/// Turn a submitted job's message channel into a live SSE stream, in
/// whichever dialect `shape` asks for: one frame the instant the job is
/// accepted, then one per token as the worker actually sends them, closing
/// with the same tool-call and usage framing a buffered answer gets. See the
/// `live` section of `translate` for why this exists instead of building the
/// whole stream from a finished [`Answer`] the way the non-streaming path
/// still does internally.
fn stream_answer(
    shape: Shape,
    served: String,
    rx: mpsc::UnboundedReceiver<WorkerMessage>,
    runner: Runner,
    params: LlmParams,
) -> Sse<impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let state = LiveState {
        rx,
        runner: Some(runner),
        pending: std::collections::VecDeque::new(),
        raw: String::new(),
        emitted: 0,
        text_index: None,
        started: false,
        done: false,
        served,
        id: format!("msg_{}", Uuid::new_v4().simple()),
        created: now(),
        shape,
        seq: 0,
        params,
    };

    let frames = futures_util::stream::unfold(state, |mut st| async move {
        loop {
            if let Some((name, data)) = st.pending.pop_front() {
                let body = match data {
                    FrameData::Json(v) => v.to_string(),
                    FrameData::Raw(s) => s.to_string(),
                };
                let mut event = Event::default().data(body);
                if let Some(name) = name {
                    event = event.event(name);
                }
                return Some((Ok(event), st));
            }
            if st.done {
                return None;
            }
            if !st.started {
                st.started = true;
                match st.shape {
                    Shape::Anthropic => st
                        .pending
                        .push_back(wrap(translate::anthropic_live_start(&st.id, &st.served))),
                    Shape::OpenAi => st.pending.push_back((
                        None,
                        FrameData::Json(translate::openai_live_start(&st.id, &st.served, st.created)),
                    )),
                    Shape::Responses => st.push_responses([
                        translate::responses_live_start(&st.id, &st.served, st.created),
                        translate::responses_live_in_progress(&st.id, &st.served, st.created),
                    ]),
                }
                continue;
            }

            match st.rx.recv().await {
                Some(WorkerMessage::JobDelta(d)) => {
                    st.raw.push_str(&d.text);
                    let peeled = translate::peel_thinking(&st.raw);
                    // `peel_thinking` rewrites the string, so a byte offset from
                    // a prior pass may land mid-character; clamp to a boundary
                    // rather than panic-slice on worker-controlled text.
                    if peeled.len() > st.emitted && peeled.is_char_boundary(st.emitted) {
                        let fresh = peeled[st.emitted..].to_string();
                        st.emitted = peeled.len();
                        match st.shape {
                            Shape::Anthropic => {
                                if st.text_index.is_none() {
                                    st.text_index = Some(0);
                                    st.pending.push_back(wrap(translate::anthropic_live_text_start(0)));
                                }
                                st.pending
                                    .push_back(wrap(translate::anthropic_live_text_delta(0, &fresh)));
                            }
                            Shape::OpenAi => st.pending.push_back((
                                None,
                                FrameData::Json(translate::openai_live_delta(&st.id, &st.served, st.created, &fresh)),
                            )),
                            Shape::Responses => {
                                if st.text_index.is_none() {
                                    st.text_index = Some(0);
                                    st.push_responses(translate::responses_live_text_start(&st.id));
                                }
                                st.push_responses([translate::responses_live_delta(&st.id, &fresh)]);
                            }
                        }
                    }
                    continue;
                }
                Some(WorkerMessage::JobResult(r)) => {
                    match build_answer(r, &st.params) {
                        Ok(answer) => {
                            match st.shape {
                                Shape::Anthropic => st
                                    .pending
                                    .extend(translate::anthropic_live_end(&answer, st.text_index).into_iter().map(wrap)),
                                Shape::OpenAi => st.pending.extend(
                                    translate::openai_live_end(&st.id, &st.served, st.created, &answer)
                                        .into_iter()
                                        .map(|v| (None, FrameData::Json(v))),
                                ),
                                Shape::Responses => st.push_responses(translate::responses_live_end(
                                    &st.id,
                                    &st.served,
                                    &answer,
                                    st.created,
                                    st.text_index.is_some(),
                                )),
                            }
                            if matches!(st.shape, Shape::OpenAi) {
                                st.pending.push_back((None, FrameData::Raw("[DONE]")));
                            }
                        }
                        Err(e) => push_error(&mut st, &e.to_string()),
                    }
                    st.done = true;
                    continue;
                }
                Some(WorkerMessage::JobStatus(s)) if s.status == JobStatus::Failed => {
                    let message = s.error.unwrap_or_else(|| "provider failed the job".into());
                    push_error(&mut st, &message);
                    st.done = true;
                    continue;
                }
                Some(_) => continue,
                // The channel closed with no result and no failure status —
                // mirror `run`'s own fallback so a live request is held to
                // the same standard a buffered one is.
                None => {
                    let message = match st.runner.take() {
                        Some(r) => match r.await {
                            Ok(Ok(())) => "provider closed the connection without answering".to_string(),
                            Ok(Err(e)) => e.to_string(),
                            Err(e) => format!("provider task failed: {e}"),
                        },
                        None => "provider closed the connection without answering".to_string(),
                    };
                    push_error(&mut st, &message);
                    st.done = true;
                    continue;
                }
            }
        }
    });

    // A comment ping while a cold worker is still loading a model — the one
    // thing that keeps a proxy or client from deciding an idle-but-alive
    // connection is a dead one. 5s rather than axum's 15s default because
    // Codex's stream sits silent through a long prefill (a Spark thinking
    // for ~10s is the common case) and some clients treat an SSE comment as
    // the only proof of life.
    Sse::new(frames).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(5)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_port_is_not_one_something_else_owns() {
        // 11434 is Ollama's. Sitting on it would make rootmode look broken
        // for anyone who has Ollama installed, and vice versa.
        assert_ne!(DEFAULT_PORT, 11434);
    }
}

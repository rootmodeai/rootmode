//! Job lifecycle: submit → stream worker messages → persist → notify the UI.
//!
//! Submission returns as soon as the job row exists. Everything after that
//! happens on a tokio task and reaches the frontend as events, so no job ever
//! blocks the UI.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use rootmode_core::{
    protocol::ClientMessage, JobDelta, JobKind, JobPayload, JobStatus, JobSubmit, WorkerMessage,
};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::error::{AppError, Result};
use crate::mock::MockTransport;
use crate::net::{Transport, WsTransport};
use crate::results;
use crate::state::AppState;
use crate::store::{now, JobRecord, Peer};

/// The smallest answer ceiling worth sending to a provider.
///
/// A reasoning model spends tokens thinking before it writes a word, and that
/// thinking is billed against `max_tokens` but never returned. A caller asking
/// for 2048 is therefore not capping the reply at 2048 — it is capping
/// thinking *plus* reply, and on such a model gets no reply at all.
///
/// This lives here, in the one place every caller crosses, rather than in each
/// of them. It has already been got wrong separately in the chat screen and in
/// the local HTTP endpoint; a rule enforced per-path is a rule that protects
/// only the paths somebody remembered.
pub const MIN_ANSWER_TOKENS: u32 = 8192;

/// Raise a ceiling too low to think under, leaving a generous one alone.
///
/// Honours what the caller meant — bound the answer — rather than what it
/// literally said, and cannot make an answer longer than the model wanted to
/// give: `max_tokens` is a ceiling, not a target.
pub fn with_workable_ceiling(payload: JobPayload) -> JobPayload {
    match payload {
        JobPayload::Llm(mut params) => {
            params.max_tokens = params.max_tokens.max(MIN_ANSWER_TOKENS);
            JobPayload::Llm(params)
        }
        other => other,
    }
}

pub const EVENT_JOB_UPDATE: &str = "job:update";
pub const EVENT_JOB_RESULT: &str = "job:result";
pub const EVENT_JOB_DELTA: &str = "job:delta";
pub const EVENT_PEER_UPDATE: &str = "peer:update";
/// A reply was filed into a conversation.
pub const EVENT_MESSAGE_NEW: &str = "message:new";

/// Build the transport for a peer row. This is the seam a libp2p backend
/// would slot into.
pub async fn transport_for(state: &AppState, peer: &Peer) -> Result<Arc<dyn Transport>> {
    if peer.is_mock() {
        return Ok(Arc::new(MockTransport));
    }
    if peer.endpoint.starts_with(crate::p2p::P2P_SCHEME) || peer.endpoint.starts_with('/') {
        let node = state.p2p_node().await?;
        // A pasted address carries a route; tell the node about it so the
        // dial does not depend on somebody else having published it.
        if peer.endpoint.starts_with('/') {
            let (id, addr) = crate::p2p::split_multiaddr(&peer.endpoint)?;
            node.add_address(id, addr).await;
        }
        return crate::p2p::transport(
            node,
            &peer.endpoint,
            state.identity(),
            peer.public_key.clone(),
            state.sign_jobs(),
        );
    }
    Ok(Arc::new(WsTransport::new(
        peer.endpoint.clone(),
        state.identity(),
        peer.public_key.clone(),
        state.sign_jobs(),
    )?))
}

pub async fn submit(
    app: &AppHandle,
    peer_row_id: &str,
    payload: JobPayload,
    conversation_id: Option<String>,
) -> Result<JobRecord> {
    let state = app.state::<Arc<AppState>>().inner().clone();

    let payload = with_workable_ceiling(payload);
    payload.validate()?;

    let peer = state
        .db
        .get_peer(peer_row_id)?
        .ok_or_else(|| AppError::NotFound(format!("peer {peer_row_id}")))?;

    let kind = payload.kind();
    if !peer.caps.is_empty() && !peer.caps.iter().any(|c| c == kind.as_str()) {
        return Err(AppError::Invalid(format!(
            "peer '{}' advertises [{}] and cannot run {} jobs",
            peer.label,
            peer.caps.join(", "),
            kind.as_str()
        )));
    }

    let ts = now();
    let record = JobRecord {
        job_id: Uuid::new_v4(),
        conversation_id,
        peer_id: peer.id.clone(),
        peer_label: peer.label.clone(),
        kind,
        summary: payload.summary(),
        model: payload.model_label(),
        payload: payload.clone(),
        status: JobStatus::Queued,
        progress: 0.0,
        error: None,
        created_at: ts,
        updated_at: ts,
    };
    state.db.insert_job(&record)?;
    let _ = app.emit(EVENT_JOB_UPDATE, &record);

    // Who gets the job if this provider produces nothing. Decided now, from
    // the network as it was when the user chose, so a retry is not steered
    // by whatever a failure did to the peer list.
    let fallbacks = state
        .db
        .list_peers()
        .map(|peers| crate::routing::fallbacks(&peers, kind, &record.model, &peer.id))
        .unwrap_or_default();

    let job_id = record.job_id;
    let app = app.clone();
    // Registered before the job starts, so a Stop click that arrives in the
    // instant between "queued" and the first real message still has
    // something to notify.
    let (stop, stop_asked, _running) = state.track_job(job_id);

    tauri::async_runtime::spawn(async move {
        let _running = _running;
        let mut peer = peer;
        let mut payload = payload;
        let mut next = fallbacks.into_iter();
        loop {
            let why = match attempt(&app, &state, job_id, &peer, &payload, stop.clone()).await {
                Outcome::Settled => break,
                Outcome::Nothing(why) => why,
            };
            // Whatever was locked for that provider is let go; whether it
            // kept anything is for the chain to say.
            crate::pot::abandon_job(&state, job_id);
            if stop_asked.load(Ordering::SeqCst) {
                fail(&app, &state, job_id, &why);
                break;
            }
            let Some((alternate, model)) = next.find_map(|f| {
                state
                    .db
                    .get_peer(&f.peer_id)
                    .ok()
                    .flatten()
                    .map(|p| (p, f.model))
            }) else {
                fail(&app, &state, job_id, &why);
                break;
            };
            log::info!(
                "job {job_id}: {} gave nothing ({why}); trying {} instead",
                peer.label,
                alternate.label
            );
            payload = for_model(payload, &model);
            if let Err(e) = state.db.reassign_job(job_id, &alternate.id, &payload) {
                fail(&app, &state, job_id, &e.to_string());
                break;
            }
            emit_job(&app, &state, job_id);
            peer = alternate;
        }
    });

    Ok(record)
}

/// How one provider's try at a job ended.
enum Outcome {
    /// The job reached a recorded terminal state — done, or failed after
    /// the provider had already said something the user saw.
    Settled,
    /// The provider produced nothing: a failure or silence before a single
    /// token. Nothing about this try was shown or kept, and the job is
    /// still open; somebody else can have it.
    Nothing(String),
}

/// The same request, asked of a different model — the case where a free
/// provider stands in for another free provider.
fn for_model(payload: JobPayload, model: &str) -> JobPayload {
    match payload {
        JobPayload::Llm(mut p) if p.model_id.is_some() && p.model_id.as_deref() != Some(model) => {
            p.model_id = Some(model.to_string());
            JobPayload::Llm(p)
        }
        other => other,
    }
}

/// One provider's try: lock funds if it charges, stream what it sends,
/// persist what the user should see. A failure before the first token is
/// reported as [`Outcome::Nothing`] rather than written to the job, so the
/// caller can hand the job on without the user ever seeing a spinner turn
/// into an error and back.
async fn attempt(
    app: &AppHandle,
    state: &Arc<AppState>,
    job_id: Uuid,
    peer: &Peer,
    payload: &JobPayload,
    stop: Arc<tokio::sync::Notify>,
) -> Outcome {
    let transport = match transport_for(state, peer).await {
        Ok(t) => t,
        Err(e) => return Outcome::Nothing(e.to_string()),
    };
    let kind = payload.kind();
    let model = payload.model_label();

    let mut submit_msg = JobSubmit::new(job_id, state.identity().peer_id(), payload.clone());
    if let Some(price) = peer
        .models
        .iter()
        .find(|m| m.id == model || model.starts_with(&m.id))
        .and_then(|m| m.price.as_ref())
        .filter(|p| p.amount > 0.0)
    {
        if let Ok(st) = crate::pot::status(state).await {
            if let (Some(client), Some(_cfg)) = (st.client.as_deref(), crate::pot::load_chain_config(state)) {
                // A wallet that cannot lock funds is the user's to see; no
                // other provider fixes it, so it is final, not retried.
                let payout = match crate::pot::named_payout(peer.payout.as_deref()) {
                    Ok(p) => p,
                    Err(e) => {
                        fail(app, state, job_id, &format!("could not lock funds for this job: {e}"));
                        return Outcome::Settled;
                    }
                };
                match crate::pot::issue_ticket(
                    state,
                    job_id,
                    price.clone(),
                    kind,
                    payload,
                    client,
                    &payout,
                    &peer.label,
                )
                .await
                {
                    Ok((bond, reserve)) => {
                        log::info!(
                            "priced job {job_id} payout={payout} reserve={}",
                            reserve.is_some()
                        );
                        submit_msg.payer = Some(client.to_string());
                        submit_msg.bond = Some(bond);
                        submit_msg.reserve = reserve;
                    }
                    Err(e) => {
                        fail(app, state, job_id, &format!("could not lock funds for this job: {e}"));
                        return Outcome::Settled;
                    }
                }
            }
        }
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<WorkerMessage>();
    let (pay_tx, pay_rx) = mpsc::unbounded_channel::<ClientMessage>();
    let runner = {
        let transport = transport.clone();
        tauri::async_runtime::spawn(async move { transport.run_job(submit_msg, tx, stop, pay_rx).await })
    };

    let mut terminal = false;
    // Whether anything reached the user. Once it has, what follows is this
    // job's verdict; before it, a failure is only this provider's.
    let mut spoke = false;
    let mut last_meta: Option<serde_json::Value> = None;
    let mut nothing: Option<String> = None;
    while let Some(msg) = rx.recv().await {
        match &msg {
            WorkerMessage::JobDelta(d) if d.job_id == job_id && !d.is_empty() => spoke = true,
            WorkerMessage::JobStatus(s)
                if s.job_id == job_id && matches!(s.status, JobStatus::Failed) && !spoke =>
            {
                nothing = Some(
                    s.error
                        .clone()
                        .unwrap_or_else(|| "the provider failed without saying why".into()),
                );
                break;
            }
            // An empty answer is not an answer either.
            WorkerMessage::JobResult(r)
                if r.job_id == job_id
                    && !spoke
                    && r.kind == JobKind::Llm
                    && r.text.as_deref().unwrap_or("").trim().is_empty()
                    && r.tool_calls.is_empty() =>
            {
                nothing = Some("the provider returned no answer".into());
                break;
            }
            _ => {}
        }
        if let WorkerMessage::JobResult(r) = &msg {
            last_meta = Some(r.meta.clone());
        }
        if let WorkerMessage::JobInvoice(inv) = &msg {
            match crate::pot::pay_invoice(state, job_id, inv).await {
                Ok(pay) => {
                    if pay_tx.send(ClientMessage::JobPay(pay)).is_err() {
                        fail(app, state, job_id, "could not send payment to the worker");
                        terminal = true;
                    }
                }
                Err(e) => {
                    fail(app, state, job_id, &e.to_string());
                    terminal = true;
                }
            }
        }
        match handle_message(app, state, job_id, msg) {
            Ok(is_terminal) => terminal |= is_terminal,
            Err(e) => {
                fail(app, state, job_id, &e.to_string());
                terminal = true;
            }
        }
    }
    if let Some(why) = nothing {
        runner.abort();
        return Outcome::Nothing(why);
    }

    // The stream is over, so the runner is too; this only reads how.
    let ended = match runner.await {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(e.to_string()),
        Err(e) => Some(format!("worker task failed: {e}")),
    };
    if !terminal {
        // A peer that closed cleanly without a terminal status still leaves a
        // job hanging; do not show a spinner nobody will resolve.
        let open = state
            .db
            .get_job(job_id)
            .ok()
            .flatten()
            .map_or(false, |job| !job.status.is_terminal());
        let why = ended.or_else(|| open.then(|| "peer ended the stream without a final status".to_string()));
        if let Some(why) = why {
            if !spoke {
                return Outcome::Nothing(why);
            }
            fail(app, state, job_id, &why);
        }
    }

    // After the stream: a real worker has already invoiced (and
    // pay_invoice marked the job paid). Mock peers never invoice, so
    // this is what bills them.
    match crate::pot::settle_job(state, job_id, last_meta.as_ref()).await {
        Ok(Some(tx)) => log::info!("pot settled {tx}"),
        Ok(None) => {}
        Err(e) => log::warn!("pot settle: {e}"),
    }
    Outcome::Settled
}

/// What persisting one worker message changed.
#[derive(Debug, Default)]
pub struct Applied {
    pub terminal: bool,
    pub result: Option<crate::store::ResultRecord>,
    /// The reply, when this job belonged to a conversation.
    pub message: Option<crate::store::Message>,
    /// Live tokens. Not persisted — the final result is.
    pub delta: Option<JobDelta>,
}

/// Persist one worker message. Pure with respect to Tauri — the emitting half
/// lives in [`handle_message`] — so the whole pipeline is testable headless.
pub fn apply_message(state: &AppState, job_id: Uuid, msg: WorkerMessage) -> Result<Applied> {
    match msg {
        WorkerMessage::JobStatus(s) => {
            if s.job_id != job_id {
                return Ok(Applied::default());
            }
            state
                .db
                .update_job_status(job_id, s.status, s.progress, s.error.as_deref())?;
            Ok(Applied {
                terminal: s.status.is_terminal(),
                result: None,
                message: None,
                delta: None,
            })
        }
        WorkerMessage::JobInvoice(_) => Ok(Applied::default()),
        WorkerMessage::JobResult(r) => {
            if r.job_id != job_id {
                return Ok(Applied::default());
            }
            if let Some(expected) = crate::pot::expected_sha256(job_id) {
                if !expected.is_empty() && !expected.eq_ignore_ascii_case(&r.sha256) {
                    return Err(AppError::Invalid(
                        "result hash does not match the invoice this job paid".into(),
                    ));
                }
            }
            let thinking = r.thinking.clone();
            // Only a worker on this machine (the in-process mock) may hand back
            // a filesystem path; a remote peer must send bytes, or it could name
            // the identity/wallet key and have the client read it.
            let local = state
                .db
                .get_job(job_id)
                .ok()
                .flatten()
                .and_then(|job| state.db.get_peer(&job.peer_id).ok().flatten())
                .map(|peer| peer.is_mock())
                .unwrap_or(false);
            let record = results::materialize(&r, &state.download_dir(), local)?;
            state.db.insert_result(&record)?;
            state
                .db
                .update_job_status(job_id, JobStatus::Done, 1.0, None)?;
            let message = file_reply(state, job_id, &record, thinking.as_deref())?;
            Ok(Applied {
                terminal: true,
                result: Some(record),
                message,
                delta: None,
            })
        }
        WorkerMessage::JobDelta(d) => {
            if d.job_id != job_id || d.is_empty() {
                return Ok(Applied::default());
            }
            Ok(Applied {
                terminal: false,
                result: None,
                message: None,
                delta: Some(d),
            })
        }
        // Announces arriving mid-job refresh capability badges; ignore the rest.
        WorkerMessage::PeerAnnounce(_) | WorkerMessage::Unknown => Ok(Applied::default()),
    }
}

/// Write a finished text reply into the conversation that asked for it.
///
/// This belongs here rather than in the chat screen. A screen only exists
/// while it is on screen: navigate away mid-generation and the component that
/// was going to save the answer is gone, so the job succeeds, the result is
/// on disk, and the conversation never shows it. Filing it in the pipeline
/// means the answer lands whatever the window is doing.
fn file_reply(
    state: &AppState,
    job_id: Uuid,
    result: &crate::store::ResultRecord,
    thinking: Option<&str>,
) -> Result<Option<crate::store::Message>> {
    let Some(job) = state.db.get_job(job_id)? else {
        return Ok(None);
    };
    let Some(conversation_id) = job.conversation_id.as_deref() else {
        return Ok(None);
    };

    // A picture has no text. The message still gets filed, carrying the job id
    // and hash so the screen can find the image — otherwise an image job would
    // leave no trace in the conversation that asked for it.
    let text = result.text.as_deref().unwrap_or("");

    let meta = &result.meta;
    let tokens = meta
        .get("total_tokens")
        .and_then(|v| v.as_u64())
        .or_else(|| meta.get("completion_tokens").and_then(|v| v.as_u64()));

    // A paid invoice is the bill. A priced job not yet billed stays None here
    // and is filled in by `settle_job` once the charge is computed; a free
    // provider never records a bill at all.
    let cost = crate::pot::job_cost_micros(job_id);

    let message = state.db.add_message(
        conversation_id,
        "assistant",
        text,
        Some(&job_id.to_string()),
        Some(&result.sha256),
        meta.get("model")
            .and_then(|v| v.as_str())
            .or(Some(&job.model)),
        Some(&job.peer_label),
        tokens,
        cost,
        thinking,
    )?;
    Ok(Some(message))
}

/// Returns `true` when the job reached a terminal state.
fn handle_message(
    app: &AppHandle,
    state: &AppState,
    job_id: Uuid,
    msg: WorkerMessage,
) -> Result<bool> {
    let applied = apply_message(state, job_id, msg)?;
    if let Some(record) = &applied.result {
        let _ = app.emit(EVENT_JOB_RESULT, record);
    }
    if let Some(message) = &applied.message {
        let _ = app.emit(EVENT_MESSAGE_NEW, message);
    }
    if let Some(delta) = &applied.delta {
        let _ = app.emit(EVENT_JOB_DELTA, delta);
    }
    emit_job(app, state, job_id);
    Ok(applied.terminal)
}

fn fail(app: &AppHandle, state: &AppState, job_id: Uuid, error: &str) {
    // The bond may already be with the worker; whether it keeps the chunk
    // is for the chain to say, so the job is abandoned, not forgotten.
    crate::pot::abandon_job(state, job_id);
    let _ = state
        .db
        .update_job_status(job_id, JobStatus::Failed, 0.0, Some(error));
    emit_job(app, state, job_id);
}

fn emit_job(app: &AppHandle, state: &AppState, job_id: Uuid) {
    if let Ok(Some(job)) = state.db.get_job(job_id) {
        let _ = app.emit(EVENT_JOB_UPDATE, &job);
    }
}

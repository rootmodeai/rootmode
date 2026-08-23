//! Job lifecycle: submit → stream worker messages → persist → notify the UI.
//!
//! Submission returns as soon as the job row exists. Everything after that
//! happens on a tokio task and reaches the frontend as events, so no job ever
//! blocks the UI.

use std::sync::Arc;

use rootmode_core::{
    protocol::ClientMessage, JobDelta, JobPayload, JobStatus, JobSubmit, WorkerMessage,
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

    let transport = transport_for(&state, &peer).await?;

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

    let mut submit_msg = JobSubmit::new(record.job_id, state.identity().peer_id(), payload.clone());
    if let Some(price) = peer
        .models
        .iter()
        .find(|m| m.id == record.model || record.model.starts_with(&m.id))
        .and_then(|m| m.price.as_ref())
        .filter(|p| p.amount > 0.0)
    {
        if let Ok(st) = crate::pot::status(&state).await {
            if let (Some(client), Some(cfg)) = (st.client.as_deref(), crate::pot::load_chain_config(&state)) {
                match crate::pot::issue_ticket(
                    &state,
                    record.job_id,
                    price.clone(),
                    kind,
                    &payload,
                    client,
                    &cfg.worker,
                )
                .await
                {
                    Ok(bond) => {
                        submit_msg.payer = Some(client.to_string());
                        submit_msg.bond = Some(bond);
                    }
                    Err(e) => {
                        let msg = format!("could not lock funds for this job: {e}");
                        fail(app, &state, record.job_id, &msg);
                        return Err(AppError::Invalid(msg));
                    }
                }
            }
        }
    }
    let job_id = record.job_id;
    let app = app.clone();
    // Registered before the job starts, so a Stop click that arrives in the
    // instant between "queued" and the first real message still has
    // something to notify.
    let (stop, _running) = state.track_job(job_id);

    tauri::async_runtime::spawn(async move {
        let _running = _running;
        let (tx, mut rx) = mpsc::unbounded_channel::<WorkerMessage>();
        let (pay_tx, pay_rx) = mpsc::unbounded_channel::<ClientMessage>();
        let runner = {
            let transport = transport.clone();
            tauri::async_runtime::spawn(async move {
                transport.run_job(submit_msg, tx, stop, pay_rx).await
            })
        };

        let mut terminal = false;
        let mut last_meta: Option<serde_json::Value> = None;
        while let Some(msg) = rx.recv().await {
            if let WorkerMessage::JobResult(r) = &msg {
                last_meta = Some(r.meta.clone());
            }
            if let WorkerMessage::JobInvoice(inv) = &msg {
                match crate::pot::pay_invoice(&state, job_id, inv).await {
                    Ok(pay) => {
                        if pay_tx.send(ClientMessage::JobPay(pay)).is_err() {
                            fail(&app, &state, job_id, "could not send payment to the worker");
                            terminal = true;
                        }
                    }
                    Err(e) => {
                        fail(&app, &state, job_id, &e.to_string());
                        terminal = true;
                    }
                }
            }
            match handle_message(&app, &state, job_id, msg) {
                Ok(is_terminal) => terminal |= is_terminal,
                Err(e) => {
                    fail(&app, &state, job_id, &e.to_string());
                    terminal = true;
                }
            }
        }

        // After the stream: a real worker has already invoiced (and
        // pay_invoice marked the job paid). Mock peers never invoice, so
        // this is what bills them.
        match crate::pot::settle_job(&state, job_id, last_meta.as_ref()).await {
            Ok(Some(tx)) => log::info!("pot settled {tx}"),
            Ok(None) => {}
            Err(e) => log::warn!("pot settle: {e}"),
        }

        match runner.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) if !terminal => fail(&app, &state, job_id, &e.to_string()),
            Ok(Err(_)) => {}
            Err(e) if !terminal => fail(&app, &state, job_id, &format!("worker task failed: {e}")),
            Err(_) => {}
        }

        // A peer that closed cleanly without a terminal status still leaves a
        // job hanging; do not show a spinner nobody will resolve.
        if !terminal {
            if let Ok(Some(job)) = state.db.get_job(job_id) {
                if !job.status.is_terminal() {
                    fail(
                        &app,
                        &state,
                        job_id,
                        "peer ended the stream without a final status",
                    );
                }
            }
        }
    });

    Ok(record)
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
    crate::pot::drop_job(job_id);
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

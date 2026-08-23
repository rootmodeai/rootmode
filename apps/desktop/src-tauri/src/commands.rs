//! The frontend's entire surface. No shell, no filesystem, no arbitrary
//! fetch — the UI can only do the things named here.

use std::sync::Arc;

use base64::Engine;
use rootmode_core::{identity::PublicIdentity, JobKind, JobPayload};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use uuid::Uuid;

use crate::error::{AppError, Result};
use crate::gateway::{Gateway, GatewayStatus};
use crate::jobs::{self, EVENT_PEER_UPDATE};
use crate::net::{normalize_endpoint, Transport};
use crate::routing::ModelOption;
use crate::state::{AppState, Settings};
use crate::store::{Conversation, JobRecord, Message, ModelUsage, Peer, ResultRecord};

type St<'a> = State<'a, Arc<AppState>>;

/// Documents were dropped on the window and read.
pub const EVENT_FILES_DROPPED: &str = "files:dropped";

// --------------------------------------------------------------- identity

#[tauri::command]
pub fn get_identity(state: St<'_>) -> PublicIdentity {
    state.identity().public()
}

/// The UI gates this behind an explicit warning. Whoever holds this string is
/// this peer.
#[tauri::command]
pub fn export_identity_secret(state: St<'_>) -> String {
    state.identity().export_secret_hex()
}

#[tauri::command]
pub fn import_identity(state: St<'_>, secret_hex: String) -> Result<PublicIdentity> {
    state.import_identity(&secret_hex)?;
    Ok(state.identity().public())
}

#[tauri::command]
pub fn regenerate_identity(state: St<'_>) -> Result<PublicIdentity> {
    state.regenerate_identity()?;
    Ok(state.identity().public())
}

// ------------------------------------------------------------------ peers

#[tauri::command]
pub fn list_peers(state: St<'_>) -> Result<Vec<Peer>> {
    state.db.list_peers()
}

#[tauri::command]
pub fn add_peer(
    state: St<'_>,
    label: String,
    endpoint: String,
    public_key: Option<String>,
) -> Result<Peer> {
    let endpoint = normalize_endpoint(&endpoint)?;
    let label = {
        let l = label.trim();
        if l.is_empty() {
            endpoint.clone()
        } else {
            l.to_string()
        }
    };
    let public_key = public_key
        .map(|k| k.trim().to_lowercase())
        .filter(|k| !k.is_empty());
    if let Some(k) = &public_key {
        if k.len() != 64 || hex::decode(k).is_err() {
            return Err(AppError::Invalid(
                "peer public key must be 64 hex characters (32-byte ed25519)".into(),
            ));
        }
    }
    state.db.add_peer(&label, &endpoint, public_key.as_deref())
}

#[tauri::command]
pub fn remove_peer(state: St<'_>, id: String) -> Result<()> {
    state.db.remove_peer(&id)
}

/// Connect, say hello, record whatever came back. Success or a clear error.
#[tauri::command]
pub async fn probe_peer(app: AppHandle, id: String) -> Result<Peer> {
    let state = app.state::<Arc<AppState>>().inner().clone();
    let peer = state
        .db
        .get_peer(&id)?
        .ok_or_else(|| AppError::NotFound(format!("peer {id}")))?;

    let transport = jobs::transport_for(&state, &peer).await?;
    match transport.probe().await {
        Ok(probe) => {
            let a = probe.announce;
            // Adopt the name it gives, unless the label is one the user typed.
            if let Some(label) = a.as_ref().and_then(|a| a.label.as_deref()) {
                let anonymous = peer.label == peer.endpoint || peer.is_discovered();
                if anonymous && !label.trim().is_empty() {
                    let _ = state.db.rename_peer(&peer.id, label.trim());
                }
            }
            state.db.update_peer_status(
                &id,
                "online",
                Some(probe.latency_ms),
                a.as_ref().map(|a| a.peer_id.as_str()),
                a.as_ref().map(|a| a.caps.as_slice()),
                a.as_ref().map(|a| a.models.as_slice()),
                a.as_ref().map(|a| a.max_concurrent),
                a.as_ref().and_then(|a| a.country.as_deref()),
                None,
            )?;
        }
        Err(e) => {
            let msg = e.to_string();
            let status = if msg.contains("key mismatch") {
                "mismatch"
            } else {
                "offline"
            };
            state
                .db
                .update_peer_status(&id, status, None, None, None, None, None, None, Some(&msg))?;
        }
    }

    let updated = state
        .db
        .get_peer(&id)?
        .ok_or_else(|| AppError::NotFound(format!("peer {id}")))?;
    let _ = app.emit(EVENT_PEER_UPDATE, &updated);
    Ok(updated)
}

#[tauri::command]
pub async fn probe_all_peers(app: AppHandle) -> Result<Vec<Peer>> {
    let state = app.state::<Arc<AppState>>().inner().clone();
    let ids: Vec<String> = state.db.list_peers()?.into_iter().map(|p| p.id).collect();
    for id in ids {
        // Sequential on purpose: a handful of peers, and one slow endpoint
        // should not hide the others behind a burst of sockets.
        let _ = probe_peer(app.clone(), id).await;
    }
    state.db.list_peers()
}

/// Ask the network who is serving anything, and fold the answers into the
/// peers list. Discovered peers are probed so their capabilities and models
/// are real rather than claimed.
pub async fn refresh_discovered(app: &AppHandle) -> Result<Vec<Peer>> {
    let state = app.state::<Arc<AppState>>().inner().clone();
    let node = state.p2p_node().await?;
    let me = state.identity().peer_id();

    for peer in crate::p2p::discover(&node).await {
        let Some(hex) = rootmode_p2p::peer_id_to_hex(&peer) else {
            continue;
        };
        if hex == me {
            continue;
        }
        let endpoint = crate::p2p::p2p_endpoint(&hex);

        // Already known: refresh it in place, error and all.
        if let Some(existing) = state.db.peer_by_endpoint(&endpoint)? {
            let _ = probe_peer(app.clone(), existing.id).await;
            continue;
        }

        // New: ask before recording it. A network is full of nodes that are
        // not workers — other people's clients, for a start — and a peers list
        // that fills up with things that never answer is worse than an empty
        // one.
        let transport = crate::p2p::Libp2pTransport::new(
            node.clone(),
            peer,
            state.identity(),
            None,
            state.sign_jobs(),
        );
        let announce = match transport.probe().await {
            Ok(probe) => match probe.announce {
                Some(announce) if !announce.caps.is_empty() => announce,
                // Answered but serves nothing: not a worker.
                _ => continue,
            },
            Err(e) => {
                log::debug!("ignoring {hex}: {e}");
                continue;
            }
        };

        // What it calls itself, if it says. A key fingerprint is a terrible
        // name for something you are about to ask a question.
        let label = announce
            .label
            .as_deref()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| l.chars().take(40).collect::<String>())
            .unwrap_or_else(|| format!("{}…{}", &hex[..8], &hex[hex.len() - 4..]));
        let row = state.db.upsert_discovered_peer(&label, &endpoint)?;
        state.db.update_peer_status(
            &row.id,
            "online",
            None,
            Some(&announce.peer_id),
            Some(&announce.caps),
            Some(&announce.models),
            Some(announce.max_concurrent),
            announce.country.as_deref(),
            None,
        )?;
        if let Ok(Some(updated)) = state.db.get_peer(&row.id) {
            log::info!("added {} ({})", updated.label, updated.caps.join(", "));
            let _ = app.emit(EVENT_PEER_UPDATE, &updated);
        }
    }

    // Forget anything that has been unreachable for a while. Workers that are
    // recreated get a new identity, and without this the list only ever grows.
    match state
        .db
        .prune_dead_discovered(std::time::Duration::from_secs(10 * 60))
    {
        Ok(n) if n > 0 => log::info!("forgot {n} provider(s) that stopped answering"),
        Err(e) => log::debug!("prune: {e}"),
        _ => {}
    }

    state.db.list_peers()
}

#[tauri::command]
pub async fn discover_peers(app: AppHandle) -> Result<Vec<Peer>> {
    refresh_discovered(&app).await
}

// ------------------------------------------------------------------- jobs

/// Run a job. When it answers a chat, name the conversation: the reply is
/// then filed by the job pipeline whatever screen the user is looking at.
#[tauri::command]
pub async fn submit_job(
    app: AppHandle,
    peer_id: String,
    payload: JobPayload,
    conversation_id: Option<String>,
) -> Result<JobRecord> {
    jobs::submit(&app, &peer_id, payload, conversation_id).await
}

/// Ask a running job to stop. A no-op if it already finished — pressing Stop
/// on a job that just landed is a race the user wins either way.
#[tauri::command]
pub fn stop_job(state: St, job_id: String) -> Result<()> {
    let job_id: Uuid = job_id
        .parse()
        .map_err(|_| AppError::Invalid("not a job id".into()))?;
    state.stop_job(job_id);
    Ok(())
}

#[tauri::command]
pub fn list_jobs(state: St<'_>, limit: Option<u32>) -> Result<Vec<JobRecord>> {
    state.db.list_jobs(limit.unwrap_or(200).min(1000))
}

#[tauri::command]
pub fn get_job(state: St<'_>, job_id: Uuid) -> Result<Option<JobRecord>> {
    state.db.get_job(job_id)
}

// ---------------------------------------------------------------- results

#[tauri::command]
pub fn get_result(state: St<'_>, job_id: Uuid) -> Result<Option<ResultRecord>> {
    state.db.get_result(job_id)
}

#[tauri::command]
pub fn list_results(
    state: St<'_>,
    kind: Option<JobKind>,
    limit: Option<u32>,
) -> Result<Vec<ResultRecord>> {
    state.db.list_results(kind, limit.unwrap_or(100).min(1000))
}

/// Read an image result back as a data URL.
///
/// Only paths recorded in the results table are readable — the frontend cannot
/// name an arbitrary file.
/// The raw base64 of a result, for sending back as a starting point.
///
/// Separate from [`read_result_image`], which wraps the same bytes in a data
/// URL for an `<img>` tag. Sending that to a worker would make it strip a
/// prefix it should never have been given.
#[tauri::command]
pub fn read_result_bytes(state: St<'_>, job_id: Uuid) -> Result<String> {
    let record = state
        .db
        .get_result(job_id)?
        .ok_or_else(|| AppError::NotFound(format!("result for job {job_id}")))?;
    let path = record
        .image_path
        .ok_or_else(|| AppError::Invalid("this result is not an image".into()))?;
    let bytes =
        std::fs::read(&path).map_err(|e| AppError::Invalid(format!("cannot read {path}: {e}")))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

#[tauri::command]
pub fn read_result_image(state: St<'_>, job_id: Uuid) -> Result<String> {
    let record = state
        .db
        .get_result(job_id)?
        .ok_or_else(|| AppError::NotFound(format!("result for job {job_id}")))?;
    let path = record
        .image_path
        .ok_or_else(|| AppError::Invalid("this result is not an image".into()))?;
    let bytes =
        std::fs::read(&path).map_err(|e| AppError::Invalid(format!("cannot read {path}: {e}")))?;
    let mime = match std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
    {
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mov") => "video/quicktime",
        _ => "image/png",
    };
    Ok(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

/// Show a result file in the OS file manager. Same restriction as above.
#[tauri::command]
pub fn reveal_result(app: AppHandle, state: St<'_>, job_id: Uuid) -> Result<()> {
    use tauri_plugin_opener::OpenerExt;
    let path = state
        .db
        .get_result(job_id)?
        .and_then(|r| r.image_path)
        .ok_or_else(|| AppError::NotFound("no file for this result".into()))?;
    app.opener()
        .reveal_item_in_dir(&path)
        .map_err(|e| AppError::Invalid(e.to_string()))
}

// ---------------------------------------------------------- conversations

#[tauri::command]
pub fn list_conversations(state: St<'_>, kind: Option<String>) -> Result<Vec<Conversation>> {
    state.db.list_conversations(kind.as_deref(), 200)
}

#[tauri::command]
pub fn conversation_messages(state: St<'_>, id: String) -> Result<Vec<Message>> {
    state.db.conversation_messages(&id)
}

/// Start a chat. The title is the first thing said in it, trimmed — renaming
/// is there for when that guess is wrong.
#[tauri::command]
pub fn new_conversation(state: St<'_>, title: String, kind: String) -> Result<Conversation> {
    let title = title.trim();
    let title = if title.is_empty() {
        "New chat".to_string()
    } else {
        title.chars().take(60).collect()
    };
    let kind = if kind == "image" { "image" } else { "llm" };
    state.db.create_conversation(&title, kind)
}

#[tauri::command]
pub fn rename_conversation(state: St<'_>, id: String, title: String) -> Result<()> {
    let title = title.trim();
    if title.is_empty() {
        return Err(AppError::Invalid("a chat needs a name".into()));
    }
    state
        .db
        .rename_conversation(&id, &title.chars().take(60).collect::<String>())
}

#[tauri::command]
pub fn delete_conversation(state: St<'_>, id: String) -> Result<()> {
    // Anything this conversation drew goes with it. A chat you deleted that
    // left its pictures on disk has not been deleted in any sense the user
    // would recognise.
    for job_id in state.db.conversation_job_ids(&id)? {
        if let Err(e) = erase_result(&state, job_id) {
            log::warn!("could not remove a result from {id}: {e}");
        }
    }
    state.db.delete_conversation(&id)
}

/// Empty the history: every chat, every picture and video they made, and the
/// jobs and results behind them.
///
/// Bytes first, rows second, for the same reason as [`delete_result`]. A file
/// that refuses to go is logged and stepped over rather than aborting the
/// wipe: the alternative is a half-deleted history that the user has to ask
/// to delete again, and they have already said what they want.
#[tauri::command]
pub fn delete_all_conversations(state: St<'_>) -> Result<()> {
    for path in state.db.all_result_paths()? {
        if let Err(e) = crate::erase::remove(std::path::Path::new(&path)) {
            log::warn!("could not remove {path}: {e}");
        }
    }
    state.db.clear_history()
}

/// Forget one result: the bytes on disk first, then the rows.
///
/// That order matters. Rows first would leave a file nothing points at — a
/// picture the user believes is gone, invisible to the app, still on disk.
#[tauri::command]
pub fn delete_result(state: St<'_>, job_id: Uuid) -> Result<()> {
    erase_result(&state, job_id)
}

fn erase_result(state: &Arc<AppState>, job_id: Uuid) -> Result<()> {
    if let Some(result) = state.db.get_result(job_id)? {
        if let Some(path) = result.image_path.as_deref() {
            crate::erase::remove(std::path::Path::new(path))?;
        }
    }
    state.db.delete_result(job_id)
}

#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub fn add_message(
    state: St<'_>,
    conversation_id: String,
    role: String,
    content: String,
    job_id: Option<String>,
    sha256: Option<String>,
    model: Option<String>,
    peer: Option<String>,
    tokens: Option<u64>,
    thinking: Option<String>,
) -> Result<Message> {
    state.db.add_message(
        &conversation_id,
        &role,
        &content,
        job_id.as_deref(),
        sha256.as_deref(),
        model.as_deref(),
        peer.as_deref(),
        tokens,
        thinking.as_deref(),
    )
}

// --------------------------------------------------------------- settings

#[tauri::command]
pub fn get_settings(state: St<'_>) -> Result<Settings> {
    state.settings()
}

#[tauri::command]
pub async fn set_setting(app: AppHandle, key: String, value: String) -> Result<Settings> {
    let state = app.state::<Arc<AppState>>().inner().clone();
    state.set_setting(&key, &value)?;
    // The node is built from these; rebuild it next time it is needed.
    if key == crate::state::SETTING_BOOTSTRAP || key == crate::state::SETTING_DISCOVERY {
        state.reset_p2p().await;
    }
    // Turning the local endpoint on or moving it should take effect now, not
    // at the next restart.
    if key == crate::gateway::SETTING_GATEWAY || key == crate::gateway::SETTING_GATEWAY_PORT {
        let gateway = app.state::<Arc<Gateway>>().inner().clone();
        crate::gateway::reconcile(gateway, state.clone()).await;
    }
    state.settings()
}

// -------------------------------------------------------- local endpoint

/// Where other programs on this machine should point, and with what token.
#[tauri::command]
pub async fn gateway_status(app: AppHandle) -> Result<GatewayStatus> {
    let state = app.state::<Arc<AppState>>().inner().clone();
    let gateway = app.state::<Arc<Gateway>>().inner().clone();
    Ok(gateway.status(&state).await)
}

/// Invalidate the old token and issue a new one. Anything still holding the
/// old one stops working, which is the point.
#[tauri::command]
pub async fn rotate_gateway_token(app: AppHandle) -> Result<GatewayStatus> {
    let state = app.state::<Arc<AppState>>().inner().clone();
    let gateway = app.state::<Arc<Gateway>>().inner().clone();
    crate::gateway::rotate_token(&state)?;
    Ok(gateway.status(&state).await)
}

// ------------------------------------------------------------ connected apps

/// The catalog of coding-agent CLIs and editors this app can point at its own
/// local endpoint, and whether each is installed and currently connected.
#[tauri::command]
pub async fn list_connected_tools(app: AppHandle) -> Result<Vec<crate::connected_tools::ToolStatus>> {
    let state = app.state::<Arc<AppState>>().inner().clone();
    Ok(crate::connected_tools::list(&state))
}

/// Turns the local endpoint on if it is not already, then patches the given
/// tool's own config file to use it.
#[tauri::command]
pub async fn connect_tool(app: AppHandle, key: String) -> Result<crate::connected_tools::ToolStatus> {
    let state = app.state::<Arc<AppState>>().inner().clone();
    if !matches!(state.db.get_setting(crate::gateway::SETTING_GATEWAY)?.as_deref(), Some("true")) {
        state.set_setting(crate::gateway::SETTING_GATEWAY, "true")?;
        let gateway = app.state::<Arc<Gateway>>().inner().clone();
        crate::gateway::reconcile(gateway.clone(), state.clone()).await;
    }
    let gateway = app.state::<Arc<Gateway>>().inner().clone();
    let status = gateway.status(&state).await;
    let endpoint = crate::connected_tools::endpoint_from_status(&status, None);
    crate::connected_tools::connect(&state, &key, &endpoint)
}

/// Removes just what a connect added from the tool's config file.
#[tauri::command]
pub async fn disconnect_tool(app: AppHandle, key: String) -> Result<crate::connected_tools::ToolStatus> {
    let state = app.state::<Arc<AppState>>().inner().clone();
    crate::connected_tools::disconnect(&state, &key)
}

#[tauri::command]
pub async fn pot_status(app: AppHandle) -> Result<crate::pot::PotStatus> {
    let state = app.state::<Arc<AppState>>().inner().clone();
    crate::pot::status(&state).await
}

#[tauri::command]
pub async fn pot_check(
    app: AppHandle,
    price: f64,
    unpriced: bool,
    kind: JobKind,
) -> Result<crate::pot::PotCheck> {
    let state = app.state::<Arc<AppState>>().inner().clone();
    crate::pot::check(&state, price, unpriced, kind).await
}

#[tauri::command]
pub async fn pot_open_fund(app: AppHandle) -> Result<String> {
    crate::pot::open_fund(&app).await
}

#[tauri::command]
pub async fn pot_deposits(app: AppHandle) -> Result<Vec<crate::pot::Deposit>> {
    let state = app.state::<Arc<AppState>>().inner().clone();
    crate::pot::deposits(&state).await
}

#[tauri::command]
pub fn token_usage(state: St<'_>) -> Result<Vec<ModelUsage>> {
    state.db.token_usage()
}

/// Opens a terminal already running the tool, so a connect can be seen
/// working immediately.
#[tauri::command]
pub async fn launch_tool(app: AppHandle, key: String) -> Result<()> {
    let state = app.state::<Arc<AppState>>().inner().clone();
    let gateway = app.state::<Arc<Gateway>>().inner().clone();
    let status = gateway.status(&state).await;
    let endpoint = crate::connected_tools::endpoint_from_status(&status, None);
    crate::connected_tools::launch(&key, &endpoint)
}

/// What the UI needs to answer "can I do anything right now?" without the
/// user knowing what a peer is.
#[derive(Debug, Serialize)]
pub struct NetworkStatus {
    /// Distinct peers online. A peer that does both text and images is one
    /// provider, not two.
    pub online: u32,
    /// Peers that answered and can run text jobs.
    pub llm_peers: u32,
    /// Peers that answered and can generate images.
    pub image_peers: u32,
    /// Peers that answered and can generate video.
    pub video_peers: u32,
    /// Distinct model names available across all of them.
    pub models: Vec<String>,
    pub image_models: Vec<String>,
    pub video_models: Vec<String>,
    pub searching: bool,
}

/// Every (model, provider) pair on offer, cheapest first — the list a person
/// picks from by hand.
#[tauri::command]
pub fn available_providers(
    state: St<'_>,
    kind: Option<JobKind>,
) -> Result<Vec<crate::routing::ProviderOption>> {
    let peers = state.db.list_peers()?;
    Ok(crate::routing::provider_options(
        &peers,
        kind.unwrap_or(JobKind::Llm),
    ))
}

/// What you can ask for, and who would serve it. The UI offers models; this
/// decides the provider.
#[tauri::command]
pub fn available_models(state: St<'_>, kind: Option<JobKind>) -> Result<Vec<ModelOption>> {
    let peers = state.db.list_peers()?;
    Ok(crate::routing::model_options(
        &peers,
        kind.unwrap_or(JobKind::Llm),
    ))
}

#[tauri::command]
pub fn network_status(state: St<'_>) -> Result<NetworkStatus> {
    let peers = state.db.list_peers()?;
    let online: Vec<_> = peers.iter().filter(|p| p.status == "online").collect();

    let mut models: Vec<String> = Vec::new();
    let mut image_models: Vec<String> = Vec::new();
    let mut video_models: Vec<String> = Vec::new();
    for peer in &online {
        for model in &peer.models {
            let list = match model.kind {
                JobKind::Llm => &mut models,
                JobKind::Image => &mut image_models,
                JobKind::Video => &mut video_models,
            };
            if !list.contains(&model.id) {
                list.push(model.id.clone());
            }
        }
    }

    Ok(NetworkStatus {
        online: online.len() as u32,
        llm_peers: online
            .iter()
            .filter(|p| p.caps.iter().any(|c| c == "llm"))
            .count() as u32,
        image_peers: online
            .iter()
            .filter(|p| p.caps.iter().any(|c| c == "image"))
            .count() as u32,
        video_peers: online
            .iter()
            .filter(|p| p.caps.iter().any(|c| c == "video"))
            .count() as u32,
        models,
        image_models,
        video_models,
        searching: state.discovery_enabled(),
    })
}

#[derive(Debug, Serialize)]
pub struct DashboardStats {
    pub peers: u32,
    pub peers_online: u32,
    pub open_jobs: u32,
    pub results: u32,
    pub peer_id: String,
    pub protocol_version: u32,
    pub discovery: bool,
}

#[tauri::command]
pub fn dashboard_stats(state: St<'_>) -> Result<DashboardStats> {
    let (peers, open_jobs, results) = state.db.counts()?;
    let peers_online = state
        .db
        .list_peers()?
        .iter()
        .filter(|p| p.status == "online" && !p.is_mock())
        .count() as u32;
    Ok(DashboardStats {
        peers,
        peers_online,
        open_jobs,
        results,
        peer_id: state.identity().peer_id(),
        protocol_version: rootmode_core::PROTOCOL_VERSION,
        discovery: state.discovery_enabled(),
    })
}

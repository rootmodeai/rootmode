//! Shared application state and settings.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use uuid::Uuid;

use rootmode_core::Identity;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, Result};
use crate::identity_store;
use crate::store::Db;

pub const SETTING_DOWNLOAD_DIR: &str = "download_dir";
pub const SETTING_DEFAULT_PEER: &str = "default_peer";
pub const SETTING_DEFAULT_LLM: &str = "default_llm_model";
pub const SETTING_DEFAULT_IMAGE: &str = "default_image_model";
pub const SETTING_THEME: &str = "theme";
pub const SETTING_SIGN_JOBS: &str = "sign_jobs";
pub const SETTING_BOOTSTRAP: &str = "bootstrap";
pub const SETTING_DISCOVERY: &str = "discovery";
pub const SETTING_MOCK_WORKER: &str = "mock_worker";
pub use crate::gateway::{
    SETTING_GATEWAY, SETTING_GATEWAY_MODEL, SETTING_GATEWAY_PORT, SETTING_GATEWAY_SUBSTITUTE,
};

pub struct AppState {
    pub db: Arc<Db>,
    identity: RwLock<Identity>,
    pub app_data: PathBuf,
    pub default_download_dir: PathBuf,
    /// The libp2p node, started the first time discovery is needed. `None`
    /// until then, so a client with no bootstrap address opens no sockets.
    p2p: tokio::sync::Mutex<Option<rootmode_p2p::Node>>,
    /// One entry per job actually in flight — inserted when the transport
    /// task starts, removed the moment it ends, however it ends. Lets a Stop
    /// button reach a job it has no other handle on: the socket, the
    /// tokio task, all of it live inside a spawned future the UI never
    /// sees.
    running: RwLock<HashMap<Uuid, (Arc<tokio::sync::Notify>, Arc<AtomicBool>)>>,
}

impl AppState {
    pub fn new(app_data: PathBuf, default_download_dir: PathBuf) -> Result<Self> {
        let db = Arc::new(Db::open(&app_data.join("rootmode.sqlite"))?);
        let identity = identity_store::load_or_create(&app_data)?;
        Ok(Self {
            db,
            identity: RwLock::new(identity),
            app_data,
            default_download_dir,
            p2p: tokio::sync::Mutex::new(None),
            running: RwLock::new(HashMap::new()),
        })
    }

    /// Register a job as stoppable, for the life of this guard. Dropping it
    /// — on any return path of the task that holds it — is what keeps this
    /// registry from outliving the jobs it describes.
    ///
    /// The flag says whether Stop was ever pressed for this job — a
    /// `Notify` wakes whoever is waiting at the time and remembers nothing,
    /// and a job handed to a second provider needs to know.
    pub fn track_job(
        self: &Arc<Self>,
        job_id: Uuid,
    ) -> (Arc<tokio::sync::Notify>, Arc<AtomicBool>, RunningGuard) {
        let notify = Arc::new(tokio::sync::Notify::new());
        let asked = Arc::new(AtomicBool::new(false));
        self.running
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(job_id, (notify.clone(), asked.clone()));
        (notify, asked, RunningGuard { state: self.clone(), job_id })
    }

    /// Ask a running job to stop. A no-op if it already finished — the same
    /// harmless race the worker's own `job.cancel` handling accepts.
    pub fn stop_job(&self, job_id: Uuid) {
        if let Some((notify, asked)) = self.running.read().unwrap_or_else(|e| e.into_inner()).get(&job_id) {
            asked.store(true, Ordering::SeqCst);
            notify.notify_waiters();
        }
    }

    pub fn identity(&self) -> Identity {
        self.identity
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn import_identity(&self, secret_hex: &str) -> Result<()> {
        let new = identity_store::import(&self.app_data, secret_hex)?;
        *self.identity.write().unwrap_or_else(|e| e.into_inner()) = new;
        Ok(())
    }

    pub fn regenerate_identity(&self) -> Result<()> {
        let fresh = Identity::generate();
        self.import_identity(&fresh.export_secret_hex())
    }

    /// Where image results land. User-configurable; falls back to the OS
    /// download directory with a `rootmode` subfolder.
    pub fn download_dir(&self) -> PathBuf {
        self.db
            .get_setting(SETTING_DOWNLOAD_DIR)
            .ok()
            .flatten()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| self.default_download_dir.clone())
    }

    pub fn sign_jobs(&self) -> bool {
        !matches!(
            self.db
                .get_setting(SETTING_SIGN_JOBS)
                .ok()
                .flatten()
                .as_deref(),
            Some("false")
        )
    }

    /// Bootstrap addresses, one per line (blank lines and `#` comments
    /// ignored, so a list can be pasted with notes in it).
    ///
    /// Configuring none means the network's own entry points, not none at all
    /// — a fresh install should join without being told how.
    pub fn bootstrap_addrs(&self) -> Vec<String> {
        let configured: Vec<String> = self
            .db
            .get_setting(SETTING_BOOTSTRAP)
            .ok()
            .flatten()
            .unwrap_or_default()
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(str::to_string)
            .collect();

        if configured.is_empty() {
            rootmode_p2p::default_bootstrap()
        } else {
            configured
        }
    }

    /// On by default and with no configuration: peers on this network are
    /// found automatically. A bootstrap address only extends the search
    /// beyond the local network.
    pub fn discovery_enabled(&self) -> bool {
        !matches!(
            self.db
                .get_setting(SETTING_DISCOVERY)
                .ok()
                .flatten()
                .as_deref(),
            Some("false")
        )
    }

    /// The node, started on first use. Restart it with [`AppState::reset_p2p`]
    /// after the bootstrap list changes.
    pub async fn p2p_node(&self) -> Result<rootmode_p2p::Node> {
        let mut guard = self.p2p.lock().await;
        if let Some(node) = guard.as_ref() {
            return Ok(node.clone());
        }
        let node = crate::p2p::start(self.identity(), &self.bootstrap_addrs()).await?;
        *guard = Some(node.clone());
        Ok(node)
    }

    /// Drop the node so the next use rebuilds it from current settings.
    pub async fn reset_p2p(&self) {
        *self.p2p.lock().await = None;
    }

    pub fn settings(&self) -> Result<Settings> {
        Ok(Settings {
            download_dir: self.download_dir().to_string_lossy().into_owned(),
            default_peer: self.db.get_setting(SETTING_DEFAULT_PEER)?,
            default_llm_model: self.db.get_setting(SETTING_DEFAULT_LLM)?,
            default_image_model: self.db.get_setting(SETTING_DEFAULT_IMAGE)?,
            // Light unless asked otherwise.
            theme: self
                .db
                .get_setting(SETTING_THEME)?
                .unwrap_or_else(|| "light".into()),
            sign_jobs: self.sign_jobs(),
            bootstrap: self.db.get_setting(SETTING_BOOTSTRAP)?.unwrap_or_default(),
            discovery: self.discovery_enabled(),
            mock_worker: matches!(
                self.db.get_setting(SETTING_MOCK_WORKER)?.as_deref(),
                Some("true")
            ) || std::env::var("ROOTMODE_MOCK").is_ok(),
            entry_points: self.bootstrap_addrs().len() as u32,
            gateway: matches!(
                self.db.get_setting(SETTING_GATEWAY)?.as_deref(),
                Some("true")
            ),
            gateway_port: self
                .db
                .get_setting(SETTING_GATEWAY_PORT)?
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(crate::gateway::DEFAULT_PORT),
            app_data_dir: self.app_data.to_string_lossy().into_owned(),
            db_path: self.db.path().to_string_lossy().into_owned(),
            key_path: identity_store::key_path(&self.app_data)
                .to_string_lossy()
                .into_owned(),
        })
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        const ALLOWED: [&str; 13] = [
            SETTING_DOWNLOAD_DIR,
            SETTING_DEFAULT_PEER,
            SETTING_DEFAULT_LLM,
            SETTING_DEFAULT_IMAGE,
            SETTING_THEME,
            SETTING_SIGN_JOBS,
            SETTING_BOOTSTRAP,
            SETTING_DISCOVERY,
            SETTING_MOCK_WORKER,
            SETTING_GATEWAY,
            SETTING_GATEWAY_PORT,
            SETTING_GATEWAY_SUBSTITUTE,
            SETTING_GATEWAY_MODEL,
        ];
        if !ALLOWED.contains(&key) {
            return Err(AppError::Invalid(format!("unknown setting '{key}'")));
        }
        if key == SETTING_BOOTSTRAP {
            // Reject the whole list rather than silently ignoring a typo in
            // one line — a bootstrap address you think is set but is not is
            // worse than an error.
            for line in value
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
            {
                rootmode_p2p::parse_bootstrap(line)
                    .map_err(|e| AppError::Invalid(e.to_string()))?;
            }
        }
        if key == SETTING_GATEWAY_PORT && !value.trim().is_empty() {
            // Ports below 1024 need privileges nobody should be granting a
            // chat app, and a typo here is otherwise only discovered as a
            // silent failure to listen.
            match value.trim().parse::<u16>() {
                Ok(p) if p >= 1024 => {}
                _ => {
                    return Err(AppError::Invalid(
                        "port must be a number between 1024 and 65535".into(),
                    ))
                }
            }
        }
        if key == SETTING_DOWNLOAD_DIR && !value.trim().is_empty() {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                return Err(AppError::Invalid(
                    "download directory must be an absolute path".into(),
                ));
            }
            std::fs::create_dir_all(&path)?;
        }
        self.db.set_setting(key, value)?;
        if key == SETTING_MOCK_WORKER {
            if value == "true" {
                self.db.enable_mock_peer()?;
            } else {
                self.db.remove_mock_peer()?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub download_dir: String,
    pub default_peer: Option<String>,
    pub default_llm_model: Option<String>,
    pub default_image_model: Option<String>,
    pub theme: String,
    pub sign_jobs: bool,
    pub bootstrap: String,
    pub discovery: bool,
    /// In-process fake worker, for trying the pot without a GPU.
    pub mock_worker: bool,
    /// How many entry points are actually in use — configured ones, or the
    /// ones shipped with the build. Zero means nothing beyond this network
    /// can be found, which the UI says out loud rather than looking broken.
    pub entry_points: u32,
    /// Whether other programs on this machine may use the network through a
    /// local HTTP endpoint.
    pub gateway: bool,
    pub gateway_port: u16,
    pub app_data_dir: String,
    pub db_path: String,
    pub key_path: String,
}

/// Removes a job from [`AppState::running`] when the task that requested
/// tracking ends — success, failure, or a stop that already fired.
pub struct RunningGuard {
    state: Arc<AppState>,
    job_id: Uuid,
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.state
            .running
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.job_id);
    }
}

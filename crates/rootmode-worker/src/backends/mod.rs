//! Backends: the things that actually hold GPUs.
//!
//! A backend turns a validated [`JobPayload`] into a [`JobResult`]. It never
//! sees the raw wire message, never sees the submitting peer, and never
//! receives anything but typed parameters — a backend is the last place a
//! hostile prompt could do damage, so it gets the least context.

pub mod comfyui;
pub mod openrouter;
pub mod uiformat;
pub mod vllm;

use std::sync::Arc;

use async_trait::async_trait;
use rootmode_core::{JobKind, JobPayload, JobResult, ModelDescriptor};
use tokio::sync::mpsc::UnboundedSender;
use uuid::Uuid;

use crate::config::BackendConfig;
use crate::error::{Result, WorkerError};

/// Incremental tokens from a running generation.
#[derive(Debug, Clone, Default)]
pub struct TokenDelta {
    pub text: String,
    pub thinking: String,
}

/// Where a backend reports how far along it is, `0.0..=1.0`, and the tokens
/// it has just produced. Dropping updates is fine — the client tolerates gaps
/// and the final `job.result` is authoritative.
#[derive(Clone)]
pub struct Progress {
    progress: Option<UnboundedSender<f32>>,
    tokens: Option<UnboundedSender<TokenDelta>>,
}

impl Progress {
    pub fn new(tx: UnboundedSender<f32>) -> Self {
        Self {
            progress: Some(tx),
            tokens: None,
        }
    }

    pub fn with_tokens(mut self, tx: UnboundedSender<TokenDelta>) -> Self {
        self.tokens = Some(tx);
        self
    }

    /// A sink that goes nowhere, for tests and one-shot runs.
    pub fn none() -> Self {
        Self {
            progress: None,
            tokens: None,
        }
    }

    pub fn set(&self, value: f32) {
        if let Some(tx) = &self.progress {
            let _ = tx.send(value.clamp(0.0, 1.0));
        }
    }

    pub fn delta(&self, text: &str, thinking: &str) {
        if text.is_empty() && thinking.is_empty() {
            return;
        }
        if let Some(tx) = &self.tokens {
            let _ = tx.send(TokenDelta {
                text: text.to_string(),
                thinking: thinking.to_string(),
            });
        }
    }
}

#[async_trait]
pub trait Backend: Send + Sync {
    /// Short name for logs and errors, e.g. `vllm`.
    fn name(&self) -> &str;

    fn kind(&self) -> JobKind;

    /// What this backend advertises. Resolved once at startup.
    async fn discover_models(&self) -> Result<Vec<ModelDescriptor>>;

    /// Is the underlying server actually up? Returns a one-line description.
    async fn health(&self) -> Result<String>;

    async fn run(
        &self,
        job_id: Uuid,
        payload: &JobPayload,
        progress: &Progress,
    ) -> Result<JobResult>;
}

/// Every configured backend, plus the models they currently resolve to.
pub struct Registry {
    entries: Vec<Entry>,
}

struct Entry {
    backend: Arc<dyn Backend>,
    /// Re-read by [`Registry::refresh`], not fixed at boot: an operator who
    /// loads another model into vLLM, or drops a checkpoint into ComfyUI,
    /// should not have to restart the worker for anyone to be able to ask
    /// for it.
    models: std::sync::RwLock<Vec<ModelDescriptor>>,
}

/// Just the ids, for comparing one discovery against the next.
fn ids(models: &[ModelDescriptor]) -> Vec<String> {
    let mut out: Vec<String> = models.iter().map(|m| m.id.clone()).collect();
    out.sort();
    out
}

impl Entry {
    fn models(&self) -> Vec<ModelDescriptor> {
        self.models.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Registry {
    /// Build from config. Model discovery failures are logged, not fatal: a
    /// backend that is down at boot may be up by the time a job arrives, and
    /// refusing to start is worse than advertising nothing for it.
    pub async fn build(configs: &[BackendConfig]) -> Result<Self> {
        let mut entries = Vec::new();
        for config in configs {
            let backend: Arc<dyn Backend> = match config {
                BackendConfig::Vllm(c) => Arc::new(vllm::VllmBackend::new(c.clone())?),
                BackendConfig::Comfyui(c) => Arc::new(comfyui::ComfyBackend::new(c.clone()).await?),
                BackendConfig::Openrouter(c) => {
                    Arc::new(openrouter::OpenRouterBackend::new(c.clone())?)
                }
            };
            let models = match backend.discover_models().await {
                Ok(models) => models,
                Err(e) => {
                    tracing::warn!(
                        backend = backend.name(),
                        "could not list models ({e}); advertising none until it is reachable"
                    );
                    Vec::new()
                }
            };
            entries.push(Entry {
                backend,
                models: std::sync::RwLock::new(models),
            });
        }
        Ok(Self { entries })
    }

    pub fn from_backends(backends: Vec<(Arc<dyn Backend>, Vec<ModelDescriptor>)>) -> Self {
        Self {
            entries: backends
                .into_iter()
                .map(|(backend, models)| Entry {
                    backend,
                    models: std::sync::RwLock::new(models),
                })
                .collect(),
        }
    }

    /// Ask every backend what it has now. Returns true when anything changed,
    /// so the caller can re-announce rather than re-publishing the same list.
    ///
    /// A backend that has gone away keeps its last known models: a server
    /// restarting mid-poll should not empty the node's advertisement.
    pub async fn refresh(&self) -> bool {
        let mut changed = false;
        for entry in &self.entries {
            let models = match entry.backend.discover_models().await {
                Ok(models) => models,
                Err(e) => {
                    tracing::debug!(
                        backend = entry.backend.name(),
                        "could not refresh models ({e}); keeping the last list"
                    );
                    continue;
                }
            };
            let before = entry.models();
            if ids(&before) == ids(&models) {
                continue;
            }
            tracing::info!(
                backend = entry.backend.name(),
                "models changed: {} -> {}",
                ids(&before).join(", "),
                ids(&models).join(", ")
            );
            *entry.models.write().unwrap_or_else(|e| e.into_inner()) = models;
            changed = true;
        }
        changed
    }

    pub fn models(&self) -> Vec<ModelDescriptor> {
        self.entries.iter().flat_map(|e| e.models()).collect()
    }

    pub fn caps(&self) -> Vec<String> {
        let mut caps: Vec<String> = self
            .models()
            .iter()
            .map(|m| m.kind.as_str().to_string())
            .collect();
        if caps.is_empty() {
            caps.extend(
                self.entries
                    .iter()
                    .map(|e| e.backend.kind().as_str().to_string()),
            );
        }
        caps.sort();
        caps.dedup();
        caps
    }

    pub fn backends(&self) -> impl Iterator<Item = &Arc<dyn Backend>> {
        self.entries.iter().map(|e| &e.backend)
    }

    /// Pick the backend for a job. A requested model that no backend
    /// advertises is refused rather than quietly served by something else —
    /// silently substituting weights would make the result hash a lie.
    pub fn route(&self, payload: &JobPayload) -> Result<&Arc<dyn Backend>> {
        let kind = payload.kind();
        let requested = requested_model(payload);

        let of_kind: Vec<&Entry> = self
            .entries
            .iter()
            .filter(|e| {
                e.backend.kind() == kind || e.models().iter().any(|m| m.kind == kind)
            })
            .collect();

        if of_kind.is_empty() {
            return Err(WorkerError::Rejected(format!(
                "this worker has no {} backend",
                kind.as_str()
            )));
        }

        match requested {
            None => Ok(&of_kind[0].backend),
            Some(model) => of_kind
                .iter()
                .find(|e| {
                    e.models()
                        .iter()
                        .any(|m| m.id == model || m.sha256.as_deref() == Some(model))
                })
                .map(|e| &e.backend)
                // A backend that advertised nothing (server was down at boot)
                // still gets a chance rather than hard-failing the job.
                .or_else(|| {
                    of_kind
                        .iter()
                        .find(|e| e.models().is_empty())
                        .map(|e| &e.backend)
                })
                .ok_or_else(|| {
                    WorkerError::Rejected(format!(
                        "model '{model}' is not served here (have: {})",
                        self.models()
                            .iter()
                            .map(|m| m.id.clone())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                }),
        }
    }
}

fn requested_model(payload: &JobPayload) -> Option<&str> {
    match payload {
        JobPayload::Llm(p) => p.model_id.as_deref().or(p.model_hash.as_deref()),
        JobPayload::Video(p) => p.checkpoint_id.as_deref().or(p.model_hash.as_deref()),
        JobPayload::Image(p) => p.checkpoint_id.as_deref().or(p.model_hash.as_deref()),
    }
}

#[cfg(any(test, feature = "testutil"))]
pub mod testing {
    use super::*;
    use rootmode_core::{sha256_hex, PROTOCOL_VERSION};

    /// A backend that answers instantly, for exercising routing and the server
    /// without a GPU anywhere in sight.
    pub struct StubBackend {
        pub kind: JobKind,
        pub reply: String,
    }

    #[async_trait]
    impl Backend for StubBackend {
        fn name(&self) -> &str {
            "stub"
        }
        fn kind(&self) -> JobKind {
            self.kind
        }
        async fn discover_models(&self) -> Result<Vec<ModelDescriptor>> {
            Ok(vec![])
        }
        async fn health(&self) -> Result<String> {
            Ok("stub".into())
        }
        async fn run(
            &self,
            job_id: Uuid,
            _payload: &JobPayload,
            progress: &Progress,
        ) -> Result<JobResult> {
            progress.set(0.5);
            progress.delta(&self.reply, "");
            Ok(JobResult {
                v: PROTOCOL_VERSION,
                job_id,
                kind: JobKind::Llm,
                tool_calls: Vec::new(),
                sha256: sha256_hex(self.reply.as_bytes()),
                text: Some(self.reply.clone()),
                image_path_or_b64: None,
                thinking: None,
                meta: serde_json::json!({ "model": "stub" }),
            })
        }
    }

    pub fn registry_with(models: Vec<ModelDescriptor>, kind: JobKind) -> Registry {
        Registry::from_backends(vec![(
            Arc::new(StubBackend {
                kind,
                reply: "ok".into(),
            }) as Arc<dyn Backend>,
            models,
        )])
    }

    /// A backend that takes a while, for tests that need to catch a job
    /// mid-flight — a stop that only ever lands on an instant job proves
    /// nothing about the branch that matters.
    pub struct SlowBackend {
        pub kind: JobKind,
        pub delay: std::time::Duration,
    }

    #[async_trait]
    impl Backend for SlowBackend {
        fn name(&self) -> &str {
            "slow"
        }
        fn kind(&self) -> JobKind {
            self.kind
        }
        async fn discover_models(&self) -> Result<Vec<ModelDescriptor>> {
            Ok(vec![])
        }
        async fn health(&self) -> Result<String> {
            Ok("slow".into())
        }
        async fn run(
            &self,
            job_id: Uuid,
            _payload: &JobPayload,
            _progress: &Progress,
        ) -> Result<JobResult> {
            tokio::time::sleep(self.delay).await;
            Ok(JobResult {
                v: PROTOCOL_VERSION,
                job_id,
                kind: JobKind::Llm,
                tool_calls: Vec::new(),
                sha256: sha256_hex(b"slow"),
                text: Some("slow".into()),
                image_path_or_b64: None,
                thinking: None,
                meta: serde_json::json!({ "model": "slow" }),
            })
        }
    }

    pub fn registry_slow(kind: JobKind, delay: std::time::Duration) -> Registry {
        Registry::from_backends(vec![(
            Arc::new(SlowBackend { kind, delay }) as Arc<dyn Backend>,
            vec![],
        )])
    }

    /// A stub listing several models at their own prices.
    pub fn registry_priced_many(kind: JobKind, models: &[(&str, f64)]) -> Registry {
        use rootmode_core::Price;
        Registry::from_backends(vec![(
            Arc::new(StubBackend {
                kind,
                reply: "ok".into(),
            }) as Arc<dyn Backend>,
            models
                .iter()
                .map(|(id, amount)| ModelDescriptor {
                    id: (*id).into(),
                    sha256: None,
                    kind,
                    price: Some(Price::new(*amount)),
                })
                .collect(),
        )])
    }

    /// A stub that advertises a price, so holdback tests have something to bill.
    pub fn registry_priced(kind: JobKind, amount: f64) -> Registry {
        use rootmode_core::Price;
        Registry::from_backends(vec![(
            Arc::new(StubBackend {
                kind,
                reply: "ok".into(),
            }) as Arc<dyn Backend>,
            vec![ModelDescriptor {
                id: "stub".into(),
                sha256: None,
                kind,
                price: Some(Price::new(amount)),
            }],
        )])
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use rootmode_core::{ChatMessage, ImageParams, LlmParams, VideoParams};

    fn llm(model: Option<&str>) -> JobPayload {
        JobPayload::Llm(LlmParams {
            model_hash: None,
            model_id: model.map(str::to_string),
            messages: vec![ChatMessage::new("user", "hi")],
            tools: Vec::new(),
            max_tokens: 16,
            temperature: 0.0,
        })
    }

    fn image() -> JobPayload {
        JobPayload::Image(ImageParams {
            model_hash: None,
            checkpoint_id: None,
            prompt: "x".into(),
            from_image: None,
            change: None,
            mask: None,
        })
    }

    fn descriptor(id: &str) -> ModelDescriptor {
        ModelDescriptor {
            id: id.into(),
            sha256: None,
            kind: JobKind::Llm,
            price: None,
        }
    }

    #[test]
    fn routes_by_kind_and_advertises_caps() {
        let registry = registry_with(vec![descriptor("llama-3.1-8b")], JobKind::Llm);
        assert_eq!(registry.caps(), vec!["llm"]);
        assert!(registry.route(&llm(None)).is_ok());

        let err = registry.route(&image()).err().unwrap().to_string();
        assert!(err.contains("no image backend"), "got: {err}");
    }

    #[test]
    fn refuses_a_model_it_does_not_serve() {
        let registry = registry_with(vec![descriptor("llama-3.1-8b")], JobKind::Llm);
        assert!(registry.route(&llm(Some("llama-3.1-8b"))).is_ok());

        let err = registry
            .route(&llm(Some("mixtral")))
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("not served here"), "got: {err}");
        assert!(
            err.contains("llama-3.1-8b"),
            "the error lists what is available"
        );
    }

    #[test]
    fn a_backend_that_listed_nothing_still_accepts_jobs() {
        // vLLM down at boot: we advertise no models, but a later job should
        // reach it rather than being refused by our own stale view.
        let registry = registry_with(vec![], JobKind::Llm);
        assert!(registry.route(&llm(Some("anything"))).is_ok());
    }

    #[test]
    fn video_jobs_route_to_a_backend_that_advertises_video_models() {
        // ComfyUI's own kind is still "image"; video is a model it also holds.
        let registry = registry_with(
            vec![ModelDescriptor {
                id: "minimax-h3".into(),
                sha256: None,
                kind: JobKind::Video,
                price: None,
            }],
            JobKind::Image,
        );
        assert_eq!(registry.caps(), vec!["video"]);
        let payload = JobPayload::Video(VideoParams {
            model_hash: None,
            checkpoint_id: Some("minimax-h3".into()),
            prompt: "a clip".into(),
            from_image: None,
        });
        assert!(registry.route(&payload).is_ok());
    }
}

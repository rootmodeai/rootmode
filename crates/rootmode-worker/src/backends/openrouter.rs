//! Seed capacity: a node that serves real models without owning a GPU.
//!
//! A new network has a chicken-and-egg problem. Nobody brings a GPU to a
//! network with no clients, and no client joins a network with no models. So
//! the first workers are ours, and they answer by forwarding to OpenRouter.
//!
//! On the wire this is indistinguishable from a node with hardware, which is
//! the point: [`PeerAnnounce`](rootmode_core::protocol::PeerAnnounce) carries a
//! label, capabilities, models and a payout address, and none of those say how
//! the answer was produced. A client picks one of these the same way it picks
//! any other, and gets a real answer from a real model.
//!
//! Two things keep it from being a lie rather than a scaffold:
//!
//! * **Prices are OpenRouter's own**, read from their API rather than typed in
//!   here, times `markup` (the seed fleet uses 1.15 — 15% above catalogue).
//! * **It is not permanent.** Every one of these is capacity we are renting
//!   until real workers arrive, and each real GPU that joins is one of these we
//!   can turn off.
//!
//! The chat path is not reimplemented — OpenRouter speaks the same
//! OpenAI-shaped API as vLLM, so [`VllmBackend`] does the work and this type
//! only handles what is different: which models to advertise, what they are
//! called, and what they cost.

use std::collections::BTreeMap;
use std::sync::RwLock;

use async_trait::async_trait;
use rootmode_core::{JobKind, JobPayload, JobResult, ModelDescriptor, Price};
use serde::Deserialize;
use uuid::Uuid;

use super::vllm::VllmBackend;
use super::{Backend, Progress};
use crate::config::{OpenRouterConfig, VllmConfig};
use crate::error::{Result, WorkerError};

/// OpenRouter's base URL. `/v1/...` is appended, so the `/api` is where the
/// path stops.
const BASE: &str = "https://openrouter.ai/api";

pub struct OpenRouterBackend {
    config: OpenRouterConfig,
    /// Does the actual talking. OpenRouter is OpenAI-shaped, so there is
    /// nothing here worth writing twice.
    inner: VllmBackend,
    http: reqwest::Client,
    /// Advertised id → the id OpenRouter knows, e.g.
    /// `llama-3.3-70b-instruct` → `meta-llama/llama-3.3-70b-instruct`.
    upstream: RwLock<BTreeMap<String, String>>,
}

impl OpenRouterBackend {
    pub fn new(config: OpenRouterConfig) -> Result<Self> {
        if config.api_key.trim().is_empty() {
            return Err(WorkerError::Config(
                "openrouter backend needs an api_key".into(),
            ));
        }
        let inner = VllmBackend::new(VllmConfig {
            endpoint: BASE.into(),
            api_key: Some(config.api_key.clone()),
            models: Vec::new(),
            model_hashes: BTreeMap::new(),
            price: None,
            prices: BTreeMap::new(),
            currency: "USD".into(),
            timeout_secs: config.timeout_secs,
        })?
        .reporting_cost();
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .map_err(|e| WorkerError::backend("vllm", e))?;
        Ok(Self {
            config,
            inner,
            http,
            upstream: RwLock::new(BTreeMap::new()),
        })
    }

    /// What OpenRouter calls a model this node advertises.
    fn upstream_id(&self, advertised: &str) -> Option<String> {
        self.upstream
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(advertised)
            .cloned()
    }
}

/// The vendor prefix, dropped.
///
/// A node with one 70B checkpoint on it advertises `llama-3.3-70b-instruct`,
/// not `meta-llama/llama-3.3-70b-instruct` — the second is a marketplace's
/// catalogue key, and a machine with weights on disk does not have one.
fn advertised_id(upstream: &str) -> String {
    upstream
        .rsplit('/')
        .next()
        .unwrap_or(upstream)
        .trim_end_matches(":free")
        .to_string()
}

#[derive(Deserialize)]
struct Catalogue {
    data: Vec<Listed>,
}

#[derive(Deserialize)]
struct Listed {
    id: String,
    #[serde(default)]
    pricing: Pricing,
}

/// Per *token*, as decimal strings — so `"0.0000025"` is $2.50 per million.
#[derive(Deserialize, Default)]
struct Pricing {
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    completion: String,
    #[serde(default)]
    input_cache_read: String,
    #[serde(default)]
    input_cache_write: String,
}

impl Pricing {
    fn per_million_token(raw: &str) -> Option<f64> {
        if raw.is_empty() {
            return None;
        }
        let per_token: f64 = raw.parse().ok()?;
        Some(per_token * 1_000_000.0)
    }

    /// OpenRouter's three (or four) rates, per million tokens, after markup.
    ///
    /// `None` when both prompt and completion are missing or zero — a free
    /// listing, not a model we invent a price for. Cache-read of `"0"` is
    /// kept as zero (free hits). Cache-write, when listed above input, is
    /// kept so uncached prompt can be billed at the write premium.
    fn to_price(&self, markup: f64) -> Option<Price> {
        let markup = if markup > 0.0 { markup } else { 1.0 };
        let input = Self::per_million_token(&self.prompt).unwrap_or(0.0) * markup;
        let output = Self::per_million_token(&self.completion).unwrap_or(0.0) * markup;
        if input <= 0.0 && output <= 0.0 {
            return None;
        }
        let input = if input > 0.0 { input } else { output };
        let output = if output > 0.0 { output } else { input };
        let cache = Self::per_million_token(&self.input_cache_read).map(|v| v * markup);
        let cache_write = Self::per_million_token(&self.input_cache_write)
            .map(|v| v * markup)
            .filter(|v| *v > 0.0);
        let amount = input
            .max(output)
            .max(cache.unwrap_or(0.0))
            .max(cache_write.unwrap_or(0.0));
        Some(
            Price {
                amount,
                currency: "USD".into(),
                input: Some(input),
                output: Some(output),
                cache,
                cache_write,
            }
            .round_protocol(),
        )
    }
}

#[async_trait]
impl Backend for OpenRouterBackend {
    /// Reports as `vllm`, deliberately.
    ///
    /// Backend names reach clients inside error strings, and a node whose
    /// failures are tagged differently from every other node's is a node
    /// clients can single out. Which of our own machines this is belongs in
    /// its config file, not in a stranger's error message.
    fn name(&self) -> &str {
        "vllm"
    }

    fn kind(&self) -> JobKind {
        JobKind::Llm
    }

    async fn discover_models(&self) -> Result<Vec<ModelDescriptor>> {
        let resp = self
            .http
            .get(format!("{BASE}/v1/models"))
            .bearer_auth(&self.config.api_key)
            .send()
            .await
            .map_err(|e| WorkerError::backend("vllm", e))?;
        if !resp.status().is_success() {
            let status = resp.status();
            return Err(WorkerError::backend(
                "vllm",
                format!("HTTP {status} listing models"),
            ));
        }
        let catalogue: Catalogue = resp
            .json()
            .await
            .map_err(|e| WorkerError::backend("vllm", format!("bad model list: {e}")))?;

        // Serving the whole catalogue is the tell: no single machine holds
        // three hundred checkpoints, and a node claiming to would be useless
        // to route against anyway. Each of these nodes is configured with the
        // handful it is pretending to have on disk — and each entry resolves
        // to exactly one model, or to none.
        let mut map = BTreeMap::new();
        let mut models = Vec::new();
        for want in &self.config.models {
            let Some(listed) = pick(want, &catalogue.data) else {
                tracing::warn!("openrouter does not list '{want}' — not advertising it");
                continue;
            };
            let id = advertised_id(&listed.id);
            if map.contains_key(&id) {
                tracing::warn!("'{want}' resolves to '{id}', which is already advertised");
                continue;
            }
            let price = listed.pricing.to_price(self.config.markup);
            if listed.id != id {
                tracing::debug!("advertising '{}' as '{id}'", listed.id);
            }
            map.insert(id.clone(), listed.id.clone());
            models.push(ModelDescriptor {
                id,
                kind: JobKind::Llm,
                sha256: None,
                price,
            });
        }

        *self.upstream.write().unwrap_or_else(|e| e.into_inner()) = map;
        Ok(models)
    }

    async fn health(&self) -> Result<String> {
        let models = self.discover_models().await?;
        Ok(format!(
            "{} model(s): {}",
            models.len(),
            models
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }

    async fn run(&self, job_id: Uuid, payload: &JobPayload, progress: &Progress) -> Result<JobResult> {
        // Translate the advertised name back to the catalogue key before
        // forwarding; a client asked for what this node said it had.
        let mut payload = payload.clone();
        if let JobPayload::Llm(params) = &mut payload {
            match params.model_id.as_deref().and_then(|id| self.upstream_id(id)) {
                Some(upstream) => params.model_id = Some(upstream),
                // No model named, or one we do not carry. Left alone: the
                // inner backend produces the same "not served here" error any
                // other node would.
                None => {}
            }
        }

        let mut result = self.inner.run(job_id, &payload, progress).await?;

        // ...and back again in the answer, so a client never sees a name this
        // node did not advertise.
        if let Some(model) = result.meta.get("model").and_then(|m| m.as_str()) {
            let shown = advertised_id(model);
            if let Some(slot) = result.meta.get_mut("model") {
                *slot = shown.into();
            }
        }
        Ok(result)
    }
}

/// The one model a config entry means.
///
/// Exact first — on OpenRouter's own id or on the bare name. Only if nothing
/// matches exactly does it fall back to a prefix, and then it takes the
/// shortest candidate: asking for `qwen3-coder` should give you `qwen3-coder`,
/// not that model plus `-next`, `-plus`, `-flash` and `-30b-a3b-instruct`. A
/// node with five near-identical variants of one model on its shelf is a node
/// that does not look like a machine.
fn pick<'a>(wanted: &str, catalogue: &'a [Listed]) -> Option<&'a Listed> {
    let wanted = wanted.to_lowercase();
    catalogue
        .iter()
        .find(|m| {
            let id = m.id.to_lowercase();
            id == wanted || advertised_id(&id) == wanted
        })
        .or_else(|| {
            catalogue
                .iter()
                .filter(|m| advertised_id(&m.id.to_lowercase()).starts_with(&wanted))
                .min_by_key(|m| m.id.len())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_catalogue_key_becomes_something_a_machine_would_call_a_file() {
        assert_eq!(advertised_id("meta-llama/llama-3.3-70b-instruct"), "llama-3.3-70b-instruct");
        assert_eq!(advertised_id("qwen/qwen3-coder:free"), "qwen3-coder");
        // Already bare, e.g. a model listed without a vendor.
        assert_eq!(advertised_id("mythomax-l2-13b"), "mythomax-l2-13b");
    }

    #[test]
    fn prices_arrive_per_token_and_are_advertised_per_million() {
        let pricing = Pricing {
            prompt: "0.0000003".into(),
            completion: "0.0000025".into(),
            input_cache_read: "0.00000003".into(),
            input_cache_write: "0.000000375".into(),
        };
        let price = pricing.to_price(1.0).expect("priced");
        assert_eq!(price.input, Some(0.3));
        assert_eq!(price.output, Some(2.5));
        assert_eq!(price.cache, Some(0.03));
        assert_eq!(price.cache_write, Some(0.38)); // 0.375 rounds up
        assert_eq!(price.amount, 2.5);
        let marked = pricing.to_price(1.15).expect("priced");
        assert_eq!(marked.input, Some(0.35)); // 0.345
        assert_eq!(marked.output, Some(2.88)); // 2.875
        assert_eq!(marked.cache, Some(0.04)); // 0.0345
        assert_eq!(marked.cache_write, Some(0.44)); // 0.43125
        assert_eq!(marked.amount, 2.88);
        // 800 fresh at write premium 0.38 + 200 cached at 0.03 + 100 out at 2.5
        assert_eq!(price.charge_llm_micros(1000, 100, 200), 304 + 6 + 250);
    }

    #[test]
    fn a_free_model_is_advertised_free_rather_than_at_a_guess() {
        let pricing = Pricing {
            prompt: "0".into(),
            completion: "0".into(),
            ..Pricing::default()
        };
        assert_eq!(pricing.to_price(1.0), None);

        // A listing with no pricing block at all must not become a charge.
        assert_eq!(Pricing::default().to_price(1.0), None);
    }

    fn listed(id: &str) -> Listed {
        Listed {
            id: id.into(),
            pricing: Pricing::default(),
        }
    }

    #[test]
    fn a_config_can_name_a_model_the_short_way() {
        let catalogue = vec![
            listed("meta-llama/llama-3.3-70b-instruct"),
            listed("openai/gpt-4o"),
        ];
        assert_eq!(pick("llama-3.3-70b-instruct", &catalogue).unwrap().id, "meta-llama/llama-3.3-70b-instruct");
        assert_eq!(pick("meta-llama/llama-3.3-70b-instruct", &catalogue).unwrap().id, "meta-llama/llama-3.3-70b-instruct");
        assert!(pick("claude-4-opus", &catalogue).is_none());
    }

    #[test]
    fn one_entry_never_drags_in_a_models_whole_family() {
        // The real catalogue, which is where this bit me: five things start
        // with `qwen3-coder`, and a node offering all five looks like a
        // reseller rather than a machine.
        let catalogue = vec![
            listed("qwen/qwen3-coder-next"),
            listed("qwen/qwen3-coder-plus"),
            listed("qwen/qwen3-coder-flash"),
            listed("qwen/qwen3-coder-30b-a3b-instruct"),
            listed("qwen/qwen3-coder"),
        ];
        assert_eq!(pick("qwen3-coder", &catalogue).unwrap().id, "qwen/qwen3-coder");

        // A name that is only ever a prefix still resolves, to the plainest
        // thing it could mean.
        assert_eq!(pick("qwen3-coder-fl", &catalogue).unwrap().id, "qwen/qwen3-coder-flash");
    }
}

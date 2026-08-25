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
use base64::Engine as _;
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
    #[serde(default)]
    architecture: Architecture,
}

#[derive(Deserialize, Default)]
struct Architecture {
    #[serde(default)]
    output_modalities: Vec<String>,
}

impl Listed {
    /// A model that answers with pictures. Text-first routers (`openrouter/auto`)
    /// also list `image` among their outputs and are not what an image job
    /// should land on; the ones that mean it name `image` first.
    fn makes_images(&self) -> bool {
        self.architecture.output_modalities.first().map(String::as_str) == Some("image")
    }
}

/// Output images are metered in tokens, and how many a picture costs is the
/// provider's business (Gemini bills 1,290 for a 1024×1024; GPT image models
/// vary with size and quality). The advertised per-image price is the
/// client's lock, so it must cover the dear case: assume this many.
const IMAGE_OUTPUT_TOKENS: f64 = 2_000.0;

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
    /// Per output-image token, for models that answer with pictures.
    #[serde(default)]
    image_output: String,
}

impl Pricing {
    /// A flat per-image price, after markup, for a model that answers with
    /// pictures. `None` when the catalogue names no output-image rate: a
    /// picture that costs something must not be advertised as free.
    fn per_image(&self, markup: f64) -> Option<Price> {
        let markup = if markup > 0.0 { markup } else { 1.0 };
        let per_token: f64 = self.image_output.trim().parse().ok()?;
        if per_token <= 0.0 {
            return None;
        }
        Some(Price::new(per_token * IMAGE_OUTPUT_TOKENS * markup))
    }

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
            let (kind, price) = if listed.makes_images() {
                match listed.pricing.per_image(self.config.markup) {
                    Some(p) => (JobKind::Image, Some(p)),
                    None => {
                        tracing::warn!("'{want}' makes images but lists no output-image rate — not advertising it");
                        continue;
                    }
                }
            } else {
                (JobKind::Llm, listed.pricing.to_price(self.config.markup))
            };
            if listed.id != id {
                tracing::debug!("advertising '{}' as '{id}'", listed.id);
            }
            map.insert(id.clone(), listed.id.clone());
            models.push(ModelDescriptor {
                id,
                kind,
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
        if let JobPayload::Image(params) = payload {
            return self.run_image(job_id, params).await;
        }
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
        // The advertised rates are the catalogue's list price times markup,
        // but OpenRouter routes each request to a provider of its choosing,
        // and that provider's rate can sit above the list. The bill it
        // reports is what this job actually cost; the job must never be
        // billed below that plus the same margin, or the margin is fiction.
        if let Some(cost) = result.meta.get("upstream_cost").and_then(|c| c.as_f64()) {
            result.meta["min_bill_micros"] = serde_json::json!(bill_floor_micros(cost, self.config.markup));
        }
        Ok(result)
    }
}

/// The least a job may be billed, in micros: what it actually cost upstream,
/// plus the same margin the advertised rates carry. Rounded up — a floor
/// that rounds down is not a floor.
fn bill_floor_micros(upstream_cost_usd: f64, markup: f64) -> u64 {
    let markup = if markup > 0.0 { markup } else { 1.0 };
    (upstream_cost_usd * markup * 1_000_000.0).ceil().max(0.0) as u64
}

impl OpenRouterBackend {
    /// A picture from a model that answers with one: the same chat-completions
    /// call as text, asking for an image modality, with the picture coming
    /// back inline as a data URL. One image per job.
    async fn run_image(&self, job_id: Uuid, params: &rootmode_core::ImageParams) -> Result<JobResult> {
        let advertised = params
            .checkpoint_id
            .as_deref()
            .ok_or_else(|| WorkerError::Rejected("image jobs must name a model".into()))?;
        let upstream = self
            .upstream_id(advertised)
            .ok_or_else(|| WorkerError::Rejected(format!("'{advertised}' is not served here")))?;

        let mut content = vec![serde_json::json!({ "type": "text", "text": params.prompt })];
        if let Some(from) = params.from_image.as_deref().filter(|s| !s.trim().is_empty()) {
            content.push(serde_json::json!({
                "type": "image_url",
                "image_url": { "url": as_data_url(from) },
            }));
        }
        let body = serde_json::json!({
            "model": upstream,
            "messages": [{ "role": "user", "content": content }],
            "modalities": ["image", "text"],
            "usage": { "include": true },
        });
        let resp = self
            .http
            .post(format!("{BASE}/v1/chat/completions"))
            .bearer_auth(&self.config.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| WorkerError::backend("openrouter", e))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| WorkerError::backend("openrouter", e))?;
        if !status.is_success() {
            return Err(WorkerError::backend("openrouter", format!("HTTP {status}: {text}")));
        }
        let (bytes, cost) = parse_image_response(&text)?;
        let mut meta = serde_json::json!({
            "model": advertised,
            "backend": "openrouter",
            "prompt_tokens": 0,
            "completion_tokens": 0,
        });
        if let Some(cost) = cost {
            meta["upstream_cost"] = serde_json::json!(cost);
            meta["min_bill_micros"] = serde_json::json!(bill_floor_micros(cost, self.config.markup));
        }
        Ok(JobResult {
            v: rootmode_core::PROTOCOL_VERSION,
            job_id,
            kind: JobKind::Image,
            sha256: rootmode_core::hash::sha256_hex(&bytes),
            text: None,
            tool_calls: Vec::new(),
            image_path_or_b64: Some(base64::engine::general_purpose::STANDARD.encode(&bytes)),
            thinking: None,
            meta,
        })
    }
}

/// Base64 in, data URL out; a data URL is left alone.
fn as_data_url(image: &str) -> String {
    let s = image.trim();
    if s.starts_with("data:") {
        s.to_string()
    } else {
        format!("data:image/png;base64,{s}")
    }
}

/// The picture out of an image-modality chat completion, and what it cost.
/// A reply with no picture is an error carrying whatever text the model
/// sent instead — usually a refusal, and worth showing.
fn parse_image_response(text: &str) -> Result<(Vec<u8>, Option<f64>)> {
    let v: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| WorkerError::backend("openrouter", format!("bad response: {e}")))?;
    let message = &v["choices"][0]["message"];
    let url = message["images"][0]["image_url"]["url"]
        .as_str()
        .ok_or_else(|| {
            let said = message["content"].as_str().unwrap_or("").trim();
            WorkerError::backend(
                "openrouter",
                if said.is_empty() {
                    "the model returned no image".to_string()
                } else {
                    format!("the model returned no image: {said}")
                },
            )
        })?;
    let b64 = url
        .split_once(";base64,")
        .map(|(_, b)| b)
        .ok_or_else(|| WorkerError::backend("openrouter", "image is not a base64 data URL"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|e| WorkerError::backend("openrouter", format!("image is not valid base64: {e}")))?;
    if bytes.is_empty() {
        return Err(WorkerError::backend("openrouter", "the model returned an empty image"));
    }
    let cost = v["usage"]["cost"].as_f64();
    Ok((bytes, cost))
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
    #[test]
    fn an_image_model_is_priced_per_picture_at_the_dear_case() {
        let listed: super::Listed = serde_json::from_str(r#"{"id":"google/gemini-3.1-flash-image",
            "pricing":{"prompt":"0.0000005","completion":"0.000003","image_output":"0.00006"},
            "architecture":{"output_modalities":["image","text"]}}"#).unwrap();
        assert!(listed.makes_images());
        let price = listed.pricing.per_image(1.15).unwrap();
        // 0.00006 × 2000 tokens × 1.15 = 0.138 → rounded up to the cent
        assert_eq!(price.amount, 0.14);
        let router: super::Listed = serde_json::from_str(r#"{"id":"openrouter/auto",
            "pricing":{"prompt":"-1","completion":"-1"},
            "architecture":{"output_modalities":["text","image"]}}"#).unwrap();
        assert!(!router.makes_images(), "text-first routers are not image models");
    }

    #[test]
    fn a_picture_comes_out_of_the_data_url_with_its_cost() {
        let png = b"\x89PNG fake";
        let b64 = base64::engine::general_purpose::STANDARD.encode(png);
        let body = format!(r#"{{"choices":[{{"message":{{"content":"","images":[{{"type":"image_url","image_url":{{"url":"data:image/png;base64,{b64}"}}}}]}}}}],"usage":{{"cost":0.0412}}}}"#);
        let (bytes, cost) = super::parse_image_response(&body).unwrap();
        assert_eq!(bytes, png);
        assert_eq!(cost, Some(0.0412));
        let refusal = r#"{"choices":[{"message":{"content":"I can't draw that."}}]}"#;
        let err = super::parse_image_response(refusal).unwrap_err().to_string();
        assert!(err.contains("I can't draw that."), "{err}");
    }

    #[test]
    fn the_bill_floor_is_actual_cost_plus_margin_rounded_up() {
        // The glm-5.2 job that exposed this: list-based bill 394 micros,
        // OpenRouter charged $0.0004022 — under the list-based bill by 2%.
        assert_eq!(super::bill_floor_micros(0.0004022, 1.15), 463);
        assert_eq!(super::bill_floor_micros(0.0, 1.15), 0);
        assert_eq!(super::bill_floor_micros(0.000001, 0.0), 1, "no markup configured is x1, never x0");
    }

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
            image_output: String::new(),
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
            architecture: Default::default(),
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

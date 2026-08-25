//! OpenAI-compatible chat completions: vLLM, SGLang, llama.cpp's server, TGI.
//!
//! Streaming is used so `job.status` progress is real rather than a fake ramp,
//! and so a long generation shows movement in the client.

use async_trait::async_trait;
use futures_util::StreamExt;
use rootmode_core::{
    sha256_hex, ChatMessage, JobKind, JobPayload, JobResult, LlmParams, ModelDescriptor, Price,
    TokenUsage, PROTOCOL_VERSION,
};
use serde::Deserialize;
use std::time::Duration;
use uuid::Uuid;

use super::{Backend, Progress};
use crate::config::VllmConfig;
use crate::error::{Result, WorkerError};

pub struct VllmBackend {
    config: VllmConfig,
    http: reqwest::Client,
    /// What the server said it had, last time we asked. Lets a job that names
    /// no model still report which one actually ran.
    discovered: std::sync::RwLock<Vec<String>>,
    /// Ask for billed usage *and* cost. OpenRouter honours this; a local
    /// server that does not know the field typically ignores it, but some
    /// reject unknown keys, so it stays off unless the caller is OpenRouter.
    include_cost: bool,
}

impl VllmBackend {
    pub fn new(config: VllmConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .build()
            .map_err(|e| WorkerError::backend("vllm", e))?;
        Ok(Self {
            config,
            http,
            discovered: std::sync::RwLock::new(Vec::new()),
            include_cost: false,
        })
    }

    /// OpenRouter: include `usage.cost` and cache details in the stream.
    pub fn reporting_cost(mut self) -> Self {
        self.include_cost = true;
        self
    }

    fn url(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.config.endpoint.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.config.api_key.as_deref().filter(|k| !k.is_empty()) {
            Some(key) => req.bearer_auth(key),
            None => req,
        }
    }

    /// The model to ask for: what the client requested, else the first one the
    /// operator pinned, else the first one the server reported.
    fn model_for(&self, params: &LlmParams) -> Option<String> {
        params
            .model_id
            .clone()
            .or_else(|| self.config.models.first().cloned())
            .or_else(|| {
                self.discovered
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .first()
                    .cloned()
            })
    }
}

#[derive(Deserialize)]
struct ModelList {
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

#[derive(Deserialize, Default, Clone)]
struct Usage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptDetails>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionDetails>,
    /// OpenRouter's billed dollar amount for this generation, when asked.
    #[serde(default)]
    cost: Option<f64>,
}

#[derive(Deserialize, Default, Clone)]
struct PromptDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Deserialize, Default, Clone)]
struct CompletionDetails {
    #[serde(default)]
    reasoning_tokens: u64,
}

impl From<&Usage> for TokenUsage {
    fn from(u: &Usage) -> Self {
        let cached = u
            .prompt_tokens_details
            .as_ref()
            .map(|d| d.cached_tokens)
            .unwrap_or(0);
        let reasoning = u
            .completion_tokens_details
            .as_ref()
            .map(|d| d.reasoning_tokens)
            .unwrap_or(0);
        TokenUsage {
            prompt: u.prompt_tokens,
            completion: if u.completion_tokens == 0 {
                u.total_tokens.saturating_sub(u.prompt_tokens)
            } else {
                u.completion_tokens
            },
            cached: cached.min(u.prompt_tokens),
            reasoning,
        }
    }
}

#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    /// vLLM can report a failure mid-stream instead of with an HTTP status.
    /// Swallowing it turns a real error into "empty completion".
    #[serde(default)]
    error: Option<serde_json::Value>,
    /// Sent in a final chunk when `stream_options.include_usage` is set. This
    /// is the authoritative token count — counting frames is a guess, and a
    /// price quoted per million tokens needs the real number.
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    /// Reasoning models (DeepSeek-R1 and friends, when vLLM runs with a
    /// reasoning parser) put their thinking here and only later — if at all —
    /// emit `content`. Counting only `content` makes a model that is working
    /// hard look like a model that returned nothing.
    ///
    /// Two spellings in the wild: older builds send `reasoning_content`,
    /// newer ones `reasoning`. Reading only one of them turns a model that
    /// spent its whole budget thinking into "the server returned an empty
    /// completion", which sends you looking in entirely the wrong place.
    #[serde(default, alias = "reasoning")]
    reasoning_content: Option<String>,
    /// Tool calls arrive in pieces like everything else: a first frame with
    /// the id and name, then `arguments` a fragment at a time, correlated by
    /// `index`.
    #[serde(default)]
    tool_calls: Option<Vec<DeltaToolCall>>,
}

#[derive(Deserialize)]
struct DeltaToolCall {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<DeltaFunction>,
}

#[derive(Deserialize)]
struct DeltaFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[async_trait]
impl Backend for VllmBackend {
    fn name(&self) -> &str {
        "vllm"
    }

    fn kind(&self) -> JobKind {
        JobKind::Llm
    }

    async fn discover_models(&self) -> Result<Vec<ModelDescriptor>> {
        // An explicit list in the config wins: an operator may want to expose
        // only one of several loaded models.
        let ids = if self.config.models.is_empty() {
            let resp = self
                .authed(self.http.get(self.url("/v1/models")))
                .send()
                .await
                .map_err(|e| WorkerError::backend("vllm", e))?;
            let resp = check_status(resp).await?;
            let list: ModelList = resp.json().await.map_err(|e| {
                WorkerError::backend("vllm", format!("bad /v1/models response: {e}"))
            })?;
            list.data.into_iter().map(|m| m.id).collect()
        } else {
            self.config.models.clone()
        };

        *self.discovered.write().unwrap_or_else(|e| e.into_inner()) = ids.clone();

        Ok(ids
            .into_iter()
            .map(|id| ModelDescriptor {
                sha256: self.config.model_hashes.get(&id).cloned(),
                price: self
                    .config
                    .prices
                    .get(&id)
                    .copied()
                    .or(self.config.price)
                    .map(|amount| {
                        Price {
                            amount,
                            currency: self.config.currency.clone(),
                            ..Price::default()
                        }
                        .round_protocol()
                    }),
                id,
                kind: JobKind::Llm,
            })
            .collect())
    }

    async fn health(&self) -> Result<String> {
        let models = self.discover_models().await?;
        Ok(format!(
            "{} — {} model(s): {}",
            self.config.endpoint,
            models.len(),
            models
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }

    async fn run(
        &self,
        job_id: Uuid,
        payload: &JobPayload,
        progress: &Progress,
    ) -> Result<JobResult> {
        let JobPayload::Llm(params) = payload else {
            return Err(WorkerError::Rejected(
                "vllm backend only runs llm jobs".into(),
            ));
        };

        let model = self.model_for(params);
        let messages = prepare_messages(&params.messages);
        let mut body = serde_json::json!({
            "messages": openai_messages(&messages),
            "max_tokens": params.max_tokens,
            "temperature": params.temperature,
            "stream": true,
            // Ask for the token counts. Servers that do not know this option
            // ignore it, and we fall back to the OpenAI tokenizer.
            "stream_options": { "include_usage": true },
        });
        if self.include_cost {
            // OpenRouter: billed dollars and cache/reasoning details.
            body["usage"] = serde_json::json!({ "include": true });
        }
        if let Some(model) = &model {
            body["model"] = serde_json::Value::String(model.clone());
        }
        if !params.tools.is_empty() {
            body["tools"] = serde_json::Value::Array(
                params
                    .tools
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "type": "function",
                            "function": {
                                "name": t.name,
                                "description": t.description.clone().unwrap_or_default(),
                                "parameters": t.input_schema,
                            }
                        })
                    })
                    .collect(),
            );
            // A reasoning model with tools must emit a tool call, not just an
            // answer. Servers that default to a high reasoning effort (e.g.
            // `--default-chat-template-kwargs reasoning_effort=max`) let the
            // model burn its whole budget thinking and stop without ever
            // emitting the answer delimiter — an empty completion. Cap the
            // effort per-request so a tool-using client gets a tool call, not
            // a monologue. Servers that do not know this option ignore it.
            body["chat_template_kwargs"] = serde_json::json!({
                "thinking": true,
                "reasoning_effort": "low",
            });
        }

        let resp = self
            .authed(self.http.post(self.url("/v1/chat/completions")))
            .json(&body)
            .send()
            .await
            .map_err(|e| WorkerError::backend("vllm", e))?;

        // `max_tokens` is what every open-weights server takes, and what this
        // sends first. The OpenAI-lineage APIs have replaced it with
        // `max_completion_tokens` and reject the old name outright — so rather
        // than pick one and be wrong half the time, ask again under the new
        // name when a server says that is the problem. Sending both is not an
        // option: servers that know only one of them reject unknown fields,
        // and a server that silently ignores the limit generates until it runs
        // out of context.
        let resp = match rename_max_tokens(resp).await? {
            Renamed::No(resp) => resp,
            Renamed::Yes(complaint) => {
                tracing::debug!("{complaint}; retrying with max_completion_tokens");
                let mut retry = body.clone();
                if let Some(limit) = retry.as_object_mut().and_then(|b| b.remove("max_tokens")) {
                    retry["max_completion_tokens"] = limit;
                }
                let resp = self
                    .authed(self.http.post(self.url("/v1/chat/completions")))
                    .json(&retry)
                    .send()
                    .await
                    .map_err(|e| WorkerError::backend("vllm", e))?;
                check_status(resp).await?
            }
        };

        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut chunks = 0u32;
        let mut finish_reason = None;
        let mut stream_error: Option<String> = None;
        let mut usage: Option<Usage> = None;
        // Keyed by the `index` the server correlates fragments with.
        let mut calls: std::collections::BTreeMap<usize, PartialCall> = Default::default();

        while let Some(next) = stream.next().await {
            let bytes = next.map_err(|e| WorkerError::backend("vllm", e))?;
            buffer.push_str(&String::from_utf8_lossy(&bytes));

            // SSE frames are newline-delimited; hold back any partial tail.
            while let Some(idx) = buffer.find('\n') {
                let line = buffer[..idx].trim().to_string();
                buffer.drain(..=idx);

                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }

                let chunk: StreamChunk = match serde_json::from_str(data) {
                    Ok(c) => c,
                    // Keep going: one unparseable frame should not lose a
                    // generation that is otherwise fine.
                    Err(e) => {
                        tracing::debug!("skipping unparseable vllm chunk: {e}");
                        continue;
                    }
                };
                if let Some(error) = chunk.error {
                    stream_error = Some(compact(&error));
                    break;
                }
                if let Some(reported) = chunk.usage {
                    usage = Some(reported);
                }

                for choice in chunk.choices {
                    let mut piece_text = String::new();
                    let mut piece_think = String::new();
                    if let Some(content) = choice.delta.content {
                        // Some OpenAI-compatible servers fail to parse the
                        // model's native tool markup and leave it in
                        // `content`. Do not stream that as the answer —
                        // a coding client then prints the invoke block.
                        let visible = markup_visible_delta(&text, &content);
                        text.push_str(&content);
                        piece_text.push_str(&visible);
                        chunks += 1;
                    }
                    if let Some(thinking) = choice.delta.reasoning_content {
                        reasoning.push_str(&thinking);
                        piece_think.push_str(&thinking);
                        chunks += 1;
                    }
                    progress.delta(&piece_text, &piece_think);
                    for part in choice.delta.tool_calls.into_iter().flatten() {
                        let slot = calls.entry(part.index).or_default();
                        if let Some(id) = part.id {
                            slot.id = id;
                        }
                        if let Some(f) = part.function {
                            if let Some(name) = f.name {
                                slot.name = name;
                            }
                            if let Some(args) = f.arguments {
                                slot.arguments.push_str(&args);
                                chunks += 1;
                            }
                        }
                    }
                    if choice.finish_reason.is_some() {
                        finish_reason = choice.finish_reason;
                    }
                }

                // One chunk is roughly one token. Cap below 1.0 — done is the
                // client's business, not ours.
                if params.max_tokens > 0 {
                    progress.set((chunks as f32 / params.max_tokens as f32).min(0.98));
                }
            }

            if stream_error.is_some() {
                break;
            }
        }

        if let Some(error) = stream_error {
            return Err(WorkerError::backend(
                "vllm",
                format!("the server reported: {error}"),
            ));
        }

        let (text, salvaged) = salvage_tool_markup(text);
        let mut tool_calls: Vec<rootmode_core::ToolCall> = calls
            .into_values()
            .filter(|c| !c.name.is_empty())
            .map(|c| rootmode_core::ToolCall {
                id: if c.id.is_empty() {
                    format!("call_{}", uuid::Uuid::new_v4().simple())
                } else {
                    c.id
                },
                name: c.name,
                // An empty argument list is `{}`, not the empty string —
                // clients parse this as JSON.
                arguments: if c.arguments.trim().is_empty() {
                    "{}".into()
                } else {
                    c.arguments
                },
            })
            .collect();
        // The inference server is supposed to parse native tool markup
        // into `tool_calls`. When it misses, the invoke lands in
        // `content` and the client prints it. Recover it here so any
        // OpenAI-compatible server — whatever model it is serving —
        // still drives a coding agent.
        if tool_calls.is_empty() && !salvaged.is_empty() {
            tool_calls = salvaged;
            if finish_reason.as_deref() != Some("tool_calls") {
                finish_reason = Some("tool_calls".into());
            }
        }

        // A model that chose to call a tool has answered; it just did not
        // answer in words.
        if text.is_empty() && tool_calls.is_empty() {
            return Err(WorkerError::backend(
                "vllm",
                empty_completion(&reasoning, finish_reason.as_deref(), params.max_tokens),
            ));
        }

        let billed = TokenUsage::measure(
            params,
            Some(&text),
            if reasoning.is_empty() {
                None
            } else {
                Some(reasoning.as_str())
            },
            &tool_calls,
        )
        .reconcile(usage.as_ref().map(TokenUsage::from));

        let mut meta = serde_json::json!({
            "model": model,
            "backend": "vllm",
            "finish_reason": finish_reason,
            "reasoning_chars": reasoning.chars().count(),
        });
        billed.write_into(&mut meta);
        if let Some(cost) = usage.as_ref().and_then(|u| u.cost) {
            meta["upstream_cost"] = serde_json::json!(cost);
        }

        Ok(JobResult {
            v: PROTOCOL_VERSION,
            job_id,
            kind: JobKind::Llm,
            sha256: sha256_hex(text.as_bytes()),
            text: Some(text),
            tool_calls,
            image_path_or_b64: None,
            thinking: if reasoning.is_empty() {
                None
            } else {
                Some(reasoning.clone())
            },
            meta,
        })
    }
}

/// A tool call being assembled from stream fragments.
#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

/// One chat message in the shape an OpenAI-compatible server expects.
///
/// Our [`ChatMessage`] keeps tool calls flat because that is easier to reason
/// about; the wire format nests them under `function`. Translating here rather
/// than storing the nested form keeps the protocol independent of any one
/// vendor's JSON.
/// What a picture looks like to an OpenAI-compatible server: a `data:` URL in
/// an `image_url` part.
///
/// Clients send either raw base64 or a `data:` URL already, and only the
/// second is meaningful here, so bare bytes get a prefix. The media type is
/// read from the bytes rather than trusted or assumed: servers do route on it,
/// and a PNG announced as a JPEG is refused by some and mis-decoded by others.
fn image_part(image: &str) -> serde_json::Value {
    let image = image.trim();
    let url = if image.starts_with("data:") {
        image.to_string()
    } else {
        format!("data:{};base64,{}", media_type(image), image)
    };
    serde_json::json!({ "type": "image_url", "image_url": { "url": url } })
}

/// The media type of base64 bytes, from the first few characters of the
/// encoding — each format's magic number lands in a fixed prefix.
fn media_type(base64: &str) -> &'static str {
    match base64.as_bytes() {
        [b'i', b'V', b'B', b'O', b'R', b'w', ..] => "image/png",
        [b'/', b'9', b'j', ..] => "image/jpeg",
        [b'R', b'0', b'l', b'G', b'O', b'D', ..] => "image/gif",
        [b'U', b'k', b'l', b'G', b'R', ..] => "image/webp",
        // Servers overwhelmingly sniff the bytes anyway; PNG is the safest
        // thing to claim when we genuinely cannot tell.
        _ => "image/png",
    }
}

/// Shape a conversation so an OpenAI-compatible server's chat template
/// will accept it. Qwen, Llama and most others allow a system message in
/// first position only, and only one of them; coding agents (Codex,
/// Claude Code) send several, including mid-conversation reminders.
/// The text is kept — folded into one opening system message, or into
/// the nearest user turn — so an instruction is never dropped.
fn prepare_messages(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut out = messages.to_vec();
    merge_leading_system_messages(&mut out);
    fold_late_system_messages(&mut out);
    out
}

fn merge_leading_system_messages(messages: &mut Vec<ChatMessage>) {
    let n = messages.iter().take_while(|m| m.role == "system").count();
    if n <= 1 {
        return;
    }
    let mut combined = String::new();
    for m in messages.iter().take(n) {
        let t = m.content.trim();
        if t.is_empty() {
            continue;
        }
        if !combined.is_empty() {
            combined.push_str("\n\n");
        }
        combined.push_str(t);
    }
    messages[0].content = combined;
    messages.drain(1..n);
}

fn fold_late_system_messages(messages: &mut Vec<ChatMessage>) {
    let opening = messages
        .iter()
        .position(|m| m.role != "system")
        .unwrap_or(messages.len());
    let mut i = opening;
    while i < messages.len() {
        if messages[i].role != "system" {
            i += 1;
            continue;
        }
        let text = format!(
            "<system-reminder>\n{}\n</system-reminder>",
            messages.remove(i).content.trim()
        );
        if i > opening && messages[i - 1].role == "user" {
            let prev = &mut messages[i - 1];
            prev.content = format!("{}\n\n{}", prev.content, text);
        } else if messages.get(i).is_some_and(|m| m.role == "user") {
            let next = &mut messages[i];
            next.content = format!("{}\n\n{}", text, next.content);
        } else {
            messages.insert(i, ChatMessage::new("user", text));
            i += 1;
        }
    }
}

/// The whole conversation on the wire, with every `tool` message naming the
/// tool it answers. The protocol correlates results to calls by id alone;
/// some providers (Kimi K3 among them) also want the tool's `name` on the
/// result, or fall back to matching results to calls by position — which
/// breaks the moment a client runs two tools in parallel. The name is
/// recoverable from the assistant turn that made the call, so send it.
fn openai_messages(messages: &[ChatMessage]) -> Vec<serde_json::Value> {
    let mut names: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for m in messages {
        for c in &m.tool_calls {
            names.insert(c.id.as_str(), c.name.as_str());
        }
    }
    messages
        .iter()
        .map(|m| {
            let mut out = openai_message(m);
            if m.role == "tool" {
                if let Some(name) = m.tool_call_id.as_deref().and_then(|id| names.get(id)) {
                    out["name"] = serde_json::Value::String((*name).to_string());
                }
            }
            out
        })
        .collect()
}

fn openai_message(m: &ChatMessage) -> serde_json::Value {
    // Plain text stays a plain string. Some servers accept only that shape for
    // a system or tool message, and every one of them accepts it for a user
    // message, so the array form is used strictly where it is needed.
    let content = if m.images.is_empty() {
        serde_json::Value::String(m.content.clone())
    } else {
        let mut parts = Vec::with_capacity(m.images.len() + 1);
        // Pictures first, question second: models follow the instruction more
        // reliably when it comes after what it refers to.
        parts.extend(m.images.iter().map(|i| image_part(i)));
        if !m.content.trim().is_empty() {
            parts.push(serde_json::json!({ "type": "text", "text": m.content }));
        }
        serde_json::Value::Array(parts)
    };

    let mut out = serde_json::json!({ "role": m.role, "content": content });
    if !m.tool_calls.is_empty() {
        out["tool_calls"] = serde_json::Value::Array(
            m.tool_calls
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "id": c.id,
                        "type": "function",
                        "function": { "name": c.name, "arguments": c.arguments },
                    })
                })
                .collect(),
        );
    }
    if let Some(id) = &m.tool_call_id {
        out["tool_call_id"] = serde_json::Value::String(id.clone());
    }
    out
}

/// DeepSeek V4's tool-call markup. `｜` is U+FF5C, not ASCII `|`.
const DSML_MARK: &str = "\u{FF5C}DSML\u{FF5C}";

/// Where native tool markup starts in `s`, if it does. Covers DeepSeek
/// DSML and the `<tool_call>` envelope Qwen, Hermes and friends use.
fn markup_start(s: &str) -> Option<usize> {
    let dsml = s.find(DSML_MARK).map(|i| s[..i].rfind('<').unwrap_or(i));
    let xml = s.find("<tool_call");
    match (dsml, xml) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// The part of `added` that is still ordinary answer text. Once native
/// tool markup starts, later content is a tool call in disguise and must
/// not be forwarded as tokens.
fn markup_visible_delta(previous: &str, added: &str) -> String {
    if markup_start(previous).is_some() {
        return String::new();
    }
    let mut combined = String::with_capacity(previous.len() + added.len());
    combined.push_str(previous);
    combined.push_str(added);
    let Some(start) = markup_start(&combined) else {
        return added.to_string();
    };
    if start <= previous.len() {
        String::new()
    } else {
        combined[previous.len()..start].to_string()
    }
}

/// Pull native tool markup out of a completion when the inference server
/// left it in `content` instead of `tool_calls`. Conservative: only
/// complete invoke blocks become calls, and whatever was around them
/// stays as the answer. Format-agnostic on purpose — operators point
/// this at whatever OpenAI-compatible server they have.
fn salvage_tool_markup(text: String) -> (String, Vec<rootmode_core::ToolCall>) {
    if text.contains(DSML_MARK) {
        let calls = parse_dsml_invokes(&text);
        return (strip_dsml(&text), calls);
    }
    if text.contains("<tool_call") {
        return salvage_xml_tool_calls(text);
    }
    (text, Vec::new())
}

fn parse_dsml_invokes(text: &str) -> Vec<rootmode_core::ToolCall> {
    let needle = format!("{DSML_MARK}invoke name=\"");
    let close = format!("</{DSML_MARK}invoke>");
    let mut calls = Vec::new();
    let mut search = text;
    let mut n = 0u32;
    while let Some(i) = search.find(&needle) {
        let after = &search[i + needle.len()..];
        let Some(q) = after.find('"') else { break };
        let name = after[..q].to_string();
        if name.is_empty() {
            search = &after[1..];
            continue;
        }
        let after_name = &after[q..];
        let Some(gt) = after_name.find('>') else { break };
        let body_and = &after_name[gt + 1..];
        let Some(end) = body_and.find(&close) else { break };
        let args = serde_json::Value::Object(parse_dsml_parameters(&body_and[..end])).to_string();
        n += 1;
        calls.push(rootmode_core::ToolCall {
            id: format!("call_dsml_{n}"),
            name,
            arguments: args,
        });
        search = &body_and[end + close.len()..];
    }
    calls
}

fn parse_dsml_parameters(body: &str) -> serde_json::Map<String, serde_json::Value> {
    let needle = format!("{DSML_MARK}parameter");
    let close = format!("</{DSML_MARK}parameter>");
    let mut map = serde_json::Map::new();
    let mut search = body;
    while let Some(i) = search.find(&needle) {
        let from_tag = &search[i..];
        let Some(gt) = from_tag.find('>') else { break };
        let tag = &from_tag[..gt];
        let Some(name) = dsml_attr(tag, "name") else {
            search = &from_tag[needle.len()..];
            continue;
        };
        let as_string = tag.contains("string=\"true\"") || tag.contains("string='true'");
        let after = &from_tag[gt + 1..];
        let Some(end) = after.find(&close) else { break };
        let raw = after[..end].to_string();
        let value = if as_string {
            serde_json::Value::String(raw)
        } else {
            serde_json::from_str(&raw).unwrap_or(serde_json::Value::String(raw))
        };
        map.insert(name, value);
        search = &after[end + close.len()..];
    }
    map
}

fn dsml_attr(tag: &str, key: &str) -> Option<String> {
    let p = format!("{key}=\"");
    let i = tag.find(&p)?;
    let rest = &tag[i + p.len()..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn strip_dsml(text: &str) -> String {
    let Some(mark) = text.find(DSML_MARK) else {
        return text.to_string();
    };
    let start = text[..mark].rfind('<').unwrap_or(mark);
    let last = text.rfind(DSML_MARK).unwrap_or(mark);
    let end = text[last..]
        .find('>')
        .map(|i| last + i + 1)
        .unwrap_or(text.len());
    let head = text[..start].trim_end();
    let tail = text[end..].trim_start();
    match (head.is_empty(), tail.is_empty()) {
        (true, true) => String::new(),
        (false, true) => head.to_string(),
        (true, false) => tail.to_string(),
        (false, false) => format!("{head}\n{tail}"),
    }
}

fn salvage_xml_tool_calls(text: String) -> (String, Vec<rootmode_core::ToolCall>) {
    let mut calls = Vec::new();
    let mut search = text.as_str();
    let mut n = 0u32;
    while let Some(start) = search.find("<tool_call") {
        let after_open = &search[start..];
        let Some(gt) = after_open.find('>') else { break };
        let body_start = &after_open[gt + 1..];
        let Some(end) = body_start.find("</tool_call>") else { break };
        if let Some(mut call) = parse_xml_tool_body(body_start[..end].trim()) {
            n += 1;
            if call.id.is_empty() {
                call.id = format!("call_xml_{n}");
            }
            calls.push(call);
        }
        search = &body_start[end + "</tool_call>".len()..];
    }
    (strip_xml_tool_calls(&text), calls)
}

fn parse_xml_tool_body(body: &str) -> Option<rootmode_core::ToolCall> {
    let body = body.trim();
    if body.starts_with('{') {
        let v: serde_json::Value = serde_json::from_str(body).ok()?;
        let name = v.get("name")?.as_str()?.to_string();
        if name.is_empty() {
            return None;
        }
        let arguments = match v.get("arguments") {
            Some(serde_json::Value::String(s)) => {
                if s.trim().is_empty() {
                    "{}".into()
                } else {
                    s.clone()
                }
            }
            Some(other) => other.to_string(),
            None => "{}".into(),
        };
        return Some(rootmode_core::ToolCall {
            id: String::new(),
            name,
            arguments,
        });
    }
    let needle = "<function=";
    let i = body.find(needle)?;
    let after = &body[i + needle.len()..];
    let name_end = after.find(['>', ' ', '\t', '\n'])?;
    let name = after[..name_end]
        .trim_matches(|c| c == '"' || c == '\'')
        .to_string();
    if name.is_empty() {
        return None;
    }
    let rest = &after[name_end..];
    let inner_end = rest.find("</function>").unwrap_or(rest.len());
    let inner = &rest[..inner_end];
    let mut map = serde_json::Map::new();
    let mut s = inner;
    while let Some(p) = s.find("<parameter=") {
        let after_p = &s[p + "<parameter=".len()..];
        let Some(name_end) = after_p.find('>') else { break };
        let pname = after_p[..name_end]
            .trim_matches(|c| c == '"' || c == '\'')
            .to_string();
        let val_and = &after_p[name_end + 1..];
        let end = val_and.find("</parameter>").unwrap_or(val_and.len());
        let raw = val_and[..end].to_string();
        let value = serde_json::from_str(&raw).unwrap_or(serde_json::Value::String(raw));
        if !pname.is_empty() {
            map.insert(pname, value);
        }
        s = if end < val_and.len() {
            &val_and[end + "</parameter>".len()..]
        } else {
            ""
        };
    }
    Some(rootmode_core::ToolCall {
        id: String::new(),
        name,
        arguments: serde_json::Value::Object(map).to_string(),
    })
}

fn strip_xml_tool_calls(text: &str) -> String {
    let Some(start) = text.find("<tool_call") else {
        return text.to_string();
    };
    let last_open = text.rfind("<tool_call").unwrap_or(start);
    let after = &text[last_open..];
    let end = after
        .find("</tool_call>")
        .map(|i| last_open + i + "</tool_call>".len())
        .unwrap_or(text.len());
    let head = text[..start].trim_end();
    let tail = text[end..].trim_start();
    match (head.is_empty(), tail.is_empty()) {
        (true, true) => String::new(),
        (false, true) => head.to_string(),
        (true, false) => tail.to_string(),
        (false, false) => format!("{head}\n{tail}"),
    }
}

/// Say what actually happened, because "empty completion" sends people looking
/// in the wrong place. The common case by far is a reasoning model spending
/// its whole budget thinking.
fn empty_completion(reasoning: &str, finish_reason: Option<&str>, max_tokens: u32) -> String {
    let out_of_room = finish_reason == Some("length");
    let thought = reasoning.chars().count();

    match (thought > 0, out_of_room) {
        (true, true) => format!(
            "the model spent all {max_tokens} tokens reasoning ({thought} characters of it) \
             and never got to an answer — raise max_tokens"
        ),
        (true, false) => format!(
            "the model produced {thought} characters of reasoning but no answer \
             (finish_reason: {})",
            finish_reason.unwrap_or("none")
        ),
        (false, true) => format!(
            "the model hit the max_tokens limit ({max_tokens}) without producing any output"
        ),
        (false, false) => format!(
            "the server returned an empty completion (finish_reason: {})",
            finish_reason.unwrap_or("none")
        ),
    }
}

/// One line, bounded, for an error message.
fn compact(value: &serde_json::Value) -> String {
    let text = value
        .get("message")
        .and_then(|m| m.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string());
    text.chars().take(300).collect()
}

/// Turn an HTTP error into something an operator can act on — the body of a
/// vLLM 400 says exactly what was wrong with the request.
/// Whether a failed response was the server objecting to `max_tokens`.
enum Renamed {
    /// Not that problem: the response as it was, already status-checked.
    No(reqwest::Response),
    /// It was, with what the server said, for the log.
    Yes(String),
}

/// Read a response far enough to tell "this server wants the other spelling"
/// apart from every other failure, which is reported as usual.
async fn rename_max_tokens(resp: reqwest::Response) -> Result<Renamed> {
    if resp.status() != reqwest::StatusCode::BAD_REQUEST {
        return Ok(Renamed::No(check_status(resp).await?));
    }
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let lower = body.to_lowercase();

    // Both halves have to be there. A server complaining that max_tokens is
    // *too large* also names the field, and retrying that under a new name
    // just fails again with a worse error message.
    let renamed = lower.contains("max_completion_tokens")
        || (lower.contains("max_tokens")
            && (lower.contains("unsupported") || lower.contains("not supported")));

    if renamed {
        return Ok(Renamed::Yes(body.chars().take(200).collect()));
    }
    let snippet: String = body.chars().take(400).collect();
    Err(WorkerError::backend(
        "vllm",
        format!("HTTP {status}: {snippet}"),
    ))
}

async fn check_status(resp: reqwest::Response) -> Result<reqwest::Response> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let snippet: String = body.chars().take(400).collect();
    Err(WorkerError::backend(
        "vllm",
        format!("HTTP {status}: {snippet}"),
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_tool_result_names_the_tool_it_answers() {
        use rootmode_core::{ChatMessage, ToolCall};
        let call = |id: &str, name: &str| ChatMessage {
            role: "assistant".into(),
            content: String::new(),
            tool_calls: vec![ToolCall { id: id.into(), name: name.into(), arguments: "{}".into() }],
            tool_call_id: None,
            images: Vec::new(),
        };
        let result = |id: &str| ChatMessage {
            role: "tool".into(),
            content: "ok".into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(id.into()),
            images: Vec::new(),
        };
        // Two parallel calls answered out of order: position would mislead,
        // the id does not.
        let wire = super::openai_messages(&[
            ChatMessage::new("user", "go"),
            call("c1", "read_file"),
            call("c2", "exec_command"),
            result("c2"),
            result("c1"),
        ]);
        assert_eq!(wire[3]["name"], "exec_command");
        assert_eq!(wire[3]["tool_call_id"], "c2");
        assert_eq!(wire[4]["name"], "read_file");
        assert!(wire[0].get("name").is_none(), "only tool messages carry a name");
    }

    use super::*;
    use crate::testutil::StubHttp;
    use rootmode_core::ChatMessage;

    fn params() -> LlmParams {
        LlmParams {
            model_hash: None,
            model_id: Some("llama-3.1-8b".into()),
            messages: vec![ChatMessage::new("user", "ping")],
            tools: Vec::new(),
            max_tokens: 8,
            temperature: 0.0,
        }
    }

    fn backend(endpoint: String) -> VllmBackend {
        VllmBackend::new(VllmConfig {
            endpoint,
            api_key: None,
            models: vec![],
            model_hashes: Default::default(),
            price: None,
            prices: Default::default(),
            currency: "USD".into(),
            timeout_secs: 10,
        })
        .unwrap()
    }

    fn priced(
        endpoint: String,
        price: Option<f64>,
        prices: std::collections::BTreeMap<String, f64>,
    ) -> VllmBackend {
        VllmBackend::new(VllmConfig {
            endpoint,
            api_key: None,
            models: vec![],
            model_hashes: Default::default(),
            price,
            prices,
            currency: "USD".into(),
            timeout_secs: 10,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn lists_models_from_the_server() {
        let stub = StubHttp::start(vec![StubHttp::json(
            200,
            r#"{"object":"list","data":[{"id":"llama-3.1-8b"},{"id":"mixtral"}]}"#,
        )])
        .await;

        let models = backend(stub.base_url()).discover_models().await.unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "llama-3.1-8b");
        assert_eq!(models[0].kind, JobKind::Llm);
        assert!(models[0].price.is_none());
    }

    #[tokio::test]
    async fn a_default_price_is_advertised_on_every_model() {
        let stub = StubHttp::start(vec![StubHttp::json(
            200,
            r#"{"object":"list","data":[{"id":"llama-3.1-8b"},{"id":"mixtral"}]}"#,
        )])
        .await;

        let models = priced(stub.base_url(), Some(0.15), Default::default())
            .discover_models()
            .await
            .unwrap();
        assert_eq!(models[0].amount(), 0.15);
        assert_eq!(models[1].amount(), 0.15);
    }

    #[tokio::test]
    async fn a_per_model_price_overrides_the_default() {
        let stub = StubHttp::start(vec![StubHttp::json(
            200,
            r#"{"object":"list","data":[{"id":"llama-3.1-8b"},{"id":"mixtral"}]}"#,
        )])
        .await;

        let mut prices = std::collections::BTreeMap::new();
        prices.insert("mixtral".into(), 0.40);
        let models = priced(stub.base_url(), Some(0.15), prices)
            .discover_models()
            .await
            .unwrap();
        assert_eq!(models[0].id, "llama-3.1-8b");
        assert_eq!(models[0].amount(), 0.15);
        assert_eq!(models[1].id, "mixtral");
        assert_eq!(models[1].amount(), 0.40);
    }

    #[tokio::test]
    async fn tokens_are_forwarded_as_they_arrive() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"hmm\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"yes\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let stub = StubHttp::start(vec![StubHttp::sse(sse)]).await;
        let (progress_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let (token_tx, mut token_rx) = tokio::sync::mpsc::unbounded_channel();

        backend(stub.base_url())
            .run(
                Uuid::nil(),
                &JobPayload::Llm(params()),
                &Progress::new(progress_tx).with_tokens(token_tx),
            )
            .await
            .unwrap();

        let mut texts = Vec::new();
        let mut thinks = Vec::new();
        while let Ok(d) = token_rx.try_recv() {
            if !d.text.is_empty() {
                texts.push(d.text);
            }
            if !d.thinking.is_empty() {
                thinks.push(d.thinking);
            }
        }
        assert_eq!(thinks, ["hmm"]);
        assert_eq!(texts, ["yes"]);
    }

    #[tokio::test]
    async fn streams_a_completion_and_hashes_it() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"a peer\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\" is a node\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let stub = StubHttp::start(vec![StubHttp::sse(sse)]).await;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let result = backend(stub.base_url())
            .run(Uuid::nil(), &JobPayload::Llm(params()), &Progress::new(tx))
            .await
            .unwrap();

        assert_eq!(result.text.as_deref(), Some("a peer is a node"));
        assert_eq!(result.sha256, sha256_hex(b"a peer is a node"));
        assert_eq!(result.meta["finish_reason"], "stop");
        assert_eq!(result.meta["model"], "llama-3.1-8b");

        let mut updates = vec![];
        while let Ok(p) = rx.try_recv() {
            updates.push(p);
        }
        assert!(!updates.is_empty(), "progress was reported");
        assert!(updates.iter().all(|p| *p <= 0.98));
    }

    #[tokio::test]
    async fn a_job_that_names_no_model_uses_and_reports_the_discovered_one() {
        let stub = StubHttp::start(vec![
            StubHttp::json(200, r#"{"data":[{"id":"llama-3.1-8b"}]}"#),
            StubHttp::sse(
                "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n",
            ),
        ])
        .await;
        let backend = backend(stub.base_url());
        backend.discover_models().await.unwrap();

        let mut anonymous = params();
        anonymous.model_id = None;
        let result = backend
            .run(Uuid::nil(), &JobPayload::Llm(anonymous), &Progress::none())
            .await
            .unwrap();

        assert_eq!(
            result.meta["model"], "llama-3.1-8b",
            "the result says which weights actually ran"
        );
        let completion = stub
            .requests()
            .into_iter()
            .find(|r| r.contains("chat/completions"))
            .unwrap();
        assert!(
            completion.contains("llama-3.1-8b"),
            "and the server was told so too"
        );
    }

    #[tokio::test]
    async fn surfaces_the_servers_error_body() {
        let stub = StubHttp::start(vec![StubHttp::json(
            400,
            r#"{"error":{"message":"max_tokens is too large"}}"#,
        )])
        .await;

        let err = backend(stub.base_url())
            .run(Uuid::nil(), &JobPayload::Llm(params()), &Progress::none())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("400"), "got: {err}");
        assert!(err.contains("max_tokens is too large"), "got: {err}");
    }

    #[tokio::test]
    async fn an_empty_completion_is_an_error_not_an_empty_result() {
        let stub = StubHttp::start(vec![StubHttp::sse("data: [DONE]\n\n")]).await;
        assert!(backend(stub.base_url())
            .run(Uuid::nil(), &JobPayload::Llm(params()), &Progress::none())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn token_counts_come_from_the_server_when_it_reports_them() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":31,\"completion_tokens\":7,\"total_tokens\":38}}\n\n",
            "data: [DONE]\n\n",
        );
        let stub = StubHttp::start(vec![StubHttp::sse(sse)]).await;
        let backend = backend(stub.base_url());

        let result = backend
            .run(Uuid::nil(), &JobPayload::Llm(params()), &Progress::none())
            .await
            .unwrap();

        assert_eq!(result.meta["prompt_tokens"], 31);
        assert_eq!(result.meta["completion_tokens"], 7);
        assert_eq!(result.meta["total_tokens"], 38);
        assert_eq!(result.meta["tokens_measured"], true);
        assert_eq!(result.meta["tokenizer"], "openai");

        // And we actually asked for them.
        let sent = stub
            .requests()
            .into_iter()
            .find(|r| r.contains("chat/completions"))
            .unwrap();
        assert!(
            sent.contains("include_usage"),
            "the request opts in: {sent}"
        );
    }

    #[tokio::test]
    async fn a_server_that_reports_no_usage_is_counted_with_the_tokenizer() {
        // Older servers ignore stream_options. The OpenAI tokenizer still
        // produces a billable number — a frame count is not a token count.
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"one\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\" two\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let stub = StubHttp::start(vec![StubHttp::sse(sse)]).await;

        let result = backend(stub.base_url())
            .run(Uuid::nil(), &JobPayload::Llm(params()), &Progress::none())
            .await
            .unwrap();

        let billed = TokenUsage::from_meta(&result.meta).expect("tokenizer filled usage in");
        assert!(billed.prompt > 0, "prompt was {}", billed.prompt);
        assert!(billed.completion > 0, "completion was {}", billed.completion);
        assert_eq!(result.meta["tokens_measured"], true);
        assert_eq!(result.meta["tokenizer"], "openai");
    }

    #[tokio::test]
    async fn an_under_reporting_provider_cannot_shrink_the_bill() {
        // OpenRouter (or a third-party worker) reporting fewer tokens than
        // the tokenizer counts is how we lose money. Take the max.
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hello world\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        );
        let stub = StubHttp::start(vec![StubHttp::sse(sse)]).await;
        let result = backend(stub.base_url())
            .run(Uuid::nil(), &JobPayload::Llm(params()), &Progress::none())
            .await
            .unwrap();

        let billed = TokenUsage::from_meta(&result.meta).unwrap();
        assert!(
            billed.prompt >= 1 && billed.completion >= 1,
            "{billed:?}"
        );
        // "hello world" is 2 o200k tokens; a report of 1 must not win.
        assert!(
            billed.completion >= 2,
            "completion was {}",
            billed.completion
        );
    }

    #[tokio::test]
    async fn cache_and_reasoning_and_cost_are_taken_from_the_provider() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{",
            "\"prompt_tokens\":80,\"completion_tokens\":20,\"total_tokens\":100,",
            "\"prompt_tokens_details\":{\"cached_tokens\":50},",
            "\"completion_tokens_details\":{\"reasoning_tokens\":8},",
            "\"cost\":0.00042}}\n\n",
            "data: [DONE]\n\n",
        );
        let stub = StubHttp::start(vec![StubHttp::sse(sse)]).await;
        let result = backend(stub.base_url())
            .run(Uuid::nil(), &JobPayload::Llm(params()), &Progress::none())
            .await
            .unwrap();

        assert_eq!(result.meta["prompt_tokens"], 80);
        assert_eq!(result.meta["completion_tokens"], 20);
        assert_eq!(result.meta["cached_tokens"], 50);
        assert_eq!(result.meta["reasoning_tokens"], 8);
        assert_eq!(result.meta["upstream_cost"], 0.00042);
    }

    #[tokio::test]
    async fn a_reasoning_model_that_answers_is_not_confused_for_an_empty_one() {
        // DeepSeek-style: thinking first, answer second. Only the answer is
        // the result; the thinking is counted but not returned.
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"let me think about this\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\" some more\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"42\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let stub = StubHttp::start(vec![StubHttp::sse(sse)]).await;

        let result = backend(stub.base_url())
            .run(Uuid::nil(), &JobPayload::Llm(params()), &Progress::none())
            .await
            .unwrap();

        assert_eq!(result.text.as_deref(), Some("42"));
        assert_eq!(
            result.thinking.as_deref(),
            Some("let me think about this some more")
        );
        assert_eq!(
            result.sha256,
            sha256_hex(b"42"),
            "the hash covers the answer only"
        );
        assert_eq!(result.meta["reasoning_chars"], 33);
    }

    #[tokio::test]
    async fn reasoning_that_runs_out_of_room_says_so() {
        // The failure that looked like "empty completion": all the budget went
        // on thinking and the answer never arrived.
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking hard\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let stub = StubHttp::start(vec![StubHttp::sse(sse)]).await;

        let error = backend(stub.base_url())
            .run(Uuid::nil(), &JobPayload::Llm(params()), &Progress::none())
            .await
            .err()
            .unwrap()
            .to_string();

        assert!(error.contains("reasoning"), "got: {error}");
        assert!(error.contains("raise max_tokens"), "got: {error}");
        assert!(
            error.contains("8"),
            "it names the limit that was hit: {error}"
        );
    }

    #[tokio::test]
    async fn an_error_reported_mid_stream_is_surfaced() {
        // A 200 response that goes wrong later must not read as "empty".
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"star\"}}]}\n\n",
            "data: {\"error\":{\"message\":\"engine died: CUDA out of memory\"}}\n\n",
        );
        let stub = StubHttp::start(vec![StubHttp::sse(sse)]).await;

        let error = backend(stub.base_url())
            .run(Uuid::nil(), &JobPayload::Llm(params()), &Progress::none())
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("CUDA out of memory"), "got: {error}");
    }

    #[test]
    fn empty_completion_messages_point_at_the_actual_cause() {
        assert!(empty_completion("thinking", Some("length"), 512).contains("raise max_tokens"));
        assert!(empty_completion("thinking", Some("stop"), 512).contains("no answer"));
        assert!(empty_completion("", Some("length"), 512).contains("max_tokens limit (512)"));
        assert!(empty_completion("", None, 512).contains("empty completion"));
    }

    #[tokio::test]
    async fn a_picture_goes_up_as_a_content_part_beside_the_question() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"a cat\"}}]}\n\ndata: [DONE]\n\n";
        let stub = StubHttp::start(vec![StubHttp::sse(sse)]).await;

        let mut p = params();
        // Raw base64, as a client that just read a file would have it. The
        // leading bytes say PNG.
        p.messages = vec![ChatMessage::new("user", "what is this?")
            .with_images(vec!["iVBORw0KGgoAAAANSUhEUg==".into()])];

        backend(stub.base_url())
            .run(Uuid::new_v4(), &JobPayload::Llm(p), &Progress::none())
            .await
            .unwrap();

        let sent = stub.requests().into_iter().last().unwrap();
        let body: serde_json::Value =
            serde_json::from_str(sent.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        let content = &body["messages"][0]["content"];

        assert!(content.is_array(), "a message with a picture is parts: {content}");
        assert_eq!(content[0]["type"], "image_url");
        // Wrapped as a data URL, with the type read off the bytes rather than
        // guessed, and never fetched from anywhere.
        let url = content[0]["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:image/png;base64,iVBORw0"), "got: {url}");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "what is this?");
    }

    #[tokio::test]
    async fn a_message_without_pictures_is_still_a_plain_string() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        let stub = StubHttp::start(vec![StubHttp::sse(sse)]).await;

        backend(stub.base_url())
            .run(Uuid::new_v4(), &JobPayload::Llm(params()), &Progress::none())
            .await
            .unwrap();

        let sent = stub.requests().into_iter().last().unwrap();
        let body: serde_json::Value =
            serde_json::from_str(sent.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        // Servers that accept only the old shape for system and tool messages
        // keep working.
        assert!(body["messages"][0]["content"].is_string());
    }

    #[test]
    fn a_data_url_is_passed_through_as_it_arrived() {
        let already = "data:image/jpeg;base64,/9j/4AAQ";
        assert_eq!(image_part(already)["image_url"]["url"], already);
        // And the type comes off the bytes for each format we can recognise.
        assert_eq!(media_type("/9j/4AAQSkZJRg"), "image/jpeg");
        assert_eq!(media_type("R0lGODlhAQAB"), "image/gif");
        assert_eq!(media_type("UklGRiQAAABXRUJQ"), "image/webp");
    }

    #[tokio::test]
    async fn a_server_that_wants_max_completion_tokens_is_asked_again() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"pong\"}}]}\n\ndata: [DONE]\n\n";
        let stub = StubHttp::start(vec![
            StubHttp::json(
                400,
                r#"{"error":{"message":"Unsupported parameter: 'max_tokens' is not supported with this model. Use 'max_completion_tokens' instead."}}"#,
            ),
            StubHttp::sse(sse),
        ])
        .await;

        let result = backend(stub.base_url())
            .run(Uuid::new_v4(), &JobPayload::Llm(params()), &Progress::none())
            .await
            .unwrap();
        assert_eq!(result.text.as_deref(), Some("pong"));

        let sent: Vec<String> = stub.requests();
        let second: serde_json::Value =
            serde_json::from_str(sent[1].split("\r\n\r\n").nth(1).unwrap()).unwrap();
        // The limit moved to the new name rather than being dropped, which
        // would leave the generation unbounded.
        assert_eq!(second["max_completion_tokens"], 8);
        assert!(second.get("max_tokens").is_none());
    }

    #[tokio::test]
    async fn a_max_tokens_value_the_server_dislikes_is_not_retried() {
        let stub = StubHttp::start(vec![StubHttp::json(
            400,
            r#"{"error":{"message":"max_tokens is too large: 131072"}}"#,
        )])
        .await;

        let err = backend(stub.base_url())
            .run(Uuid::new_v4(), &JobPayload::Llm(params()), &Progress::none())
            .await
            .unwrap_err()
            .to_string();

        // Retrying under a new name would fail the same way with a worse
        // message, so the real complaint is surfaced.
        assert!(err.contains("too large"), "got: {err}");
        assert_eq!(stub.requests().len(), 1, "asked once");
    }

    fn dsml_invoke(name: &str, param: &str, value: &str) -> String {
        format!(
            "<{DSML_MARK}_tool_calls>\
             <{DSML_MARK}invoke name=\"{name}\">\
             <{DSML_MARK}parameter name=\"{param}\" string=\"true\">{value}</{DSML_MARK}parameter>\
             </{DSML_MARK}invoke>\
             </{DSML_MARK}tool_calls>"
        )
    }

    #[test]
    fn a_malformed_dsml_wrapper_is_still_a_tool_call() {
        // The Flash-0731 leak Codex printed: opening wrapper is `_tool_calls`
        // instead of `tool_calls`, so vLLM's parser misses and the markup
        // arrives as content. The invoke itself is intact.
        let markup = dsml_invoke(
            "exec_command",
            "cmd",
            "sed -n '820,900p' impl/shield/src/tall.rs",
        );
        let (text, calls) = salvage_tool_markup(markup);
        assert!(text.is_empty(), "markup must not remain as the answer: {text}");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "exec_command");
        let args: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["cmd"], "sed -n '820,900p' impl/shield/src/tall.rs");
    }

    #[test]
    fn dsml_after_a_sentence_leaves_the_sentence() {
        let markup = format!("I'll look.\n{}", dsml_invoke("Read", "path", "a.txt"));
        let (text, calls) = salvage_tool_markup(markup);
        assert_eq!(text, "I'll look.");
        assert_eq!(calls[0].name, "Read");
    }

    #[test]
    fn dsml_is_not_visible_in_later_deltas() {
        assert_eq!(markup_visible_delta("", "I'll look."), "I'll look.");
        let markup = dsml_invoke("Read", "path", "a.txt");
        assert_eq!(markup_visible_delta("I'll look.", &markup), "");
        assert_eq!(markup_visible_delta(&format!("I'll look.{markup}"), " more"), "");
        // Crossing the boundary in one chunk: keep the sentence, drop the markup.
        let mixed = format!("I'll look.{markup}");
        assert_eq!(markup_visible_delta("", &mixed), "I'll look.");
    }

    #[test]
    fn qwen_xml_tool_calls_are_salvaged() {
        let markup = concat!(
            "<tool_call>\n",
            "<function=Read>\n",
            "<parameter=path>a.txt</parameter>\n",
            "</function>\n",
            "</tool_call>",
        );
        let (text, calls) = salvage_tool_markup(markup.into());
        assert!(text.is_empty(), "got: {text}");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "Read");
        let args: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["path"], "a.txt");
    }

    #[test]
    fn two_opening_system_messages_become_one() {
        // Qwen's chat template raises "System message must be at the
        // beginning" if *any* system message is not messages[0]. Codex
        // sends `instructions` plus a `developer` turn; both land as
        // system. Merging keeps the text and satisfies the template.
        let out = prepare_messages(&[
            ChatMessage::new("system", "You are Codex."),
            ChatMessage::new("system", "Be terse."),
            ChatMessage::new("user", "hi"),
        ]);
        assert_eq!(
            out.iter().map(|m| m.role.as_str()).collect::<Vec<_>>(),
            ["system", "user"]
        );
        assert!(out[0].content.contains("You are Codex."));
        assert!(out[0].content.contains("Be terse."));
    }

    #[test]
    fn a_system_message_after_a_tool_result_is_not_sent_as_system() {
        let out = prepare_messages(&[
            ChatMessage::new("system", "rules"),
            ChatMessage::new("user", "read it"),
            ChatMessage::new("assistant", ""),
            ChatMessage::new("tool", "file contents"),
            ChatMessage::new("system", "now answer"),
        ]);
        let roles: Vec<&str> = out.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles.iter().filter(|r| **r == "system").count(), 1, "{roles:?}");
        assert!(out.iter().any(|m| m.content.contains("now answer")));
        assert!(out.iter().any(|m| m.content.contains("<system-reminder>")));
    }

    #[tokio::test]
    async fn the_inference_server_sees_one_opening_system_message() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
        let stub = StubHttp::start(vec![StubHttp::sse(sse)]).await;
        let mut p = params();
        p.messages = vec![
            ChatMessage::new("system", "a"),
            ChatMessage::new("system", "b"),
            ChatMessage::new("user", "hi"),
            ChatMessage::new("system", "reminder"),
        ];
        backend(stub.base_url())
            .run(Uuid::nil(), &JobPayload::Llm(p), &Progress::none())
            .await
            .unwrap();
        let sent = stub.requests().into_iter().last().unwrap();
        let body: serde_json::Value =
            serde_json::from_str(sent.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        let roles: Vec<&str> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles[0], "system");
        assert_eq!(
            roles.iter().filter(|r| **r == "system").count(),
            1,
            "Qwen rejects a second system message: {roles:?}"
        );
    }

    #[test]
    fn hermes_json_tool_calls_are_salvaged() {
        let markup = r#"I'll look.
<tool_call>
{"name": "exec_command", "arguments": {"cmd": "ls"}}
</tool_call>"#;
        let (text, calls) = salvage_tool_markup(markup.into());
        assert_eq!(text, "I'll look.");
        assert_eq!(calls[0].name, "exec_command");
        let args: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["cmd"], "ls");
    }
}

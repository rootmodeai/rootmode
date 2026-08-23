//! Turning the two API shapes the world already speaks into rootmode jobs,
//! and rootmode results back into them.
//!
//! Nothing here touches a socket or the database, so the mapping — which is
//! the part that quietly goes wrong — is tested on its own.
//!
//! Two shapes are supported because two shapes is all it takes to cover the
//! tools people actually use:
//!
//! * **Anthropic Messages** (`/v1/messages`) — Claude Code.
//! * **OpenAI chat completions** (`/v1/chat/completions`) — Cursor, Continue,
//!   Cline, Zed, Aider, and roughly everything else.
//!
//! Both collapse to the same [`LlmParams`], because a rootmode worker serves
//! one thing: a chat completion.
//!
//! Tool calls travel end to end. That is not a nicety — an editor whose tool
//! calls are dropped does not degrade, it stops working, because the model
//! answers by calling a tool and nothing is listening. The two dialects
//! disagree about where tool results live (Anthropic puts them in a user
//! turn, OpenAI gives them their own `tool` role), so translating means
//! re-splitting turns, not renaming fields.

use rootmode_core::{ChatMessage, LlmParams, TokenUsage, ToolCall, ToolDef};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What to allow when the client names no limit. OpenAI-shaped clients
/// routinely omit `max_tokens`; Anthropic requires it.
///
/// Generous on purpose. `max_tokens` is a ceiling, not a target: a model that
/// finishes early stops early and the unused budget costs nothing. A ceiling
/// set too low costs everything — a reasoning model spends its budget
/// thinking before it writes a word, and a cap below that floor produces no
/// answer at all rather than a short one.
const DEFAULT_MAX_TOKENS: u32 = 16_384;

/// What both request shapes boil down to.
pub struct ChatRequest {
    pub model: String,
    pub params: LlmParams,
    pub stream: bool,
}

/// What a finished job looks like to either dialect.
#[derive(Debug, Clone, Default)]
pub struct Answer {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    /// What the inference server said ended the generation, verbatim —
    /// `stop`, `length`, `tool_calls`. Absent when it did not say.
    pub finish: Option<String>,
}

/// Pull reasoning tags out of a completion so a coding client never sees them
/// as the answer. Reasoning models write those into `content` when the
/// inference server has no parser; Claude Code then treats the monologue as
/// the reply and the session falls apart.
pub fn peel_thinking(input: &str) -> String {
    let mut out = input.to_string();
    for (open, close) in [("<think>", "</think>"), ("<thinking>", "</thinking>")] {
        loop {
            let Some(start) = find_ignore_ascii_case(&out, open) else {
                break;
            };
            let after = start + open.len();
            match find_ignore_ascii_case(&out[after..], close) {
                Some(rel) => {
                    let end = after + rel + close.len();
                    out.replace_range(start..end, "");
                }
                None => {
                    out.truncate(start);
                    break;
                }
            }
        }
    }
    collapse_blank_lines(&out)
}

fn find_ignore_ascii_case(hay: &str, needle: &str) -> Option<usize> {
    hay.as_bytes()
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

fn collapse_blank_lines(s: &str) -> String {
    let mut out = String::new();
    let mut blank = 0u8;
    for line in s.lines() {
        if line.trim().is_empty() {
            blank = blank.saturating_add(1);
            if blank <= 1 && !out.is_empty() {
                out.push('\n');
            }
        } else {
            blank = 0;
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(line.trim_end());
        }
    }
    out.trim().to_string()
}

impl Answer {
    /// True when the model ran out of room rather than finishing.
    ///
    /// Reporting this as a normal ending is the worst kind of wrong: the
    /// client shows a half-sentence as though it were the whole answer, and
    /// nobody can tell the difference from the outside.
    fn truncated(&self) -> bool {
        self.finish.as_deref() == Some("length")
    }
}

impl Answer {
    fn acted(&self) -> bool {
        !self.tool_calls.is_empty()
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct BadRequest(pub String);

type Result<T> = std::result::Result<T, BadRequest>;

// --------------------------------------------------------------- anthropic in

#[derive(Debug, Deserialize)]
pub struct AnthropicRequest {
    pub model: String,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub system: Option<Value>,
    #[serde(default)]
    pub messages: Vec<AnthropicMessage>,
    #[serde(default)]
    pub tools: Vec<Value>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub temperature: Option<f32>,
}

#[derive(Debug, Deserialize)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: Value,
}

impl AnthropicRequest {
    pub fn into_chat(self) -> Result<ChatRequest> {
        let mut messages: Vec<ChatMessage> = Vec::new();

        if let Some(system) = &self.system {
            let text = flatten_text(system);
            if !text.trim().is_empty() {
                messages.push(ChatMessage::new("system", text));
            }
        }

        for m in &self.messages {
            push_anthropic_turn(&mut messages, &m.role, &m.content);
        }

        if !messages.iter().any(|m| {
            m.role != "system" && (!m.content.trim().is_empty() || !m.tool_calls.is_empty())
        }) {
            return Err(BadRequest("request has no user or assistant turns".into()));
        }

        fold_late_system_messages(&mut messages);

        Ok(ChatRequest {
            model: self.model.clone(),
            params: LlmParams {
                model_hash: None,
                model_id: Some(self.model),
                messages,
                tools: self.tools.iter().filter_map(anthropic_tool).collect(),
                max_tokens: self.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
                temperature: self.temperature.unwrap_or(0.0),
            },
            stream: self.stream,
        })
    }
}

fn anthropic_tool(t: &Value) -> Option<ToolDef> {
    // Anthropic's server-side tools (`web_search`, `computer`, …) carry a
    // `type` and no schema. A rootmode worker cannot run them, and passing a
    // schema-less tool to vLLM is a bad request, so they are left out.
    let name = t.get("name").and_then(Value::as_str)?;
    let schema = t.get("input_schema")?.clone();
    if !schema.is_object() {
        return None;
    }
    Some(ToolDef {
        name: name.to_string(),
        description: t
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        input_schema: schema,
    })
}

/// Split one Anthropic turn into the messages an OpenAI-shaped server needs.
///
/// The awkward case is a user turn holding tool results: OpenAI wants each of
/// those as its own `tool` message *before* any user text, so one turn in can
/// be several out.
fn push_anthropic_turn(out: &mut Vec<ChatMessage>, role: &str, content: &Value) {
    let blocks = match content {
        Value::Array(b) => b.clone(),
        // A bare string is a plain text turn.
        other => vec![serde_json::json!({ "type": "text", "text": flatten_text(other) })],
    };

    let mut text = String::new();
    let mut calls: Vec<ToolCall> = Vec::new();

    for block in &blocks {
        match block.get("type").and_then(Value::as_str).unwrap_or("text") {
            "text" => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    if !t.is_empty() {
                        if !text.is_empty() {
                            text.push_str("\n\n");
                        }
                        text.push_str(t);
                    }
                }
            }
            "tool_use" => {
                calls.push(ToolCall {
                    id: block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_string(),
                    arguments: block
                        .get("input")
                        .map(|i| i.to_string())
                        .unwrap_or_else(|| "{}".into()),
                });
            }
            "tool_result" => {
                let body = block.get("content").map(flatten_text).unwrap_or_default();
                out.push(ChatMessage {
                    role: "tool".into(),
                    content: body,
                    tool_calls: Vec::new(),
                    tool_call_id: block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    images: Vec::new(),
                });
            }
            // Pictures are carried alongside the text, for a model that can
            // see. Thinking blocks and anything a newer client invents have
            // nothing to carry.
            _ => {}
        }
    }

    let images = images_in(content);
    if !text.is_empty() || !calls.is_empty() || !images.is_empty() {
        out.push(ChatMessage {
            role: role.to_string(),
            content: text,
            tool_calls: calls,
            tool_call_id: None,
            images,
        });
    }
}

/// Move any system instruction that is not the opening one into a user turn.
///
/// Anthropic lets an operator drop a `system` message into the middle of a
/// conversation, and Claude Code leans on it heavily — a reminder after a
/// tool result, a mode switch mid-session. Chat templates on the other side
/// generally accept a system message in first position only; DeepSeek's
/// answers a misplaced one with an empty generation, which surfaces as "the
/// server returned an empty completion" and is impossible to diagnose from
/// the outside.
///
/// Dropping the text is not an option — it is an instruction the user meant.
/// So it is folded into the nearest user turn, tagged, keeping its position
/// so a late instruction still reads as late.
fn fold_late_system_messages(messages: &mut Vec<ChatMessage>) {
    // A leading run of system messages is legal and stays where it is.
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

        let text = wrap_system(&messages.remove(i).content);

        // Prefer the turn just before, so the instruction lands with the
        // context it was written about; fall back to the one after.
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

fn wrap_system(text: &str) -> String {
    // The same marker Claude Code uses for injected context, so a model that
    // has seen the convention reads it as an instruction rather than as
    // something the user typed.
    format!("<system-reminder>\n{}\n</system-reminder>", text.trim())
}

/// Text content only — used for system prompts and tool-result bodies, which
/// are both "whatever this says, as a string".
fn flatten_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| {
                if b.get("type").and_then(Value::as_str) == Some("text") {
                    b.get("text").and_then(Value::as_str).map(str::to_string)
                } else if b.is_string() {
                    b.as_str().map(str::to_string)
                } else {
                    None
                }
            })
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Pictures out of a content array, in whichever dialect they arrived in.
///
/// Anthropic sends `{"type":"image","source":{"type":"base64","data":…}}`;
/// OpenAI sends `{"type":"image_url","image_url":{"url":"data:…"}}`. Both end
/// up as something the worker can put in front of a vision model without
/// fetching anything.
///
/// A `url` source that is not a `data:` URL is dropped rather than passed on:
/// following it would have the worker request a location a stranger chose.
fn images_in(content: &Value) -> Vec<String> {
    let Value::Array(blocks) = content else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter_map(|b| match b.get("type").and_then(Value::as_str) {
            Some("image") => {
                let source = b.get("source")?;
                match source.get("type").and_then(Value::as_str) {
                    Some("base64") => {
                        let data = source.get("data").and_then(Value::as_str)?;
                        let media = source
                            .get("media_type")
                            .and_then(Value::as_str)
                            .unwrap_or("image/png");
                        Some(format!("data:{media};base64,{data}"))
                    }
                    _ => source
                        .get("url")
                        .and_then(Value::as_str)
                        .filter(|u| u.starts_with("data:"))
                        .map(str::to_string),
                }
            }
            Some("image_url") => b
                .pointer("/image_url/url")
                .and_then(Value::as_str)
                .filter(|u| u.starts_with("data:"))
                .map(str::to_string),
            _ => None,
        })
        .collect()
}

// -------------------------------------------------------------- anthropic out

/// The `content` array of an Anthropic reply: text first, then any tool calls.
fn anthropic_content(answer: &Answer) -> Vec<Value> {
    let mut blocks: Vec<Value> = Vec::new();
    if !answer.text.is_empty() {
        blocks.push(serde_json::json!({ "type": "text", "text": answer.text }));
    }
    for call in &answer.tool_calls {
        blocks.push(serde_json::json!({
            "type": "tool_use",
            "id": call.id,
            "name": call.name,
            // Anthropic wants a parsed object. A model can emit arguments that
            // are not valid JSON; sending the raw string under a key the
            // client can still read beats failing the whole turn.
            "input": parse_arguments(&call.arguments),
        }));
    }
    blocks
}

fn parse_arguments(arguments: &str) -> Value {
    serde_json::from_str(arguments)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| serde_json::json!({ "_raw": arguments }))
}

fn stop_reason(answer: &Answer) -> &'static str {
    if answer.acted() {
        "tool_use"
    } else if answer.truncated() {
        "max_tokens"
    } else {
        "end_turn"
    }
}

/// A finished Anthropic response body.
pub fn anthropic_message(id: &str, model: &str, answer: &Answer) -> Value {
    serde_json::json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": anthropic_content(answer),
        "stop_reason": stop_reason(answer),
        "stop_sequence": null,
        "usage": answer.usage.anthropic_json(),
    })
}

/// The SSE events for one response, in order.
///
/// The worker hands us the whole answer at once, so each block's content
/// arrives as a single delta rather than token by token. The event sequence
/// is the real one, which is what clients parse against.
pub fn anthropic_stream(id: &str, model: &str, answer: &Answer) -> Vec<(String, Value)> {
    let mut events = vec![(
        "message_start".into(),
        serde_json::json!({
            "type": "message_start",
            "message": {
                "id": id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": answer.usage.input,
                    "output_tokens": 0,
                    "cache_read_input_tokens": answer.usage.cached,
                },
            }
        }),
    )];

    let mut index = 0usize;

    if !answer.text.is_empty() {
        events.push((
            "content_block_start".into(),
            serde_json::json!({
                "type": "content_block_start",
                "index": index,
                "content_block": { "type": "text", "text": "" }
            }),
        ));
        events.push((
            "content_block_delta".into(),
            serde_json::json!({
                "type": "content_block_delta",
                "index": index,
                "delta": { "type": "text_delta", "text": answer.text }
            }),
        ));
        events.push((
            "content_block_stop".into(),
            serde_json::json!({ "type": "content_block_stop", "index": index }),
        ));
        index += 1;
    }

    for call in &answer.tool_calls {
        events.push((
            "content_block_start".into(),
            serde_json::json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {
                    "type": "tool_use",
                    "id": call.id,
                    "name": call.name,
                    "input": {}
                }
            }),
        ));
        // The arguments go over as a JSON *string* here, unlike the
        // non-streaming body — that is the shape clients accumulate.
        events.push((
            "content_block_delta".into(),
            serde_json::json!({
                "type": "content_block_delta",
                "index": index,
                "delta": { "type": "input_json_delta", "partial_json": call.arguments }
            }),
        ));
        events.push((
            "content_block_stop".into(),
            serde_json::json!({ "type": "content_block_stop", "index": index }),
        ));
        index += 1;
    }

    events.push((
        "message_delta".into(),
        serde_json::json!({
            "type": "message_delta",
            "delta": { "stop_reason": stop_reason(answer), "stop_sequence": null },
            "usage": { "output_tokens": answer.usage.output }
        }),
    ));
    events.push((
        "message_stop".into(),
        serde_json::json!({ "type": "message_stop" }),
    ));
    events
}

pub fn anthropic_error(kind: &str, message: &str) -> Value {
    serde_json::json!({
        "type": "error",
        "error": { "type": kind, "message": message }
    })
}

// ------------------------------------------------------------------ openai in

#[derive(Debug, Deserialize)]
pub struct OpenAiRequest {
    pub model: String,
    #[serde(default)]
    pub messages: Vec<OpenAiMessage>,
    #[serde(default)]
    pub tools: Vec<Value>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Newer clients send this instead; same meaning for our purposes.
    #[serde(default)]
    pub max_completion_tokens: Option<u32>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub temperature: Option<f32>,
}

#[derive(Debug, Deserialize)]
pub struct OpenAiMessage {
    pub role: String,
    #[serde(default)]
    pub content: Value,
    #[serde(default)]
    pub tool_calls: Vec<Value>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
}

impl OpenAiRequest {
    pub fn into_chat(self) -> Result<ChatRequest> {
        let messages: Vec<ChatMessage> = self
            .messages
            .iter()
            .map(|m| ChatMessage {
                role: m.role.clone(),
                content: flatten_text(&m.content),
                images: images_in(&m.content),
                tool_calls: m
                    .tool_calls
                    .iter()
                    .map(|c| ToolCall {
                        id: c
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        name: c
                            .pointer("/function/name")
                            .and_then(Value::as_str)
                            .unwrap_or("tool")
                            .to_string(),
                        arguments: c
                            .pointer("/function/arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("{}")
                            .to_string(),
                    })
                    .collect(),
                tool_call_id: m.tool_call_id.clone(),
            })
            .filter(|m| !m.content.trim().is_empty() || !m.tool_calls.is_empty())
            .collect();

        if !messages.iter().any(|m| m.role != "system") {
            return Err(BadRequest("request has no user or assistant turns".into()));
        }

        let mut messages = messages;
        fold_late_system_messages(&mut messages);

        Ok(ChatRequest {
            model: self.model.clone(),
            params: LlmParams {
                model_hash: None,
                model_id: Some(self.model),
                messages,
                tools: self.tools.iter().filter_map(openai_tool).collect(),
                max_tokens: self
                    .max_tokens
                    .or(self.max_completion_tokens)
                    .unwrap_or(DEFAULT_MAX_TOKENS),
                temperature: self.temperature.unwrap_or(0.0),
            },
            stream: self.stream,
        })
    }
}

fn openai_tool(t: &Value) -> Option<ToolDef> {
    let f = t.get("function")?;
    Some(ToolDef {
        name: f.get("name").and_then(Value::as_str)?.to_string(),
        description: f
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        input_schema: f
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({ "type": "object" })),
    })
}

// ----------------------------------------------------------------- openai out

fn openai_tool_calls(answer: &Answer) -> Vec<Value> {
    answer
        .tool_calls
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id,
                "type": "function",
                "function": { "name": c.name, "arguments": c.arguments },
            })
        })
        .collect()
}

fn openai_finish(answer: &Answer) -> &'static str {
    if answer.acted() {
        "tool_calls"
    } else if answer.truncated() {
        "length"
    } else {
        "stop"
    }
}

pub fn openai_completion(id: &str, model: &str, answer: &Answer, created: i64) -> Value {
    let mut message = serde_json::json!({ "role": "assistant", "content": answer.text });
    if answer.acted() {
        message["tool_calls"] = Value::Array(openai_tool_calls(answer));
        // A tool-calling turn has no prose; `null` is what clients expect
        // there, and an empty string trips some of them up.
        if answer.text.is_empty() {
            message["content"] = Value::Null;
        }
    }

    serde_json::json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{ "index": 0, "message": message, "finish_reason": openai_finish(answer) }],
        "usage": answer.usage.openai_json(),
    })
}

/// The chunks for a streamed completion. The caller still has to send the
/// `[DONE]` sentinel, which is not JSON and so cannot live in this list.
pub fn openai_stream(id: &str, model: &str, answer: &Answer, created: i64) -> Vec<Value> {
    let chunk = |delta: Value, finish: Value| {
        serde_json::json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        })
    };

    let mut out = vec![chunk(
        serde_json::json!({ "role": "assistant", "content": "" }),
        Value::Null,
    )];
    if !answer.text.is_empty() {
        out.push(chunk(
            serde_json::json!({ "content": answer.text }),
            Value::Null,
        ));
    }
    if answer.acted() {
        // Streamed tool calls carry the index they belong to, so a client can
        // assemble them the same way it would from a real server.
        let calls: Vec<Value> = answer
            .tool_calls
            .iter()
            .enumerate()
            .map(|(i, c)| {
                serde_json::json!({
                    "index": i,
                    "id": c.id,
                    "type": "function",
                    "function": { "name": c.name, "arguments": c.arguments },
                })
            })
            .collect();
        out.push(chunk(
            serde_json::json!({ "tool_calls": calls }),
            Value::Null,
        ));
    }
    out.push(chunk(
        serde_json::json!({}),
        Value::String(openai_finish(answer).into()),
    ));
    // Final usage chunk, the same shape OpenAI sends when the client asked
    // for `stream_options.include_usage`. Without it, streaming clients
    // never see a token count and cannot bill the turn.
    out.push(serde_json::json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [],
        "usage": answer.usage.openai_json(),
    }));
    out
}

pub fn openai_error(kind: &str, message: &str) -> Value {
    serde_json::json!({
        "error": { "type": kind, "message": message, "code": kind }
    })
}

// ------------------------------------------------------------- responses in
//
// OpenAI's Responses API (`/v1/responses`) — Codex's only remaining wire
// format; it dropped `chat.completions` support in mid-2026. Structurally
// this is chat completions with the messages array renamed to `input`, item
// types spelled out (`message`, `function_call`, `function_call_output`
// instead of a `role` doing all the work), and a top-level `instructions`
// string standing in for a system message. It still collapses to the same
// [`LlmParams`], because Codex still only ever wants a chat completion.

#[derive(Debug, Deserialize)]
pub struct ResponsesRequest {
    pub model: String,
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub tools: Vec<Value>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub temperature: Option<f32>,
}

/// Responses API content blocks use `input_text` / `output_text` where chat
/// completions uses plain `text` — otherwise the same shape [`flatten_text`]
/// already handles, so this only has to cover the renamed type.
fn flatten_responses_text(content: &Value) -> String {
    match content {
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                Some("input_text") | Some("output_text") | Some("text") => {
                    b.get("text").and_then(Value::as_str).map(str::to_string)
                }
                _ if b.is_string() => b.as_str().map(str::to_string),
                _ => None,
            })
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        other => flatten_text(other),
    }
}

/// `input_image` blocks carry the data URL directly in `image_url`, not
/// nested under `image_url.url` the way chat completions nests it.
fn responses_images_in(content: &Value) -> Vec<String> {
    let Value::Array(blocks) = content else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter_map(|b| {
            if b.get("type").and_then(Value::as_str) != Some("input_image") {
                return None;
            }
            b.get("image_url")
                .and_then(Value::as_str)
                .filter(|u| u.starts_with("data:"))
                .map(str::to_string)
        })
        .collect()
}

/// Flat `{type:"function", name, description, parameters}` — Responses tools
/// have no nested `function` object the way chat completions tools do.
fn responses_tool(t: &Value) -> Option<ToolDef> {
    if t.get("type").and_then(Value::as_str) != Some("function") {
        return None;
    }
    Some(ToolDef {
        name: t.get("name").and_then(Value::as_str)?.to_string(),
        description: t.get("description").and_then(Value::as_str).map(str::to_string),
        input_schema: t
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({ "type": "object" })),
    })
}

impl ResponsesRequest {
    pub fn into_chat(self) -> Result<ChatRequest> {
        let mut messages: Vec<ChatMessage> = Vec::new();

        if let Some(instructions) = &self.instructions {
            if !instructions.trim().is_empty() {
                messages.push(ChatMessage::new("system", instructions.clone()));
            }
        }

        match &self.input {
            Value::String(s) => messages.push(ChatMessage::new("user", s.clone())),
            Value::Array(items) => {
                for item in items {
                    push_responses_item(&mut messages, item);
                }
            }
            _ => {}
        }

        if !messages.iter().any(|m| m.role != "system") {
            return Err(BadRequest("request has no input".into()));
        }

        fold_late_system_messages(&mut messages);

        Ok(ChatRequest {
            model: self.model.clone(),
            params: LlmParams {
                model_hash: None,
                model_id: Some(self.model),
                messages,
                tools: self.tools.iter().filter_map(responses_tool).collect(),
                max_tokens: self.max_output_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
                temperature: self.temperature.unwrap_or(0.0),
            },
            stream: self.stream,
        })
    }
}

fn push_responses_item(out: &mut Vec<ChatMessage>, item: &Value) {
    match item.get("type").and_then(Value::as_str) {
        // A `message` item with no `type` at all is also a message — Codex
        // omits the field on the common case.
        Some("message") | None => {
            let role = item
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user");
            // Responses has a `developer` role where chat completions has
            // `system`; both mean instructions the model should follow.
            let role = if role == "developer" { "system" } else { role };
            let content = item.get("content").cloned().unwrap_or(Value::Null);
            let text = flatten_responses_text(&content);
            let images = responses_images_in(&content);
            if text.is_empty() && images.is_empty() {
                return;
            }
            out.push(ChatMessage::new(role, text).with_images(images));
        }
        Some("function_call") => {
            let id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string();
            let arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}")
                .to_string();
            out.push(ChatMessage {
                role: "assistant".into(),
                content: String::new(),
                tool_calls: vec![ToolCall { id, name, arguments }],
                tool_call_id: None,
                images: Vec::new(),
            });
        }
        Some("function_call_output") => {
            let id = item
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let output = match item.get("output") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => flatten_responses_text(other),
                None => String::new(),
            };
            out.push(ChatMessage {
                role: "tool".into(),
                content: output,
                tool_calls: Vec::new(),
                tool_call_id: Some(id),
                images: Vec::new(),
            });
        }
        // `reasoning` and anything else from a newer client: no text content
        // to feed the model, and nothing here relies on it.
        _ => {}
    }
}

// ------------------------------------------------------------ responses out

fn responses_output_items(id: &str, answer: &Answer) -> Vec<Value> {
    let mut items = Vec::new();
    if !answer.text.is_empty() || !answer.acted() {
        items.push(serde_json::json!({
            "type": "message",
            "id": format!("msg_{id}"),
            "status": "completed",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": answer.text, "annotations": [] }],
        }));
    }
    for (i, call) in answer.tool_calls.iter().enumerate() {
        items.push(serde_json::json!({
            "type": "function_call",
            "id": format!("fc_{id}_{i}"),
            "call_id": call.id,
            "name": call.name,
            "arguments": call.arguments,
            "status": "completed",
        }));
    }
    items
}

fn responses_status(answer: &Answer) -> &'static str {
    if answer.truncated() {
        "incomplete"
    } else {
        "completed"
    }
}

pub fn openai_response(id: &str, model: &str, answer: &Answer, created: i64) -> Value {
    serde_json::json!({
        "id": id,
        "object": "response",
        "created_at": created,
        "status": responses_status(answer),
        "model": model,
        "output": responses_output_items(id, answer),
        "output_text": answer.text,
        "usage": {
            "input_tokens": answer.usage.input,
            "output_tokens": answer.usage.output,
            "total_tokens": answer.usage.input + answer.usage.output,
            "input_tokens_details": { "cached_tokens": answer.usage.cached },
            "output_tokens_details": { "reasoning_tokens": answer.usage.reasoning },
        },
    })
}

fn responses_skeleton(id: &str, model: &str, created: i64, status: &str) -> Value {
    serde_json::json!({
        "id": id,
        "object": "response",
        "created_at": created,
        "status": status,
        "error": null,
        "model": model,
        "output": [],
    })
}

fn responses_message_id(id: &str) -> String {
    format!("msg_{id}")
}

fn responses_function_id(id: &str, i: usize) -> String {
    format!("fc_{id}_{i}")
}

fn responses_text_part(text: &str) -> Value {
    serde_json::json!({ "type": "output_text", "text": text, "annotations": [] })
}

/// `(event name, payload)` pairs for a streamed response, including the
/// `output_item.added` / `content_part.added` frame Codex requires before
/// it will accept any text delta. The live path emits the same sequence
/// incrementally; this is the one-shot form for tests and anything that
/// already has the finished answer.
pub fn openai_response_stream(id: &str, model: &str, answer: &Answer, created: i64) -> Vec<(String, Value)> {
    let mut events = vec![
        responses_live_start(id, model, created),
        responses_live_in_progress(id, model, created),
    ];
    if !answer.text.is_empty() {
        events.extend(responses_live_text_start(id));
        events.push(responses_live_delta(id, &answer.text));
    }
    events.extend(responses_live_end(
        id,
        model,
        answer,
        created,
        !answer.text.is_empty(),
    ));
    events
}

pub fn responses_error(kind: &str, message: &str) -> Value {
    serde_json::json!({
        "error": { "type": kind, "message": message, "code": kind }
    })
}

// --------------------------------------------------------------------- live
//
// True incremental variants of the `*_stream` builders above: one frame per
// token as a worker actually sends them, instead of the whole answer
// packaged as a single "stream" after the fact. The latter is not just less
// pleasant to watch type out — a client that expects the first byte within
// some short window (Codex among them) gives up and disconnects long before
// a cold GPU has finished loading a model, and the answer that eventually
// arrives lands nowhere. Starting the frame as soon as the job is submitted,
// then forwarding each delta as it's typed, keeps the connection visibly
// alive the whole time instead of silent until the very end.
//
// The non-incremental builders above are still what closes out a live
// stream — `answer` here is the same completed [`Answer`] `run` builds, so
// the tool-call and usage framing does not need a second implementation.

pub fn anthropic_live_start(id: &str, model: &str) -> (String, Value) {
    (
        "message_start".into(),
        serde_json::json!({
            "type": "message_start",
            "message": {
                "id": id, "type": "message", "role": "assistant", "model": model,
                "content": [], "stop_reason": null, "stop_sequence": null,
                "usage": { "input_tokens": 0, "output_tokens": 0 },
            }
        }),
    )
}

pub fn anthropic_live_text_start(index: usize) -> (String, Value) {
    (
        "content_block_start".into(),
        serde_json::json!({
            "type": "content_block_start", "index": index,
            "content_block": { "type": "text", "text": "" },
        }),
    )
}

pub fn anthropic_live_text_delta(index: usize, text: &str) -> (String, Value) {
    (
        "content_block_delta".into(),
        serde_json::json!({
            "type": "content_block_delta", "index": index,
            "delta": { "type": "text_delta", "text": text },
        }),
    )
}

/// Everything after the text block: its close (when one was ever opened —
/// a tool-only turn has none), any tool calls, and the message's own close.
pub fn anthropic_live_end(answer: &Answer, text_index: Option<usize>) -> Vec<(String, Value)> {
    let mut events = Vec::new();
    let mut index = text_index.map_or(0, |i| i + 1);
    if let Some(i) = text_index {
        events.push((
            "content_block_stop".into(),
            serde_json::json!({ "type": "content_block_stop", "index": i }),
        ));
    }
    for call in &answer.tool_calls {
        events.push((
            "content_block_start".into(),
            serde_json::json!({
                "type": "content_block_start", "index": index,
                "content_block": { "type": "tool_use", "id": call.id, "name": call.name, "input": {} },
            }),
        ));
        events.push((
            "content_block_delta".into(),
            serde_json::json!({
                "type": "content_block_delta", "index": index,
                "delta": { "type": "input_json_delta", "partial_json": call.arguments },
            }),
        ));
        events.push((
            "content_block_stop".into(),
            serde_json::json!({ "type": "content_block_stop", "index": index }),
        ));
        index += 1;
    }
    events.push((
        "message_delta".into(),
        serde_json::json!({
            "type": "message_delta",
            "delta": { "stop_reason": stop_reason(answer), "stop_sequence": null },
            "usage": { "output_tokens": answer.usage.output },
        }),
    ));
    events.push(("message_stop".into(), serde_json::json!({ "type": "message_stop" })));
    events
}

pub fn anthropic_live_error(message: &str) -> (String, Value) {
    ("error".into(), anthropic_error("overloaded_error", message))
}

pub fn openai_live_start(id: &str, model: &str, created: i64) -> Value {
    serde_json::json!({
        "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
        "choices": [{ "index": 0, "delta": { "role": "assistant", "content": "" }, "finish_reason": null }],
    })
}

pub fn openai_live_delta(id: &str, model: &str, created: i64, text: &str) -> Value {
    serde_json::json!({
        "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
        "choices": [{ "index": 0, "delta": { "content": text }, "finish_reason": null }],
    })
}

pub fn openai_live_end(id: &str, model: &str, created: i64, answer: &Answer) -> Vec<Value> {
    let chunk = |delta: Value, finish: Value| {
        serde_json::json!({
            "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        })
    };
    let mut out = Vec::new();
    if answer.acted() {
        let calls: Vec<Value> = answer
            .tool_calls
            .iter()
            .enumerate()
            .map(|(i, c)| {
                serde_json::json!({
                    "index": i, "id": c.id, "type": "function",
                    "function": { "name": c.name, "arguments": c.arguments },
                })
            })
            .collect();
        out.push(chunk(serde_json::json!({ "tool_calls": calls }), Value::Null));
    }
    out.push(chunk(serde_json::json!({}), Value::String(openai_finish(answer).into())));
    out.push(serde_json::json!({
        "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
        "choices": [],
        "usage": answer.usage.openai_json(),
    }));
    out
}

pub fn openai_live_error(message: &str) -> Value {
    openai_error("api_error", message)
}

pub fn responses_live_start(id: &str, model: &str, created: i64) -> (String, Value) {
    (
        "response.created".into(),
        serde_json::json!({
            "type": "response.created",
            "response": responses_skeleton(id, model, created, "in_progress"),
        }),
    )
}

pub fn responses_live_in_progress(id: &str, model: &str, created: i64) -> (String, Value) {
    (
        "response.in_progress".into(),
        serde_json::json!({
            "type": "response.in_progress",
            "response": responses_skeleton(id, model, created, "in_progress"),
        }),
    )
}

/// Open the assistant message item and its text part. Codex logs
/// `OutputTextDelta without active item` and discards every subsequent
/// delta if this pair is missing — the GPU still runs, the answer still
/// arrives, and the editor shows nothing.
pub fn responses_live_text_start(id: &str) -> Vec<(String, Value)> {
    let item_id = responses_message_id(id);
    vec![
        (
            "response.output_item.added".into(),
            serde_json::json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "id": item_id,
                    "type": "message",
                    "status": "in_progress",
                    "role": "assistant",
                    "content": [],
                },
            }),
        ),
        (
            "response.content_part.added".into(),
            serde_json::json!({
                "type": "response.content_part.added",
                "item_id": item_id,
                "output_index": 0,
                "content_index": 0,
                "part": responses_text_part(""),
            }),
        ),
    ]
}

pub fn responses_live_delta(id: &str, text: &str) -> (String, Value) {
    (
        "response.output_text.delta".into(),
        serde_json::json!({
            "type": "response.output_text.delta",
            "item_id": responses_message_id(id),
            "output_index": 0,
            "content_index": 0,
            "delta": text,
        }),
    )
}

fn responses_live_text_close(id: &str, text: &str) -> Vec<(String, Value)> {
    let item_id = responses_message_id(id);
    let part = responses_text_part(text);
    vec![
        (
            "response.output_text.done".into(),
            serde_json::json!({
                "type": "response.output_text.done",
                "item_id": item_id,
                "output_index": 0,
                "content_index": 0,
                "text": text,
            }),
        ),
        (
            "response.content_part.done".into(),
            serde_json::json!({
                "type": "response.content_part.done",
                "item_id": item_id,
                "output_index": 0,
                "content_index": 0,
                "part": part,
            }),
        ),
        (
            "response.output_item.done".into(),
            serde_json::json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "id": item_id,
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [responses_text_part(text)],
                },
            }),
        ),
    ]
}

fn responses_live_function_call(id: &str, i: usize, output_index: usize, call: &ToolCall) -> Vec<(String, Value)> {
    let item_id = responses_function_id(id, i);
    vec![
        (
            "response.output_item.added".into(),
            serde_json::json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": {
                    "type": "function_call",
                    "id": item_id,
                    "call_id": call.id,
                    "name": call.name,
                    "arguments": "",
                    "status": "in_progress",
                },
            }),
        ),
        (
            "response.function_call_arguments.delta".into(),
            serde_json::json!({
                "type": "response.function_call_arguments.delta",
                "item_id": item_id,
                "output_index": output_index,
                "delta": call.arguments,
            }),
        ),
        (
            "response.function_call_arguments.done".into(),
            serde_json::json!({
                "type": "response.function_call_arguments.done",
                "item_id": item_id,
                "output_index": output_index,
                "arguments": call.arguments,
            }),
        ),
        (
            "response.output_item.done".into(),
            serde_json::json!({
                "type": "response.output_item.done",
                "output_index": output_index,
                "item": {
                    "type": "function_call",
                    "id": item_id,
                    "call_id": call.id,
                    "name": call.name,
                    "arguments": call.arguments,
                    "status": "completed",
                },
            }),
        ),
    ]
}

/// Close a previously opened text item (or open-dump-close one that only
/// arrived with the finished result), emit any tool calls with the right
/// `output_index`, and finish with `response.completed`.
///
/// `text_opened` is whether [`responses_live_text_start`] already ran
/// during the live stream. An older worker that only sends the result
/// never produces a delta, so this path still opens the item before
/// dumping the text — otherwise Codex has nothing to attach it to.
pub fn responses_live_end(
    id: &str,
    model: &str,
    answer: &Answer,
    created: i64,
    text_opened: bool,
) -> Vec<(String, Value)> {
    let mut events = Vec::new();
    let mut next_index = 0usize;

    if text_opened {
        events.extend(responses_live_text_close(id, &answer.text));
        next_index = 1;
    } else if !answer.text.is_empty() {
        events.extend(responses_live_text_start(id));
        events.push(responses_live_delta(id, &answer.text));
        events.extend(responses_live_text_close(id, &answer.text));
        next_index = 1;
    }

    for (i, call) in answer.tool_calls.iter().enumerate() {
        events.extend(responses_live_function_call(id, i, next_index + i, call));
    }

    events.push((
        "response.completed".into(),
        serde_json::json!({
            "type": "response.completed",
            "response": openai_response(id, model, answer, created),
        }),
    ));
    events
}

pub fn responses_live_error(message: &str) -> (String, Value) {
    (
        "response.failed".into(),
        serde_json::json!({
            "type": "response.failed",
            "response": { "status": "failed", "error": { "message": message } },
        }),
    )
}

// ----------------------------------------------------------------------- misc

/// Token counts billed for a completion.
///
/// Independently counted with the OpenAI tokenizer, then raised to whatever
/// the worker reported when that is higher — so an untrusted or OpenRouter
/// worker cannot shrink the bill below what we can measure. `cached` is the
/// one figure we cannot observe locally and is taken from the worker as a
/// subset of `input`.
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cached: u64,
    pub reasoning: u64,
}

impl From<TokenUsage> for Usage {
    fn from(u: TokenUsage) -> Self {
        Self {
            input: u.prompt,
            output: u.completion,
            cached: u.cached,
            reasoning: u.reasoning,
        }
    }
}

impl Usage {
    /// Count the job ourselves and keep the worker's number only when it is
    /// higher. Cache hits stay the worker's to report.
    pub fn billed(params: &LlmParams, text: &str, thinking: Option<&str>, calls: &[ToolCall], meta: &Value) -> Self {
        TokenUsage::measure(params, Some(text), thinking, calls)
            .reconcile(TokenUsage::from_meta(meta))
            .into()
    }

    /// Read `prompt_tokens` / `completion_tokens` out of a result's `meta`.
    ///
    /// Prefer [`Self::billed`]: this is the worker's claim, unchallenged.
    pub fn from_meta(meta: &Value) -> Self {
        TokenUsage::from_meta(meta).map(Usage::from).unwrap_or_default()
    }

    fn openai_json(&self) -> Value {
        serde_json::json!({
            "prompt_tokens": self.input,
            "completion_tokens": self.output,
            "total_tokens": self.input + self.output,
            "prompt_tokens_details": { "cached_tokens": self.cached },
            "completion_tokens_details": { "reasoning_tokens": self.reasoning },
        })
    }

    fn anthropic_json(&self) -> Value {
        serde_json::json!({
            "input_tokens": self.input,
            "output_tokens": self.output,
            "cache_read_input_tokens": self.cached,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anthropic(body: Value) -> ChatRequest {
        serde_json::from_value::<AnthropicRequest>(body)
            .unwrap()
            .into_chat()
            .unwrap()
    }

    fn answering(text: &str, calls: Vec<ToolCall>) -> Answer {
        Answer {
            text: text.into(),
            tool_calls: calls,
            usage: Usage::default(),
            finish: None,
        }
    }

    fn call(name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: "toolu_1".into(),
            name: name.into(),
            arguments: args.into(),
        }
    }

    #[test]
    fn a_system_prompt_becomes_the_first_message() {
        let req = anthropic(serde_json::json!({
            "model": "deepseek",
            "max_tokens": 20_000,
            "system": "You are terse.",
            "messages": [{ "role": "user", "content": "hi" }],
        }));

        assert_eq!(req.params.messages.len(), 2);
        assert_eq!(req.params.messages[0].role, "system");
        assert_eq!(req.params.messages[0].content, "You are terse.");
        assert_eq!(req.params.messages[1].content, "hi");
        assert_eq!(req.params.max_tokens, 20_000);
    }

    #[test]
    fn a_block_array_system_prompt_works_too() {
        // Claude Code sends this form, with cache_control on the blocks.
        let req = anthropic(serde_json::json!({
            "model": "m",
            "system": [
                { "type": "text", "text": "part one" },
                { "type": "text", "text": "part two", "cache_control": { "type": "ephemeral" } },
            ],
            "messages": [{ "role": "user", "content": "hi" }],
        }));
        assert_eq!(req.params.messages[0].content, "part one\n\npart two");
    }

    #[test]
    fn tools_reach_the_worker() {
        // Without this the model calls a tool it was never given, the server
        // has nothing to parse it into, and the job comes back empty.
        let req = anthropic(serde_json::json!({
            "model": "m",
            "messages": [{ "role": "user", "content": "read a.txt" }],
            "tools": [{
                "name": "Read",
                "description": "Read a file",
                "input_schema": { "type": "object", "properties": { "path": { "type": "string" } } },
            }],
        }));

        assert_eq!(req.params.tools.len(), 1);
        assert_eq!(req.params.tools[0].name, "Read");
        assert_eq!(req.params.tools[0].input_schema["type"], "object");
    }

    #[test]
    fn server_side_tools_that_a_worker_cannot_run_are_left_out() {
        // `web_search` and friends have no input_schema; forwarding one would
        // be a bad request to the inference server.
        let req = anthropic(serde_json::json!({
            "model": "m",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [
                { "type": "web_search_20260209", "name": "web_search" },
                { "name": "Read", "input_schema": { "type": "object" } },
            ],
        }));
        assert_eq!(
            req.params
                .tools
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Read"]
        );
    }

    #[test]
    fn a_tool_exchange_is_resplit_into_the_roles_openai_expects() {
        // Anthropic puts the result in a *user* turn; an OpenAI-shaped server
        // needs it as its own `tool` message, correlated by id.
        let req = anthropic(serde_json::json!({
            "model": "m",
            "messages": [
                { "role": "user", "content": "read a.txt" },
                { "role": "assistant", "content": [
                    { "type": "tool_use", "id": "t1", "name": "Read", "input": { "path": "a.txt" } }
                ]},
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "t1", "content": "hello world" }
                ]},
            ],
        }));

        let m = &req.params.messages;
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].role, "user");

        assert_eq!(m[1].role, "assistant");
        assert_eq!(m[1].tool_calls.len(), 1);
        assert_eq!(m[1].tool_calls[0].name, "Read");
        assert_eq!(m[1].tool_calls[0].arguments, r#"{"path":"a.txt"}"#);

        assert_eq!(m[2].role, "tool");
        assert_eq!(m[2].tool_call_id.as_deref(), Some("t1"));
        assert_eq!(m[2].content, "hello world");
    }

    #[test]
    fn a_tool_call_comes_back_as_a_tool_use_block() {
        let answer = answering("", vec![call("Read", r#"{"path":"a.txt"}"#)]);
        let body = anthropic_message("msg_1", "m", &answer);

        assert_eq!(body["stop_reason"], "tool_use");
        assert_eq!(body["content"][0]["type"], "tool_use");
        assert_eq!(body["content"][0]["name"], "Read");
        // Parsed, not a string — this is the shape clients read.
        assert_eq!(body["content"][0]["input"]["path"], "a.txt");
    }

    #[test]
    fn text_and_a_tool_call_can_arrive_together() {
        let answer = answering("Let me look.", vec![call("Read", "{}")]);
        let body = anthropic_message("msg_1", "m", &answer);
        assert_eq!(body["content"][0]["type"], "text");
        assert_eq!(body["content"][1]["type"], "tool_use");
    }

    #[test]
    fn arguments_that_are_not_valid_json_still_reach_the_client() {
        // Failing the whole turn because a model produced malformed JSON
        // loses the answer as well; keep it where the client can see it.
        let answer = answering("", vec![call("Read", "not json")]);
        let body = anthropic_message("msg_1", "m", &answer);
        assert_eq!(body["content"][0]["input"]["_raw"], "not json");
    }

    #[test]
    fn the_streamed_tool_call_carries_the_blocks_clients_accumulate() {
        let answer = answering("", vec![call("Read", r#"{"path":"a.txt"}"#)]);
        let events = anthropic_stream("msg_1", "m", &answer);

        let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        assert_eq!(events[1].1["content_block"]["type"], "tool_use");
        assert_eq!(events[2].1["delta"]["type"], "input_json_delta");
        assert_eq!(events[2].1["delta"]["partial_json"], r#"{"path":"a.txt"}"#);
        assert_eq!(events[4].1["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn a_plain_answer_still_streams_as_text() {
        let events = anthropic_stream("msg_1", "m", &answering("hello", vec![]));
        assert_eq!(events[1].1["content_block"]["type"], "text");
        assert_eq!(events[2].1["delta"]["text"], "hello");
        assert_eq!(events[4].1["delta"]["stop_reason"], "end_turn");
    }

    #[test]
    fn image_only_turns_do_not_become_blank_messages() {
        let req = anthropic(serde_json::json!({
            "model": "m",
            "messages": [
                { "role": "user", "content": [{ "type": "image", "source": {} }] },
                { "role": "user", "content": "what is this" },
            ],
        }));
        assert_eq!(req.params.messages.len(), 1);
        assert_eq!(req.params.messages[0].content, "what is this");
    }

    #[test]
    fn a_request_with_nothing_to_answer_is_refused() {
        let body = serde_json::json!({ "model": "m", "system": "hi", "messages": [] });
        assert!(serde_json::from_value::<AnthropicRequest>(body)
            .unwrap()
            .into_chat()
            .is_err());
    }

    #[test]
    fn a_client_that_names_no_temperature_gets_zero() {
        let anthropic = anthropic(serde_json::json!({
            "model": "m",
            "messages": [{ "role": "user", "content": "hi" }],
        }));
        let openai: ChatRequest = serde_json::from_value::<OpenAiRequest>(serde_json::json!({
            "model": "m",
            "messages": [{ "role": "user", "content": "hi" }],
        }))
        .unwrap()
        .into_chat()
        .unwrap();
        assert_eq!(anthropic.params.temperature, 0.0);
        assert_eq!(openai.params.temperature, 0.0);
    }

    #[test]
    fn thinking_tags_are_not_the_answer() {
        assert_eq!(
            peel_thinking("<think>let me parse this carefully</think>\n\nuse the Read tool"),
            "use the Read tool"
        );
        assert_eq!(
            peel_thinking("<THINK>looping</THINK>\nhello"),
            "hello"
        );
        assert_eq!(
            peel_thinking("ok\n<thinking>no</thinking>\nreal answer"),
            "ok\nreal answer"
        );
        assert_eq!(peel_thinking("<think>never closed"), "");
        assert_eq!(peel_thinking("just text"), "just text");
    }

    #[test]
    fn missing_max_tokens_gets_a_workable_default() {
        // OpenAI clients routinely omit it; 0 would fail validation.
        let req = anthropic(serde_json::json!({
            "model": "m",
            "messages": [{ "role": "user", "content": "hi" }],
        }));
        assert_eq!(req.params.max_tokens, DEFAULT_MAX_TOKENS);
        rootmode_core::JobPayload::Llm(req.params)
            .validate()
            .unwrap();
    }

    #[test]
    fn openai_requests_map_the_same_way() {
        let req: ChatRequest = serde_json::from_value::<OpenAiRequest>(serde_json::json!({
            "model": "deepseek",
            "messages": [
                { "role": "system", "content": "be brief" },
                { "role": "user", "content": "hi" },
            ],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "weather",
                    "parameters": { "type": "object" },
                },
            }],
            "max_completion_tokens": 20_000,
            "stream": true,
        }))
        .unwrap()
        .into_chat()
        .unwrap();

        assert!(req.stream);
        assert_eq!(req.params.max_tokens, 20_000);
        assert_eq!(req.params.messages.len(), 2);
        assert_eq!(req.params.tools[0].name, "get_weather");
    }

    #[test]
    fn an_openai_tool_exchange_round_trips() {
        let req: ChatRequest = serde_json::from_value::<OpenAiRequest>(serde_json::json!({
            "model": "m",
            "messages": [
                { "role": "user", "content": "weather?" },
                { "role": "assistant", "content": null, "tool_calls": [{
                    "id": "c1", "type": "function",
                    "function": { "name": "get_weather", "arguments": "{\"city\":\"Paris\"}" },
                }]},
                { "role": "tool", "tool_call_id": "c1", "content": "18C" },
            ],
        }))
        .unwrap()
        .into_chat()
        .unwrap();

        let m = &req.params.messages;
        assert_eq!(m[1].tool_calls[0].name, "get_weather");
        assert_eq!(m[2].role, "tool");
        assert_eq!(m[2].tool_call_id.as_deref(), Some("c1"));
    }

    #[test]
    fn openai_replies_use_tool_calls_and_the_matching_finish_reason() {
        let answer = answering("", vec![call("Read", "{}")]);
        let body = openai_completion("id", "m", &answer, 0);

        assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            body["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            "Read"
        );
        // Not an empty string: some clients choke on that.
        assert!(body["choices"][0]["message"]["content"].is_null());

        let chunks = openai_stream("id", "m", &answer, 0);
        let finish = chunks
            .iter()
            .find(|c| c["choices"].get(0).is_some_and(|ch| ch["finish_reason"].is_string()))
            .expect("a finish_reason chunk");
        assert_eq!(finish["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            chunks[1]["choices"][0]["delta"]["tool_calls"][0]["index"],
            0
        );
        let usage = chunks.last().unwrap();
        assert_eq!(usage["usage"]["completion_tokens"], 0);
        assert!(usage["choices"].as_array().is_some_and(|c| c.is_empty()));
    }

    #[test]
    fn a_system_message_after_the_conversation_starts_is_folded_into_a_user_turn() {
        // Claude Code sends these constantly. Passed through as a `system`
        // role mid-conversation, DeepSeek's chat template answers with
        // nothing at all — the "empty completion" failure.
        let req = anthropic(serde_json::json!({
            "model": "m",
            "system": "top level",
            "messages": [
                { "role": "user", "content": "what is this" },
                { "role": "system", "content": "Reminder: be terse." },
                { "role": "assistant", "content": [
                    { "type": "tool_use", "id": "t1", "name": "Read", "input": {} }
                ]},
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "t1", "content": "# rootmode" }
                ]},
                { "role": "system", "content": [{ "type": "text", "text": "Answer now." }] },
            ],
        }));

        let roles: Vec<&str> = req
            .params
            .messages
            .iter()
            .map(|m| m.role.as_str())
            .collect();
        assert_eq!(
            roles,
            ["system", "user", "assistant", "tool", "user"],
            "only the opening system message keeps that role"
        );

        // Nothing was thrown away: the instruction is an instruction, and a
        // silently dropped one is worse than a badly placed one.
        assert!(req.params.messages[1]
            .content
            .contains("Reminder: be terse."));
        assert!(req.params.messages[1].content.contains("<system-reminder>"));
        // A trailing one has no user turn to join, so it becomes its own.
        assert!(req.params.messages[4].content.contains("Answer now."));
    }

    #[test]
    fn a_leading_run_of_system_messages_is_left_alone() {
        let req = anthropic(serde_json::json!({
            "model": "m",
            "system": "first",
            "messages": [
                { "role": "system", "content": "also up front" },
                { "role": "user", "content": "hi" },
            ],
        }));
        let roles: Vec<&str> = req
            .params
            .messages
            .iter()
            .map(|m| m.role.as_str())
            .collect();
        assert_eq!(roles, ["system", "system", "user"]);
    }

    #[test]
    fn a_client_that_names_no_ceiling_gets_room_to_think_first() {
        // A reasoning model can spend thousands of tokens before writing a
        // word. A default below that floor does not shorten the answer, it
        // removes it — the generation ends having produced only reasoning,
        // which the client never sees. OpenAI-shaped clients omit the field
        // routinely, so this default is load-bearing.
        let openai: ChatRequest = serde_json::from_value::<OpenAiRequest>(serde_json::json!({
            "model": "m",
            "messages": [{ "role": "user", "content": "hi" }],
        }))
        .unwrap()
        .into_chat()
        .unwrap();

        let anthropic = anthropic(serde_json::json!({
            "model": "m",
            "messages": [{ "role": "user", "content": "hi" }],
        }));

        for req in [openai, anthropic] {
            assert!(
                req.params.max_tokens >= 8192,
                "a ceiling of {} yields no answer at all on a reasoning model",
                req.params.max_tokens
            );
        }
    }

    #[test]
    fn an_answer_cut_short_says_so_rather_than_looking_finished() {
        // A reasoning model can spend its whole budget thinking and get cut
        // off mid-sentence. Reporting that as `end_turn` shows the client a
        // half-answer it has no way to recognise as incomplete.
        let mut answer = answering("half a sen", vec![]);
        answer.finish = Some("length".into());

        assert_eq!(
            anthropic_message("id", "m", &answer)["stop_reason"],
            "max_tokens"
        );
        assert_eq!(
            anthropic_stream("id", "m", &answer)[4].1["delta"]["stop_reason"],
            "max_tokens"
        );
        assert_eq!(
            openai_completion("id", "m", &answer, 0)["choices"][0]["finish_reason"],
            "length"
        );

        // A normal ending is still a normal ending.
        let mut done = answering("all of it", vec![]);
        done.finish = Some("stop".into());
        assert_eq!(
            anthropic_message("id", "m", &done)["stop_reason"],
            "end_turn"
        );
    }

    #[test]
    fn usage_that_the_worker_did_not_report_stays_zero_until_we_count_it() {
        assert_eq!(Usage::from_meta(&serde_json::json!({})), Usage::default());
        assert_eq!(
            Usage::from_meta(&serde_json::json!({
                "prompt_tokens": 12, "completion_tokens": 30, "cached_tokens": 4, "reasoning_tokens": 8
            })),
            Usage {
                input: 12,
                output: 30,
                cached: 4,
                reasoning: 8,
            }
        );
    }

    #[test]
    fn billed_usage_takes_the_tokenizer_when_the_worker_under_reports() {
        let params = LlmParams {
            model_hash: None,
            model_id: Some("gpt-4o".into()),
            messages: vec![ChatMessage::new("user", "hello world")],
            tools: Vec::new(),
            max_tokens: 16,
            temperature: 0.0,
        };
        let low = serde_json::json!({ "prompt_tokens": 1, "completion_tokens": 1 });
        let billed = Usage::billed(&params, "hello world", None, &[], &low);
        assert!(
            billed.input > 1,
            "prompt framing must beat a worker that reported 1, got {}",
            billed.input
        );
        assert!(
            billed.output >= 2,
            "o200k 'hello world' is 2 tokens, worker said 1, got {}",
            billed.output
        );
    }

    #[test]
    fn a_picture_in_an_anthropic_turn_reaches_the_worker() {
        let req: AnthropicRequest = serde_json::from_value(serde_json::json!({
            "model": "qwen2.5-vl",
            "max_tokens": 100,
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "image", "source": {
                        "type": "base64", "media_type": "image/jpeg", "data": "/9j/4AAQ" } },
                    { "type": "text", "text": "what is in this picture?" }
                ]
            }]
        }))
        .unwrap();

        let chat = req.into_chat().unwrap();
        let user = chat.params.messages.last().unwrap();
        assert_eq!(user.content, "what is in this picture?");
        assert_eq!(user.images, vec!["data:image/jpeg;base64,/9j/4AAQ"]);
    }

    #[test]
    fn a_picture_in_an_openai_turn_reaches_the_worker() {
        let req: OpenAiRequest = serde_json::from_value(serde_json::json!({
            "model": "qwen2.5-vl",
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": "read this" },
                    { "type": "image_url",
                      "image_url": { "url": "data:image/png;base64,iVBORw0KGgo" } }
                ]
            }]
        }))
        .unwrap();

        let chat = req.into_chat().unwrap();
        let user = chat.params.messages.last().unwrap();
        assert_eq!(user.content, "read this");
        assert_eq!(user.images, vec!["data:image/png;base64,iVBORw0KGgo"]);
    }

    #[test]
    fn a_picture_that_is_a_link_is_dropped_rather_than_fetched() {
        // Following it would have the worker request whatever address the
        // sender named, from inside the operator's network.
        let req: OpenAiRequest = serde_json::from_value(serde_json::json!({
            "model": "m",
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": "look" },
                    { "type": "image_url", "image_url": { "url": "http://169.254.169.254/latest/meta-data/" } }
                ]
            }]
        }))
        .unwrap();

        let chat = req.into_chat().unwrap();
        assert!(chat.params.messages.last().unwrap().images.is_empty());
    }

    #[test]
    fn a_turn_that_is_only_a_picture_still_becomes_a_message() {
        let mut out = Vec::new();
        push_anthropic_turn(
            &mut out,
            "user",
            &serde_json::json!([{ "type": "image", "source": {
                "type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo" } }]),
        );
        // No text to carry, but there is still something to ask about.
        assert_eq!(out.len(), 1);
        assert!(out[0].content.is_empty());
        assert_eq!(out[0].images.len(), 1);
    }

    // ------------------------------------------------------------ responses

    fn responses(body: Value) -> ChatRequest {
        serde_json::from_value::<ResponsesRequest>(body)
            .unwrap()
            .into_chat()
            .unwrap()
    }

    #[test]
    fn a_plain_string_input_becomes_a_user_turn() {
        let req = responses(serde_json::json!({
            "model": "m",
            "input": "hi there",
        }));
        assert_eq!(req.params.messages.len(), 1);
        assert_eq!(req.params.messages[0].role, "user");
        assert_eq!(req.params.messages[0].content, "hi there");
    }

    #[test]
    fn instructions_become_the_first_message() {
        let req = responses(serde_json::json!({
            "model": "m",
            "instructions": "Be terse.",
            "input": [{ "type": "message", "role": "user", "content": [
                { "type": "input_text", "text": "hi" }
            ]}],
        }));
        assert_eq!(req.params.messages.len(), 2);
        assert_eq!(req.params.messages[0].role, "system");
        assert_eq!(req.params.messages[0].content, "Be terse.");
        assert_eq!(req.params.messages[1].content, "hi");
    }

    #[test]
    fn developer_role_is_treated_as_system() {
        let req = responses(serde_json::json!({
            "model": "m",
            "input": [
                { "type": "message", "role": "developer", "content": [
                    { "type": "input_text", "text": "follow the rules" }
                ]},
                { "type": "message", "role": "user", "content": [
                    { "type": "input_text", "text": "hi" }
                ]},
            ],
        }));
        assert_eq!(req.params.messages[0].role, "system");
    }

    #[test]
    fn a_function_call_and_its_output_round_trip() {
        let req = responses(serde_json::json!({
            "model": "m",
            "input": [
                { "type": "message", "role": "user", "content": [
                    { "type": "input_text", "text": "weather?" }
                ]},
                { "type": "function_call", "call_id": "c1", "name": "get_weather", "arguments": "{\"city\":\"Paris\"}" },
                { "type": "function_call_output", "call_id": "c1", "output": "18C" },
            ],
        }));

        let m = &req.params.messages;
        assert_eq!(m.len(), 3);
        assert_eq!(m[1].role, "assistant");
        assert_eq!(m[1].tool_calls[0].name, "get_weather");
        assert_eq!(m[2].role, "tool");
        assert_eq!(m[2].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(m[2].content, "18C");
    }

    #[test]
    fn responses_tools_have_no_nested_function_object() {
        let req = responses(serde_json::json!({
            "model": "m",
            "input": "hi",
            "tools": [{
                "type": "function", "name": "get_weather", "description": "…",
                "parameters": { "type": "object" },
            }],
        }));
        assert_eq!(req.params.tools[0].name, "get_weather");
    }

    #[test]
    fn a_responses_reply_carries_output_text_and_usage() {
        let answer = answering("Hi!", vec![]);
        let body = openai_response("id1", "m", &answer, 0);
        assert_eq!(body["output_text"], "Hi!");
        assert_eq!(body["output"][0]["type"], "message");
        assert_eq!(body["output"][0]["content"][0]["text"], "Hi!");
        assert_eq!(body["status"], "completed");
    }

    #[test]
    fn a_responses_tool_call_becomes_a_function_call_output_item() {
        let answer = answering("", vec![call("Read", "{\"path\":\"a\"}")]);
        let body = openai_response("id1", "m", &answer, 0);
        assert_eq!(body["output"][0]["type"], "function_call");
        assert_eq!(body["output"][0]["name"], "Read");
        assert_eq!(body["output"][0]["call_id"], "toolu_1");
    }

    #[test]
    fn a_streamed_response_ends_with_a_completed_event_carrying_the_full_answer() {
        let answer = answering("hello", vec![]);
        let events = openai_response_stream("id1", "m", &answer, 0);
        let (name, data) = events.last().unwrap();
        assert_eq!(name, "response.completed");
        assert_eq!(data["response"]["output_text"], "hello");
    }

    fn event_names(events: &[(String, Value)]) -> Vec<&str> {
        events.iter().map(|(n, _)| n.as_str()).collect()
    }

    #[test]
    fn a_streamed_response_opens_the_output_item_before_any_text_delta() {
        // Codex logs "OutputTextDelta without active item" and drops the
        // whole answer if a delta arrives without this pair first. The GPU
        // still ran; the editor just never sees what it produced.
        let answer = answering("hello", vec![]);
        let events = openai_response_stream("id1", "m", &answer, 0);
        let names = event_names(&events);
        let added = names.iter().position(|n| *n == "response.output_item.added").unwrap();
        let part = names.iter().position(|n| *n == "response.content_part.added").unwrap();
        let delta = names.iter().position(|n| *n == "response.output_text.delta").unwrap();
        assert!(added < part, "{names:?}");
        assert!(part < delta, "{names:?}");
        assert!(names.contains(&"response.output_text.done"));
        assert!(names.contains(&"response.content_part.done"));
        assert!(names.contains(&"response.output_item.done"));
        assert!(names.contains(&"response.in_progress"));
    }

    #[test]
    fn a_tool_only_stream_does_not_reserve_output_index_zero_for_missing_text() {
        let answer = answering("", vec![call("Read", "{\"path\":\"a\"}")]);
        let events = openai_response_stream("id1", "m", &answer, 0);
        let added = events
            .iter()
            .find(|(n, _)| n == "response.output_item.added")
            .map(|(_, v)| v)
            .unwrap();
        assert_eq!(added["output_index"], 0);
        assert_eq!(added["item"]["type"], "function_call");
        assert!(
            event_names(&events)
                .iter()
                .filter(|n| **n == "response.output_item.done")
                .count()
                == 1,
            "the function_call item must close before response.completed"
        );
    }

    #[test]
    fn live_end_without_a_prior_text_item_still_opens_one() {
        // A worker that only sends the finished result, no deltas: Codex
        // still needs the item opened before the text is dumped.
        let answer = answering("hi", vec![]);
        let events = responses_live_end("id1", "m", &answer, 0, false);
        let names = event_names(&events);
        assert_eq!(
            names,
            vec![
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
    }
}

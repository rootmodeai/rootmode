//! Job model. Jobs are *data*: a fixed set of workflow kinds with typed
//! parameters. There is no free-form "run this" field, and worker output is
//! never interpreted as code.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{CoreError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobKind {
    Llm,
    Image,
    Video,
}

impl JobKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobKind::Llm => "llm",
            JobKind::Image => "image",
            JobKind::Video => "video",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Queued,
    Running,
    Done,
    Failed,
}

impl JobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobStatus::Queued => "queued",
            JobStatus::Running => "running",
            JobStatus::Done => "done",
            JobStatus::Failed => "failed",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, JobStatus::Done | JobStatus::Failed)
    }
}

/// Ceiling on one encoded image in a message, in characters of base64.
///
/// Well under the 64 MB frame limit, so a job carrying several pictures is
/// refused here — with a sentence saying which and why — rather than at the
/// transport, where the whole submission dies without explanation.
pub const MAX_IMAGE_CHARS: usize = 20_000_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    /// "system" | "user" | "assistant" | "tool" — kept as a string so an
    /// unfamiliar role from a newer peer does not fail the whole message.
    pub role: String,
    pub content: String,
    /// Tools this assistant turn asked for. Empty for every other role.
    ///
    /// Carried as history so a model can see what it already called: without
    /// it, a second turn re-requests the same tool because as far as it knows
    /// nothing happened.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Set on a `tool` message: which call this is the answer to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Pictures the model should look at, alongside `content`.
    ///
    /// Base64 image bytes, or a `data:` URL — both are accepted, because a
    /// client that already has one for display should not have to unwrap it.
    /// Never an `http(s)` URL: that would have the worker fetch a location a
    /// stranger chose, which is a different and much worse thing than
    /// decoding bytes it was handed.
    ///
    /// Added after v1, so it defaults to empty: a worker that predates it
    /// ignores the field and answers on the text, and an older client never
    /// sends one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
}

impl ChatMessage {
    pub fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            images: Vec::new(),
        }
    }

    /// The same message, with pictures for the model to look at.
    pub fn with_images(mut self, images: Vec<String>) -> Self {
        self.images = images;
        self
    }
}

/// One tool invocation, in the shape both API dialects agree on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    /// Correlates the call with its result. Echoed back by the client.
    pub id: String,
    pub name: String,
    /// The arguments as a **JSON string**, not an object — that is what
    /// OpenAI-compatible servers emit, and re-parsing it here would lose
    /// whatever the model actually produced when it is not valid JSON.
    pub arguments: String,
}

/// A tool the client is offering the model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema for the arguments.
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LlmParams {
    /// Content address of the weights, when the peer advertises one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_hash: Option<String>,
    /// Human-facing model identifier, e.g. "llama-3.1-8b-instruct".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    pub messages: Vec<ChatMessage>,
    /// Tools the model may call. Empty means a plain completion.
    ///
    /// Added after v1 shipped, so it defaults to empty: an older worker
    /// ignores the field and answers as it always did, and an older client
    /// simply never sends it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    /// How hard a reasoning model should think before it answers — one of
    /// [`REASONING_EFFORTS`]. Absent means the provider's default, which is
    /// what every job asked for before this field existed; an older worker
    /// ignores it and does exactly that.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

/// The efforts a client may ask for, lowest first. The names are the ones
/// OpenAI-shaped APIs use, so a coding tool's setting passes through unchanged.
pub const REASONING_EFFORTS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh"];

fn default_max_tokens() -> u32 {
    512
}
fn default_temperature() -> f32 {
    0.7
}

/// What a client asks for when it wants a picture: a model, and words.
///
/// Nothing else. Sampler steps, guidance scale, resolution, scheduler and the
/// shape of the graph are how an operator built their pipeline, not what a
/// user asked for — and a client cannot know the right values for a pipeline
/// it has never seen. A guidance scale of 6 is sensible on one checkpoint and
/// ruinous on another; the operator knows which, and the client never can.
///
/// The worker reports what it actually used in the result's `meta`, so the
/// numbers are visible after the fact without being dictated beforehand.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImageParams {
    /// Content address of the checkpoint, when the peer advertises one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_hash: Option<String>,
    /// Which advertised model to use, when a worker serves more than one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_id: Option<String>,
    pub prompt: String,
    /// A picture to start from, base64-encoded, for "more like this, but…".
    ///
    /// This is intent, not pipeline configuration — "begin here" is a thing
    /// only the person asking can know — so it belongs alongside the prompt
    /// while steps and guidance stay with the operator.
    ///
    /// Added after v1: absent means an ordinary text-to-image job, and an
    /// older worker ignores the field and makes a fresh picture rather than
    /// failing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_image: Option<String>,
    /// How much of the starting picture to keep, `0.0`–`1.0`.
    ///
    /// Coarse on purpose: `0.2` is a nudge, `0.8` is a fresh picture that
    /// merely rhymes with the old one. Absent lets the worker choose, which
    /// is the right default — a client cannot calibrate this for a checkpoint
    /// it has never seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<f32>,
    /// Where to repaint: a base64 PNG the same size as `from_image`, white
    /// where the picture should change and black where it must not.
    ///
    /// This is the difference between "make me something like this" and
    /// "leave all of this alone except here". Everything outside the white is
    /// returned untouched, so a person's face survives a change of clothes —
    /// which no amount of denoise tuning can achieve, because that regenerates
    /// the whole canvas.
    ///
    /// Only meaningful with `from_image`; a mask with nothing to mask is a
    /// mistake worth reporting rather than ignoring.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask: Option<String>,
}

/// A clip from words, optionally starting from a still.
///
/// Length, fps, resolution and the graph stay with the operator — a client
/// cannot know the right values for MiniMax H3 versus Wan versus LTX.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VideoParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_id: Option<String>,
    pub prompt: String,
    /// First frame, base64, for image-to-video. Absent is text-to-video.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_image: Option<String>,
    /// How long, in seconds. Absent means the provider's default shape —
    /// what every clip was before a client could ask, so an older worker
    /// that ignores these fields makes exactly the clip it always did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<u32>,
    /// "480p", "720p", "1080p", "4K" — whatever the provider lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    /// "16:9", "9:16", "1:1" — whatever the provider lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aspect_ratio: Option<String>,
    /// Whether the clip should have sound, on providers that sell both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<bool>,
}

impl VideoParams {
    /// Whether the client asked for any particular shape. A request with
    /// none is the provider's default clip at the price it advertised.
    pub fn is_shaped(&self) -> bool {
        self.seconds.is_some() || self.resolution.is_some() || self.aspect_ratio.is_some() || self.audio.is_some()
    }
}

/// The `payload` of a `job.submit`, discriminated by `kind`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum JobPayload {
    Llm(LlmParams),
    Image(ImageParams),
    Video(VideoParams),
}

impl JobPayload {
    pub fn kind(&self) -> JobKind {
        match self {
            JobPayload::Llm(_) => JobKind::Llm,
            JobPayload::Image(_) => JobKind::Image,
            JobPayload::Video(_) => JobKind::Video,
        }
    }

    /// Bounds checks applied before a job leaves this machine. A peer is free
    /// to reject more aggressively; we refuse to *send* obvious garbage.
    pub fn validate(&self) -> Result<()> {
        match self {
            JobPayload::Llm(p) => {
                if p.messages.is_empty() {
                    return Err(CoreError::Invalid("llm job has no messages".into()));
                }
                if p.messages.iter().all(|m| {
                    m.content.trim().is_empty() && m.tool_calls.is_empty() && m.images.is_empty()
                }) {
                    return Err(CoreError::Invalid("llm job has no prompt text".into()));
                }
                for m in &p.messages {
                    for image in &m.images {
                        let image = image.trim();
                        if image.is_empty() {
                            return Err(CoreError::Invalid("a message image is empty".into()));
                        }
                        // Only bytes the client already holds. A URL would
                        // make the worker fetch whatever a stranger names —
                        // its own network, its own credentials, its own
                        // internal addresses.
                        let lower = image.to_ascii_lowercase();
                        if !lower.starts_with("data:") && lower.contains("://") {
                            return Err(CoreError::Invalid(
                                "message images must be base64 or a data: URL, not a link".into(),
                            ));
                        }
                        if image.len() > MAX_IMAGE_CHARS {
                            return Err(CoreError::Invalid(format!(
                                "a message image is larger than {} MB encoded",
                                MAX_IMAGE_CHARS / 1_000_000
                            )));
                        }
                    }
                }
                for t in &p.tools {
                    if t.name.trim().is_empty() {
                        return Err(CoreError::Invalid("a tool has no name".into()));
                    }
                }
                if p.max_tokens == 0 || p.max_tokens > 131_072 {
                    return Err(CoreError::Invalid(
                        "max_tokens out of range (1..=131072)".into(),
                    ));
                }
                if !(0.0..=2.0).contains(&p.temperature) {
                    return Err(CoreError::Invalid(
                        "temperature out of range (0.0..=2.0)".into(),
                    ));
                }
                if let Some(effort) = &p.reasoning_effort {
                    if !REASONING_EFFORTS.contains(&effort.as_str()) {
                        return Err(CoreError::Invalid(format!(
                            "reasoning_effort must be one of {}",
                            REASONING_EFFORTS.join(", ")
                        )));
                    }
                }
            }
            JobPayload::Image(p) => {
                if p.prompt.trim().is_empty() {
                    return Err(CoreError::Invalid("image job has an empty prompt".into()));
                }
                if p.prompt.chars().count() > 8_000 {
                    return Err(CoreError::Invalid("image prompt is too long".into()));
                }
                if let Some(change) = p.change {
                    if !(0.0..=1.0).contains(&change) {
                        return Err(CoreError::Invalid("change out of range (0.0..=1.0)".into()));
                    }
                }
                if p.from_image.as_ref().is_some_and(|i| i.trim().is_empty()) {
                    return Err(CoreError::Invalid("from_image is present but empty".into()));
                }
            }
            JobPayload::Video(p) => {
                if p.prompt.trim().is_empty() {
                    return Err(CoreError::Invalid("video job has an empty prompt".into()));
                }
                if p.prompt.chars().count() > 8_000 {
                    return Err(CoreError::Invalid("video prompt is too long".into()));
                }
                if p.from_image.as_ref().is_some_and(|i| i.trim().is_empty()) {
                    return Err(CoreError::Invalid("from_image is present but empty".into()));
                }
                if let Some(s) = p.seconds {
                    if !(1..=60).contains(&s) {
                        return Err(CoreError::Invalid("seconds out of range (1..=60)".into()));
                    }
                }
                if let Some(r) = &p.resolution {
                    if r.trim().is_empty() || r.len() > 8 {
                        return Err(CoreError::Invalid("resolution is not a name like 720p".into()));
                    }
                }
                if let Some(a) = &p.aspect_ratio {
                    let ok = a.split_once(':').is_some_and(|(w, h)| {
                        w.parse::<u8>().is_ok_and(|w| w > 0) && h.parse::<u8>().is_ok_and(|h| h > 0)
                    });
                    if !ok {
                        return Err(CoreError::Invalid("aspect_ratio is not a ratio like 16:9".into()));
                    }
                }
            }
        }
        Ok(())
    }

    /// Short one-line summary for job tables.
    pub fn summary(&self) -> String {
        let text = match self {
            JobPayload::Llm(p) => p
                .messages
                .iter()
                .rev()
                .find(|m| m.role == "user")
                .or_else(|| p.messages.last())
                .map(|m| m.content.clone())
                .unwrap_or_default(),
            JobPayload::Image(p) => p.prompt.clone(),
            JobPayload::Video(p) => p.prompt.clone(),
        };
        let text = text.trim().replace('\n', " ");
        if text.chars().count() > 80 {
            let head: String = text.chars().take(79).collect();
            format!("{head}…")
        } else {
            text
        }
    }

    pub fn model_label(&self) -> String {
        let (hash, id) = match self {
            JobPayload::Llm(p) => (p.model_hash.as_deref(), p.model_id.as_deref()),
            JobPayload::Image(p) => (p.model_hash.as_deref(), p.checkpoint_id.as_deref()),
            JobPayload::Video(p) => (p.model_hash.as_deref(), p.checkpoint_id.as_deref()),
        };
        id.map(str::to_string)
            // char-safe truncation: `model_hash` is client-controlled, so a
            // byte slice at 12 could split a multibyte char and panic.
            .or_else(|| hash.map(|h| format!("sha256:{}", h.chars().take(12).collect::<String>())))
            .unwrap_or_else(|| "peer default".to_string())
    }
}

/// A job as tracked locally. The wire form is [`crate::protocol::JobSubmit`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub job_id: Uuid,
    pub peer_id: String,
    pub payload: JobPayload,
    pub status: JobStatus,
    #[serde(default)]
    pub progress: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Unix seconds.
    pub created_at: i64,
}

impl Job {
    pub fn new(peer_id: impl Into<String>, payload: JobPayload, created_at: i64) -> Self {
        Self {
            job_id: Uuid::new_v4(),
            peer_id: peer_id.into(),
            payload,
            status: JobStatus::Queued,
            progress: 0.0,
            error: None,
            created_at,
        }
    }

    pub fn kind(&self) -> JobKind {
        self.payload.kind()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn llm(msgs: Vec<(&str, &str)>) -> JobPayload {
        JobPayload::Llm(LlmParams {
            model_hash: None,
            model_id: Some("m".into()),
            messages: msgs
                .into_iter()
                .map(|(r, c)| ChatMessage::new(r, c))
                .collect(),
            tools: Vec::new(),
            max_tokens: 64,
            temperature: 0.7,
            reasoning_effort: None,
        })
    }

    #[test]
    fn payload_tag_roundtrips() {
        let p = llm(vec![("user", "hi")]);
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"kind\":\"llm\""));
        assert_eq!(serde_json::from_str::<JobPayload>(&json).unwrap(), p);
    }

    #[test]
    fn validation_catches_bad_input() {
        assert!(llm(vec![("user", "hi")]).validate().is_ok());
        assert!(llm(vec![]).validate().is_err());
        assert!(llm(vec![("user", "   ")]).validate().is_err());

        let mut img = ImageParams {
            model_hash: None,
            checkpoint_id: Some("c".into()),
            prompt: "a node".into(),
            from_image: None,
            change: None,
            mask: None,
        };
        assert!(JobPayload::Image(img.clone()).validate().is_ok());
        img.prompt = "  ".into();
        assert!(JobPayload::Image(img).validate().is_err());
    }

    #[test]
    fn an_image_job_carries_a_model_and_words_and_nothing_else() {
        // Sampler steps, guidance and size are how an operator built their
        // pipeline. A client cannot know the right values for a checkpoint it
        // has never seen, so it is not asked to.
        let json = serde_json::to_string(&JobPayload::Image(ImageParams {
            model_hash: None,
            checkpoint_id: Some("sdxl".into()),
            prompt: "a glider over a datacentre".into(),
            from_image: None,
            change: None,
            mask: None,
        }))
        .unwrap();

        for gone in ["steps", "cfg", "width", "height", "seed", "negative_prompt"] {
            assert!(
                !json.contains(gone),
                "{gone} should not be on the wire: {json}"
            );
        }
    }

    #[test]
    fn summary_prefers_last_user_message() {
        let p = llm(vec![("system", "be terse"), ("user", "what is a peer")]);
        assert_eq!(p.summary(), "what is a peer");
    }

    #[test]
    fn video_payload_is_a_model_and_words() {
        let p = JobPayload::Video(VideoParams {
            model_hash: None,
            checkpoint_id: Some("minimax-h3".into()),
            prompt: "a glider over a datacentre".into(),
            from_image: None,
            seconds: None,
            resolution: None,
            aspect_ratio: None,
            audio: None,
        });
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"kind\":\"video\""));
        assert_eq!(serde_json::from_str::<JobPayload>(&json).unwrap(), p);
        assert!(p.validate().is_ok());

        let empty = JobPayload::Video(VideoParams {
            model_hash: None,
            checkpoint_id: None,
            prompt: "  ".into(),
            from_image: None,
            seconds: None,
            resolution: None,
            aspect_ratio: None,
            audio: None,
        });
        assert!(empty.validate().is_err());
    }
}

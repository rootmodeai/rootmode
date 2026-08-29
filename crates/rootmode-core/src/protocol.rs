//! RootmodeProtocol v1 — versioned JSON messages.
//!
//! Every message carries `"v"`. Messages with an unknown `"type"` deserialize
//! to [`WorkerMessage::Unknown`] and are dropped by the client rather than
//! erroring the connection: forwards-compatibility without trust.
//!
//! See `docs/PROTOCOL.md`, which is kept in sync with this file.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    canonical::canonical_bytes,
    identity::{verify_hex, Identity},
    job::{JobKind, JobPayload, JobStatus},
    CoreError, Result,
};

pub const PROTOCOL_VERSION: u32 = 1;

fn default_version() -> u32 {
    PROTOCOL_VERSION
}

/// Same default, for types in other modules of this crate.
pub fn default_version_pub() -> u32 {
    PROTOCOL_VERSION
}

/// Reject anything we do not speak. Called on every inbound message.
pub fn check_version(v: u32) -> Result<()> {
    if v == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(CoreError::Version {
            got: v,
            expected: PROTOCOL_VERSION,
        })
    }
}

// ---------------------------------------------------------------- client → worker

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobCancel {
    pub job_id: Uuid,
}

/// What a client-requested stop is reported as, in a `job.status`'s `error`
/// field. One string, defined once, because every worker implementation and
/// the desktop client's own demo transport all need to agree on exactly the
/// same text for a client to tell "you stopped this" apart from "this broke".
pub const STOPPED: &str = "stopped by client";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobSubmit {
    #[serde(default = "default_version")]
    pub v: u32,
    pub job_id: Uuid,
    /// Submitting peer id (hex ed25519 public key).
    pub from: String,
    pub payload: JobPayload,
    /// Hex ed25519 signature over the canonical JSON of this message with
    /// `sig` removed. Optional in v1; workers may require it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
    /// What the client authorises to be spent, cumulatively, on the channel it
    /// shares with this worker.
    ///
    /// Carried with the job rather than settled afterwards: money leaves a
    /// client's balance only with the client's signature, and a worker that
    /// starts work before holding one is working for nothing. Absent on a free
    /// peer, and on any worker that charges nothing.
    ///
    /// Covered by `sig`, which binds the two together: an authorisation
    /// cannot be lifted off this job and presented with another. It carries
    /// its own EIP-712 signature as well, because the peer id that asks and
    /// the address that pays are different keys on different curves — one
    /// proves who is asking, the other is what a contract on Base will
    /// honour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend: Option<crate::payments::SpendingAuth>,
    /// The paying wallet, `0x…`. A priced worker looks this up on the pot
    /// before spending GPU: no remaining lock, no work. Absent on a free
    /// peer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payer: Option<String>,
    /// Prepaid spend for this job, sized in 1M-token chunks at the dearest
    /// rate (clipped to the pot cap). The app signs however many chunks the
    /// prompt + ceiling need, so the stream never stalls on a boundary.
    /// After the job, `job.pay` may capture the actual bill; if none arrives
    /// the worker settles this prepaid amount.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bond: Option<JobPay>,
    /// Signed `reserve()` for the worker to post. Anyone can submit it; the
    /// worker already pays gas to settle, so it posts this too. The client
    /// never needs ETH.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve: Option<ReservePost>,
}

/// App-key signature over a [`crate::payments::ReserveTicket`]. The worker
/// (or anyone) posts it on-chain.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReservePost {
    pub ticket: crate::payments::ReserveTicket,
    pub sig: String,
}

impl JobSubmit {
    pub fn new(job_id: Uuid, from: impl Into<String>, payload: JobPayload) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            job_id,
            from: from.into(),
            payload,
            sig: None,
            spend: None,
            payer: None,
            bond: None,
            reserve: None,
        }
    }

    /// Sign in place. The `from` field is set to the signer's peer id.
    ///
    /// Preimage is the job object (`v`, `from`, `job_id`, `payload`, and any
    /// `payer`/`bond`/`spend`/`reserve`) with `sig` removed. The on-the-wire
    /// `"type"` tag is not part of it.
    pub fn signed_by(mut self, identity: &Identity) -> Result<Self> {
        self.from = identity.peer_id();
        self.sig = None;
        let bytes = canonical_bytes(&self)?;
        self.sig = Some(identity.sign_hex(&bytes));
        Ok(self)
    }

    /// Verify `sig` against `from`. Errors if unsigned.
    pub fn verify(&self) -> Result<()> {
        let sig = self
            .sig
            .as_ref()
            .ok_or_else(|| CoreError::Signature("message is unsigned".into()))?;
        let mut unsigned = self.clone();
        unsigned.sig = None;
        let body = canonical_bytes(&unsigned)?;
        verify_hex(&self.from, &body, sig)
    }

    /// Verify using the JSON that actually arrived, not a re-serialised
    /// struct. That is the only preimage that cannot drift through floats or
    /// skipped fields. Strips `sig` and the `"type"` tag, then checks the
    /// same job-object preimage [`signed_by`] covers.
    pub fn verify_wire(raw: &str) -> Result<()> {
        let mut value: serde_json::Value = serde_json::from_str(raw)?;
        let from = value
            .get("from")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::Signature("message has no from".into()))?
            .to_string();
        let sig = value
            .get("sig")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CoreError::Signature("message is unsigned".into()))?
            .to_string();
        match value.as_object_mut() {
            Some(obj) => {
                obj.remove("sig");
                obj.remove("type");
            }
            None => return Err(CoreError::Invalid("expected a JSON object".into())),
        }
        let body = serde_json::to_string(&value)?.into_bytes();
        verify_hex(&from, &body, &sig)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ClientMessage {
    #[serde(rename = "job.submit")]
    JobSubmit(JobSubmit),
    /// Stop a job in progress. Unsigned: it asks the worker to stop spending
    /// its own GPU time, which needs no more authority than knowing the job
    /// id — the same bar every other status message on this job is held to.
    #[serde(rename = "job.cancel")]
    JobCancel(JobCancel),
    /// Sent on connect so the worker knows who is asking.
    #[serde(rename = "peer.hello")]
    PeerHello(PeerHello),
    /// Capture this job's actual bill, at or below the prepaid 1M-token chunk.
    #[serde(rename = "job.pay")]
    JobPay(JobPay),
    /// Anything this version does not know about. Workers drop it rather than
    /// closing the connection — the same rule clients apply to worker messages.
    #[serde(other)]
    Unknown,
}

impl ClientMessage {
    /// Strict parse with the same guarantees as [`WorkerMessage::parse`].
    pub fn parse(raw: &str) -> Result<Self> {
        if raw.len() > MAX_MESSAGE_BYTES {
            return Err(CoreError::Invalid(format!(
                "message exceeds {MAX_MESSAGE_BYTES} bytes"
            )));
        }
        let msg: ClientMessage = serde_json::from_str(raw)?;
        match &msg {
            ClientMessage::JobSubmit(m) => check_version(m.v)?,
            ClientMessage::PeerHello(m) => check_version(m.v)?,
            ClientMessage::JobPay(m) => check_version(m.v)?,
            ClientMessage::JobCancel(_) | ClientMessage::Unknown => {}
        }
        Ok(msg)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PeerHello {
    #[serde(default = "default_version")]
    pub v: u32,
    pub peer_id: String,
}

/// What this job actually costs. Streaming already ran against a prepaid
/// 1M-token chunk; this is the lower capture so an honest client is not
/// charged the whole unused remainder of the chunk.
///
/// `amount` is this job's delta in USDC micros, not the channel cumulative.
/// The client recomputes it from the advertised price and these token
/// counts, and refuses to sign a higher number.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobInvoice {
    #[serde(default = "default_version")]
    pub v: u32,
    pub job_id: Uuid,
    pub amount: u64,
    /// Hash of the result bytes the worker will send after pay. The client
    /// rejects a result that does not match, so a worker cannot invoice one
    /// job and deliver another.
    pub sha256: String,
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub cached_tokens: u64,
    /// Another 1M-token slice, requested mid-stream so a long reply does not
    /// stop. The client app signs this the same way as submit, with no UI.
    #[serde(default)]
    pub top_up: bool,
}

/// The client's signed SpendTicket for [`JobInvoice`]. Cumulative: the
/// newest total this worker is authorised to take from the lock.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobPay {
    #[serde(default = "default_version")]
    pub v: u32,
    pub job_id: Uuid,
    pub ticket: crate::payments::SpendTicket,
    /// Hex 65-byte EIP-712 signature by the pot app key.
    pub sig: String,
}

// ---------------------------------------------------------------- worker → client

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobStatusUpdate {
    #[serde(default = "default_version")]
    pub v: u32,
    pub job_id: Uuid,
    pub status: JobStatus,
    #[serde(default)]
    pub progress: f32,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobResult {
    #[serde(default = "default_version")]
    pub v: u32,
    pub job_id: Uuid,
    pub kind: JobKind,
    /// sha256 of the result bytes: the utf-8 text for `llm`, the image file
    /// for `image`. This is the content address shown in the UI.
    pub sha256: String,
    /// llm only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// llm only: tools the model asked to run. A result may carry these with
    /// no text at all — that is a model choosing to act rather than answer,
    /// not an empty response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<crate::job::ToolCall>,
    /// image only: a path if the worker is local, otherwise base64 bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_path_or_b64: Option<String>,
    /// Reasoning the model produced before the answer, when it produced any.
    /// Not part of `sha256` — the hash covers the answer the user asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(default)]
    pub meta: serde_json::Value,
}

impl JobResult {
    /// Typed view of the payload, with the kind/field mismatch rejected.
    pub fn body(&self) -> Result<JobResultBody> {
        match self.kind {
            JobKind::Llm => match (&self.text, self.tool_calls.is_empty()) {
                (Some(t), _) => Ok(JobResultBody::Text(t.clone())),
                // Acting instead of answering is a legitimate outcome.
                (None, false) => Ok(JobResultBody::Text(String::new())),
                (None, true) => Err(CoreError::Invalid("llm result has no text".into())),
            },
            JobKind::Image => self
                .image_path_or_b64
                .clone()
                .map(JobResultBody::Image)
                .ok_or_else(|| CoreError::Invalid("image result has no image data".into())),
            JobKind::Video => self
                .image_path_or_b64
                .clone()
                .map(JobResultBody::Video)
                .ok_or_else(|| CoreError::Invalid("video result has no video data".into())),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum JobResultBody {
    Text(String),
    /// Path on disk, or base64-encoded bytes.
    Image(String),
    Video(String),
}

/// One prepaid authorisation covers this many tokens, billed at the dearest
/// of input / output / cache-write so a mixed job of the same size cannot
/// come out as a loss. Clipped on the pot by `maxPerJob`.
pub const TOKEN_CHUNK: u64 = 1_000_000;

/// What an operator asks for a model.
///
/// The unit is fixed by the job kind: per **million tokens** for `llm`, per
/// **image** for `image`. For text, a single [`Self::amount`] is a flat rate
/// used when the worker does not split input, output and cache (a vLLM box
/// with one number). OpenRouter workers fill [`Self::input`], [`Self::output`]
/// and [`Self::cache`] so a bill can match the upstream invoice instead of
/// guessing from a blend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Price {
    /// Flat rate, or the highest of the split rates. Used for ranking and as
    /// the fallback when a result has no token breakdown.
    pub amount: f64,
    #[serde(default = "default_currency")]
    pub currency: String,
    /// USD per million uncached prompt tokens. Absent → [`Self::amount`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<f64>,
    /// USD per million completion tokens. Absent → [`Self::amount`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<f64>,
    /// USD per million cached prompt tokens. Absent → billed as input, which
    /// never loses money against a cheaper cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<f64>,
    /// USD per million prompt-cache writes. Some providers charge more than
    /// input to fill the cache; uncached prompt is billed at
    /// `max(input, cache_write)` so that write is not eaten as a loss.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

fn default_currency() -> String {
    "USD".to_string()
}

impl Default for Price {
    fn default() -> Self {
        Self {
            amount: 0.0,
            currency: default_currency(),
            input: None,
            output: None,
            cache: None,
            cache_write: None,
        }
    }
}

/// Round a USD rate up to two decimal places.
pub fn ceil_cents(x: f64) -> f64 {
    if !x.is_finite() || x <= 0.0 {
        return 0.0;
    }
    let scaled = x * 100.0;
    let nearest = scaled.round();
    if (scaled - nearest).abs() < 1e-6 {
        nearest / 100.0
    } else {
        scaled.ceil() / 100.0
    }
}

impl Price {
    pub fn new(amount: f64) -> Self {
        Self {
            amount: ceil_cents(amount),
            ..Self::default()
        }
    }

    /// Round every rate up to two decimal places.
    pub fn round_protocol(mut self) -> Self {
        self.amount = ceil_cents(self.amount);
        self.input = self.input.map(ceil_cents);
        self.output = self.output.map(ceil_cents);
        self.cache = self.cache.map(ceil_cents);
        self.cache_write = self.cache_write.map(ceil_cents);
        let top = self.max_rate();
        if top > self.amount {
            self.amount = top;
        }
        self
    }

    pub fn is_free(&self) -> bool {
        self.amount <= 0.0
            && self.input.unwrap_or(0.0) <= 0.0
            && self.output.unwrap_or(0.0) <= 0.0
    }

    /// Split rates in USD / million tokens. A flat [`Self::amount`] fills
    /// every missing component. Each rate is rounded up to two decimal places.
    pub fn llm_rates(&self) -> (f64, f64, f64, f64) {
        let input = ceil_cents(self.input.unwrap_or(self.amount));
        let output = ceil_cents(self.output.unwrap_or(self.amount));
        let cache = ceil_cents(self.cache.unwrap_or(input));
        let cache_write = ceil_cents(self.cache_write.unwrap_or(input)).max(input);
        (input, output, cache, cache_write)
    }

    /// Charge for a chat, in USDC micros.
    ///
    /// `cached` is a subset of `prompt`. Fresh prompt is billed at
    /// `max(input, cache_write)` so a cache-write premium is not a loss;
    /// cached prompt at the cache-read rate; completion at output.
    pub fn charge_llm_micros(&self, prompt: u64, completion: u64, cached: u64) -> u64 {
        let (input, output, cache, cache_write) = self.llm_rates();
        let cached = cached.min(prompt);
        let uncached = prompt.saturating_sub(cached);
        let fresh = input.max(cache_write);
        (uncached as f64 * fresh + cached as f64 * cache + completion as f64 * output)
            .round()
            .max(0.0) as u64
    }

    /// Dearest USD-per-million rate this price table will ever charge.
    pub fn max_rate(&self) -> f64 {
        let (input, output, _, cache_write) = self.llm_rates();
        input.max(output).max(cache_write).max(0.0)
    }

    /// Micros to buy [`TOKEN_CHUNK`] tokens at [`Self::max_rate`].
    pub fn chunk_micros(&self) -> u64 {
        (TOKEN_CHUNK as f64 * self.max_rate()).round().max(1.0) as u64
    }

    /// How many tokens in total — prompt included — a bond of `micros`
    /// covers for a job whose prompt is `prompt` tokens long.
    ///
    /// The prompt is billed at the input rate and only what is left buys
    /// completion tokens at the output rate. Pricing the prompt at the
    /// output rate instead (as [`Self::tokens_for_micros`] does, being
    /// rate-agnostic) undercounts the budget by the ratio of the two rates:
    /// on a model five times dearer to write than to read, a long prompt
    /// exhausts such a budget before the first token is written, and a
    /// bond that in fact covers the whole answer is treated as spent.
    ///
    /// A bond that does not even cover the prompt covers no completion.
    pub fn authorized_tokens(&self, micros: u64, prompt: u64) -> u64 {
        let (_, output, _, _) = self.llm_rates();
        if output <= 0.0 {
            return u64::MAX;
        }
        let prompt_cost = self.charge_llm_micros(prompt, 0, 0);
        let left = micros.saturating_sub(prompt_cost);
        let completion = (left as f64 / output).floor() as u64;
        prompt.saturating_add(completion)
    }

    /// How many tokens `micros` buys at [`Self::max_rate`].
    pub fn tokens_for_micros(&self, micros: u64) -> u64 {
        let rate = self.max_rate();
        if rate <= 0.0 {
            return u64::MAX;
        }
        (micros as f64 / rate).floor() as u64
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelDescriptor {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub kind: JobKind,
    /// Absent means the operator named no price. Treated as free, because
    /// nothing is being charged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<Price>,
    /// Video models: the shapes a clip may take and what each costs. `price`
    /// stays the default shape's price, so a client that never looks here
    /// locks and pays exactly what it did before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<VideoOffer>,
}

/// Whether a provider's clips have sound, and whether that is up to the client.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AudioOffer {
    /// Silent, always.
    Never,
    /// Sound built in; it cannot be switched off.
    Always,
    /// Either, priced apart. The default is silent, the cheaper.
    Optional,
}

/// USD per second, after markup, for one shape of clip. A rate with no
/// resolution applies to a provider that offers no choice of one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VideoRate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    pub audio: bool,
    pub from_image: bool,
    pub usd_per_second: f64,
    /// The least a generation costs, however short.
    #[serde(default)]
    pub minimum_usd: f64,
}

/// The menu for a video model: what may be chosen, what is chosen when
/// nothing is, and the rate for every combination. The client quotes a
/// shape from this before it locks; the provider bills the same quote.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VideoOffer {
    /// Lengths on offer, seconds. Empty: only `default_seconds`.
    #[serde(default)]
    pub durations: Vec<u32>,
    pub default_seconds: u32,
    /// Empty: the provider offers no choice.
    #[serde(default)]
    pub resolutions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_resolution: Option<String>,
    #[serde(default)]
    pub aspect_ratios: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_aspect: Option<String>,
    pub audio: AudioOffer,
    /// Whether a first frame (image-to-video) is taken at all. Absent on
    /// the wire means an older provider that did not say: assumed yes.
    #[serde(default = "yes")]
    pub first_frame: bool,
    pub rates: Vec<VideoRate>,
}

fn yes() -> bool {
    true
}

/// One clip, fully decided: what will be asked of the provider and priced.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoShape {
    pub seconds: u32,
    pub resolution: Option<String>,
    pub aspect_ratio: Option<String>,
    pub audio: bool,
    pub from_image: bool,
}

impl VideoOffer {
    /// The shape a request resolves to: the client's choices where it made
    /// them, this offer's defaults where it did not, and an error naming
    /// the menu when a choice is not on it.
    pub fn shape_for(&self, p: &crate::job::VideoParams) -> std::result::Result<VideoShape, String> {
        let seconds = p.seconds.unwrap_or(self.default_seconds);
        if !self.durations.is_empty() {
            if !self.durations.contains(&seconds) {
                return Err(format!(
                    "{seconds} s is not offered; this model makes {} s clips",
                    list_of(&self.durations.iter().map(u32::to_string).collect::<Vec<_>>())
                ));
            }
        } else if seconds != self.default_seconds {
            return Err(format!("this model only makes {} s clips", self.default_seconds));
        }
        let resolution = match &p.resolution {
            Some(want) => Some(
                self.resolutions
                    .iter()
                    .find(|r| r.eq_ignore_ascii_case(want))
                    .cloned()
                    .ok_or_else(|| {
                        if self.resolutions.is_empty() {
                            "this model offers no choice of resolution".to_string()
                        } else {
                            format!("{want} is not offered; this model makes {}", list_of(&self.resolutions))
                        }
                    })?,
            ),
            None => self.default_resolution.clone(),
        };
        let aspect_ratio = match &p.aspect_ratio {
            Some(want) => Some(
                self.aspect_ratios
                    .iter()
                    .find(|a| a.as_str() == want)
                    .cloned()
                    .ok_or_else(|| {
                        if self.aspect_ratios.is_empty() {
                            "this model offers no choice of aspect ratio".to_string()
                        } else {
                            format!("{want} is not offered; this model makes {}", list_of(&self.aspect_ratios))
                        }
                    })?,
            ),
            None => self.default_aspect.clone(),
        };
        let audio = match self.audio {
            AudioOffer::Never => {
                if p.audio == Some(true) {
                    return Err("this model makes silent clips".into());
                }
                false
            }
            AudioOffer::Always => {
                if p.audio == Some(false) {
                    return Err("sound cannot be switched off on this model".into());
                }
                true
            }
            AudioOffer::Optional => p.audio.unwrap_or(false),
        };
        let from_image = p.from_image.as_deref().is_some_and(|s| !s.trim().is_empty());
        if from_image && !self.first_frame {
            return Err("this model cannot start from a picture".into());
        }
        Ok(VideoShape {
            seconds,
            resolution,
            aspect_ratio,
            audio,
            from_image,
        })
    }

    /// The rate for a shape: the entry matching its resolution and sound,
    /// preferring the one that also matches whether a first frame is sent,
    /// and among several the dearest, so a quote never undercuts the bill.
    pub fn rate_for(&self, shape: &VideoShape) -> Option<&VideoRate> {
        let fits = |r: &&VideoRate| {
            r.audio == shape.audio
                && match (&r.resolution, &shape.resolution) {
                    (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                    (None, _) => true,
                    (Some(_), None) => false,
                }
        };
        fn dearest<'a>(rs: impl Iterator<Item = &'a VideoRate>) -> Option<&'a VideoRate> {
            rs.fold(None, |m: Option<&'a VideoRate>, r| match m {
                Some(m) if m.usd_per_second >= r.usd_per_second => Some(m),
                _ => Some(r),
            })
        }
        let exact = dearest(self.rates.iter().filter(fits).filter(|r| r.from_image == shape.from_image));
        if exact.is_some() {
            return exact;
        }
        dearest(self.rates.iter().filter(fits))
    }

    /// What a shape costs, in USD, rounded up to the cent.
    pub fn quote_usd(&self, shape: &VideoShape) -> Option<f64> {
        let rate = self.rate_for(shape)?;
        let cost = (rate.usd_per_second * shape.seconds as f64).max(rate.minimum_usd);
        Some(ceil_cents(cost))
    }

    /// The default shape's price — what `ModelDescriptor::price` carries.
    pub fn default_shape(&self) -> VideoShape {
        VideoShape {
            seconds: self.default_seconds,
            resolution: self.default_resolution.clone(),
            aspect_ratio: self.default_aspect.clone(),
            audio: self.audio == AudioOffer::Always,
            from_image: false,
        }
    }
}

fn list_of(items: &[String]) -> String {
    items.join(", ")
}

impl ModelDescriptor {
    /// What this costs for ranking and the picker. Unpriced is free.
    pub fn amount(&self) -> f64 {
        self.price
            .as_ref()
            .map(|p| ceil_cents(p.amount.max(p.max_rate())))
            .unwrap_or(0.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PeerAnnounce {
    #[serde(default = "default_version")]
    pub v: u32,
    pub peer_id: String,
    /// A human name for this node, e.g. "dgx spark". Optional and untrusted —
    /// anyone can call themselves anything, so it is a convenience for the
    /// operator's own machines, never an identity. The peer id is the identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Where the operator says the machine is: an ISO 3166-1 alpha-2 code,
    /// `"DE"`, `"GB"`, `"SG"`.
    ///
    /// Declared, never derived. The alternative is looking the endpoint's
    /// address up in somebody's geolocation service, which would hand a third
    /// party the list of peers you talk to — the exact record this network
    /// exists to not create. So it is what the operator typed, and like
    /// `label` it is a convenience rather than a fact: a node can claim any
    /// country, and nothing here can check it.
    ///
    /// Added after v1: absent means "did not say", which is different from
    /// "nowhere" and is displayed as such.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(default)]
    pub caps: Vec<String>,
    #[serde(default)]
    pub models: Vec<ModelDescriptor>,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: u32,
    /// Where this node would be paid, when there is anything to pay: an
    /// address on the settlement chain.
    ///
    /// Nominated by the peer itself and needing no proof — naming somebody
    /// else's address only gives them your money. A client shows it so a
    /// person can see who benefits before sending work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payout: Option<String>,
}

fn default_max_concurrent() -> u32 {
    1
}

/// Incremental tokens for a running job. Older peers ignore this type.
///
/// `text` is the answer the user will read. `thinking` is what the model
/// said to itself first, when it said anything. Either field may be empty
/// on a given frame — a reasoning model often emits only thinking for a
/// long time, then only text.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct JobDelta {
    #[serde(default = "default_version")]
    pub v: u32,
    pub job_id: Uuid,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub thinking: String,
}

impl JobDelta {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.thinking.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum WorkerMessage {
    #[serde(rename = "job.status")]
    JobStatus(JobStatusUpdate),
    #[serde(rename = "job.result")]
    JobResult(JobResult),
    #[serde(rename = "job.delta")]
    JobDelta(JobDelta),
    #[serde(rename = "peer.announce")]
    PeerAnnounce(PeerAnnounce),
    /// Bill for a priced job. The result follows only after [`JobPay`].
    #[serde(rename = "job.invoice")]
    JobInvoice(JobInvoice),
    /// Anything this version does not know about. Ignored, not fatal.
    #[serde(other)]
    Unknown,
}

impl WorkerMessage {
    /// Strict parse: unknown types survive as [`WorkerMessage::Unknown`], but a
    /// known type with a wrong version or a malformed body is an error.
    pub fn parse(raw: &str) -> Result<Self> {
        if raw.len() > MAX_MESSAGE_BYTES {
            return Err(CoreError::Invalid(format!(
                "message exceeds {MAX_MESSAGE_BYTES} bytes"
            )));
        }
        let msg: WorkerMessage = serde_json::from_str(raw)?;
        match &msg {
            WorkerMessage::JobStatus(m) => check_version(m.v)?,
            WorkerMessage::JobResult(m) => check_version(m.v)?,
            WorkerMessage::JobDelta(m) => check_version(m.v)?,
            WorkerMessage::PeerAnnounce(m) => check_version(m.v)?,
            WorkerMessage::JobInvoice(m) => check_version(m.v)?,
            WorkerMessage::Unknown => {}
        }
        Ok(msg)
    }
}

/// Hard cap on an inbound frame. Base64 images are the big case; 64 MiB is
/// generous for a 4096² PNG and still bounds a hostile peer.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{ChatMessage, LlmParams};

    fn payload() -> JobPayload {
        JobPayload::Llm(LlmParams {
            model_hash: None,
            model_id: Some("test".into()),
            messages: vec![ChatMessage::new("user", "ping")],
            tools: Vec::new(),
            max_tokens: 32,
            temperature: 0.0,
            reasoning_effort: None,
        })
    }

    #[test]
    fn submit_shape_matches_spec() {
        let msg = ClientMessage::JobSubmit(JobSubmit::new(Uuid::nil(), "abcd", payload()));
        let v: serde_json::Value = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], "job.submit");
        assert_eq!(v["v"], 1);
        assert_eq!(v["payload"]["kind"], "llm");
        assert!(v.get("sig").is_none(), "unsigned submits omit sig");
    }

    #[test]
    fn sign_then_verify() {
        let id = Identity::generate();
        let submit = JobSubmit::new(Uuid::new_v4(), "placeholder", payload())
            .signed_by(&id)
            .unwrap();
        assert_eq!(submit.from, id.peer_id());
        submit.verify().unwrap();

        let mut tampered = submit.clone();
        tampered.job_id = Uuid::new_v4();
        assert!(tampered.verify().is_err());
    }

    #[test]
    fn a_signed_chat_survives_the_wire() {
        // The chat screen sends temperature 0.7 (f32). If re-canonicalising
        // after JSON parse changes that number, the worker rejects the job
        // with "Verification equation was not satisfied".
        let id = Identity::generate();
        let payload = JobPayload::Llm(LlmParams {
            model_hash: None,
            model_id: Some("llama-3.1-8b-instruct".into()),
            messages: vec![
                ChatMessage::new("user", "Test"),
                ChatMessage::new("user", "Test"),
            ],
            tools: Vec::new(),
            max_tokens: 16384,
            temperature: 0.7,
            reasoning_effort: None,
        });
        let submit = JobSubmit::new(Uuid::new_v4(), "placeholder", payload)
            .signed_by(&id)
            .unwrap();
        submit.verify().unwrap();

        let mut unsigned = submit.clone();
        unsigned.sig = None;
        let body = canonical_bytes(&unsigned).unwrap();
        crate::identity::verify_hex(&submit.from, &body, submit.sig.as_ref().unwrap())
            .expect("signed_by must cover the job body");

        let wire = serde_json::to_string(&ClientMessage::JobSubmit(submit.clone())).unwrap();
        let parsed = ClientMessage::parse(&wire).expect("wire");
        let ClientMessage::JobSubmit(again) = parsed else {
            panic!("not a submit");
        };
        again
            .verify()
            .unwrap_or_else(|e| panic!("round-trip verify failed: {e}\nwire={wire}"));
        let wire = crate::canonical::wire_json(&ClientMessage::JobSubmit(submit.clone())).unwrap();
        JobSubmit::verify_wire(&wire)
            .unwrap_or_else(|e| panic!("verify_wire failed: {e}\nwire={wire}"));
    }

    #[test]
    fn an_envelope_signature_is_rejected() {
        let id = Identity::generate();
        let mut submit = JobSubmit::new(Uuid::new_v4(), "placeholder", payload());
        submit.from = id.peer_id();
        submit.sig = None;
        let bytes = canonical_bytes(&ClientMessage::JobSubmit(submit.clone())).unwrap();
        submit.sig = Some(id.sign_hex(&bytes));
        assert!(
            submit.verify().is_err(),
            "signatures over the type-tagged envelope are not accepted"
        );
    }

    #[test]
    fn a_delta_carries_whichever_side_moved() {
        let msg = WorkerMessage::parse(
            r#"{"v":1,"type":"job.delta","job_id":"00000000-0000-0000-0000-000000000000","thinking":"hmm"}"#,
        )
        .unwrap();
        match msg {
            WorkerMessage::JobDelta(d) => {
                assert_eq!(d.thinking, "hmm");
                assert!(d.text.is_empty());
            }
            other => panic!("expected a delta, got {other:?}"),
        }
    }

    #[test]
    fn unknown_type_is_ignored_not_fatal() {
        let msg = WorkerMessage::parse(r#"{"v":1,"type":"peer.gossip","x":1}"#).unwrap();
        assert_eq!(msg, WorkerMessage::Unknown);
    }

    #[test]
    fn wrong_version_is_rejected() {
        let raw = r#"{"v":2,"type":"job.status","job_id":"00000000-0000-0000-0000-000000000000","status":"running"}"#;
        assert!(matches!(
            WorkerMessage::parse(raw),
            Err(CoreError::Version {
                got: 2,
                expected: 1
            })
        ));
    }

    #[test]
    fn malformed_known_type_is_rejected() {
        assert!(WorkerMessage::parse(r#"{"v":1,"type":"job.status","status":"nope"}"#).is_err());
    }

    #[test]
    fn result_body_requires_matching_field() {
        let r = JobResult {
            v: 1,
            job_id: Uuid::nil(),
            kind: JobKind::Image,
            tool_calls: Vec::new(),
            sha256: "00".into(),
            text: Some("wrong field".into()),
            image_path_or_b64: None,
            thinking: None,
            meta: serde_json::json!({}),
        };
        assert!(r.body().is_err());
    }

    #[test]
    fn llm_charge_uses_input_output_and_cache() {
        let price = Price {
            amount: 3.0,
            input: Some(1.0),
            output: Some(3.0),
            cache: Some(0.1),
            cache_write: None,
            ..Price::default()
        };
        // 800 fresh + 200 cached + 500 out = 800*1 + 200*0.1 + 500*3
        assert_eq!(price.charge_llm_micros(1000, 500, 200), 2_320);
    }

    #[test]
    fn cache_write_premium_is_taken_on_fresh_prompt() {
        let price = Price {
            amount: 3.0,
            input: Some(1.0),
            output: Some(2.0),
            cache: Some(0.1),
            cache_write: Some(1.25),
            ..Price::default()
        };
        // 100 uncached at 1.25, 900 cached at 0.1, 10 out at 2
        assert_eq!(price.charge_llm_micros(1000, 10, 900), 125 + 90 + 20);
    }

    #[test]
    fn a_flat_rate_bills_every_token_the_same() {
        let price = Price::new(20.0);
        assert_eq!(price.charge_llm_micros(0, 1000, 0), 20_000);
        assert_eq!(price.charge_llm_micros(500, 500, 0), 20_000);
        assert_eq!(price.chunk_micros(), 20_000_000, "1M tokens at $20/M is $20");
        assert_eq!(price.tokens_for_micros(500_000), 25_000); // $0.50 at $20/M
    }

    #[test]
    fn a_bond_covers_the_prompt_at_the_input_rate_and_the_rest_at_output() {
        // Reads at $2/M, writes at $10/M — the shape of a reasoning model.
        let price = Price {
            input: Some(2.0),
            output: Some(10.0),
            ..Price::new(10.0)
        };
        // 40k prompt tokens and room for exactly 1,000 completion tokens.
        let bond = price.charge_llm_micros(40_000, 1_000, 0);
        assert_eq!(bond, 80_000 + 10_000);
        assert_eq!(price.authorized_tokens(bond, 40_000), 41_000);
        // Priced at the dearest rate throughout, the same bond looks like
        // 9,000 tokens — fewer than the prompt alone. That is the bug this
        // replaces: a 40k-token prompt would be "over budget" before the
        // first token was written.
        assert_eq!(price.tokens_for_micros(bond), 9_000);
        // A bond short of the prompt buys nothing beyond it.
        assert_eq!(price.authorized_tokens(1, 40_000), 40_000);
        // A flat rate reduces to the old arithmetic.
        let flat = Price::new(20.0);
        assert_eq!(flat.authorized_tokens(500_000, 5_000), flat.tokens_for_micros(500_000));
        // A free model has no budget to run out of.
        assert_eq!(Price::new(0.0).authorized_tokens(0, 40_000), u64::MAX);
    }

    fn offer() -> VideoOffer {
        VideoOffer {
            durations: vec![4, 6, 8],
            default_seconds: 6,
            resolutions: vec!["720p".into(), "1080p".into()],
            default_resolution: Some("720p".into()),
            aspect_ratios: vec!["16:9".into(), "9:16".into()],
            default_aspect: Some("16:9".into()),
            audio: AudioOffer::Optional,
            first_frame: true,
            rates: vec![
                VideoRate { resolution: Some("720p".into()), audio: false, from_image: false, usd_per_second: 0.05, minimum_usd: 0.0 },
                VideoRate { resolution: Some("720p".into()), audio: false, from_image: true, usd_per_second: 0.07, minimum_usd: 0.0 },
                VideoRate { resolution: Some("720p".into()), audio: true, from_image: false, usd_per_second: 0.10, minimum_usd: 0.0 },
                VideoRate { resolution: Some("1080p".into()), audio: false, from_image: false, usd_per_second: 0.20, minimum_usd: 0.5 },
            ],
        }
    }

    fn clip() -> crate::job::VideoParams {
        crate::job::VideoParams {
            model_hash: None,
            checkpoint_id: Some("m".into()),
            prompt: "a cat".into(),
            from_image: None,
            seconds: None,
            resolution: None,
            aspect_ratio: None,
            audio: None,
        }
    }

    #[test]
    fn a_request_with_no_choices_is_the_default_shape_at_the_advertised_price() {
        let o = offer();
        let shape = o.shape_for(&clip()).unwrap();
        assert_eq!(shape, o.default_shape());
        assert_eq!(shape.seconds, 6);
        assert_eq!(shape.resolution.as_deref(), Some("720p"));
        assert!(!shape.audio);
        assert_eq!(o.quote_usd(&shape), Some(0.30)); // 0.05 × 6
    }

    #[test]
    fn choices_are_priced_by_their_own_rate_and_checked_against_the_menu() {
        let o = offer();
        let mut p = clip();
        p.seconds = Some(8);
        p.resolution = Some("1080P".into());
        p.aspect_ratio = Some("9:16".into());
        let shape = o.shape_for(&p).unwrap();
        assert_eq!(shape.resolution.as_deref(), Some("1080p"), "matched case-insensitively, stored as offered");
        assert_eq!(o.quote_usd(&shape), Some(1.60)); // 0.20 × 8, above the 0.50 minimum
        // A short clip is charged the minimum.
        p.seconds = Some(4);
        assert_eq!(o.quote_usd(&o.shape_for(&p).unwrap()), Some(0.80)); // 0.20 × 4 = 0.80 > 0.5
        // Sound, where it is optional, is its own rate.
        let mut loud = clip();
        loud.audio = Some(true);
        assert_eq!(o.quote_usd(&o.shape_for(&loud).unwrap()), Some(0.60)); // 0.10 × 6
        // A first frame picks the image-to-video rate when there is one.
        let mut from = clip();
        from.from_image = Some("abc".into());
        assert_eq!(o.quote_usd(&o.shape_for(&from).unwrap()), Some(0.42)); // 0.07 × 6
        // ...and falls back to the text rate where there is not.
        from.audio = Some(true);
        assert_eq!(o.quote_usd(&o.shape_for(&from).unwrap()), Some(0.60));
        // Off the menu is refused in words that name the menu.
        let mut bad = clip();
        bad.seconds = Some(5);
        assert!(o.shape_for(&bad).unwrap_err().contains("4, 6, 8"));
        let mut bad = clip();
        bad.resolution = Some("4K".into());
        assert!(o.shape_for(&bad).unwrap_err().contains("720p, 1080p"));
        let mut bad = clip();
        bad.aspect_ratio = Some("1:1".into());
        assert!(o.shape_for(&bad).unwrap_err().contains("16:9"));
    }

    #[test]
    fn sound_follows_what_the_provider_sells() {
        let mut o = offer();
        o.audio = AudioOffer::Always;
        assert!(o.shape_for(&clip()).unwrap().audio, "built in: on by default");
        let mut off = clip();
        off.audio = Some(false);
        assert!(o.shape_for(&off).unwrap_err().contains("switched off"));
        o.audio = AudioOffer::Never;
        let mut on = clip();
        on.audio = Some(true);
        assert!(o.shape_for(&on).unwrap_err().contains("silent"));
        assert!(!o.shape_for(&clip()).unwrap().audio);
    }

    #[test]
    fn a_model_that_takes_no_first_frame_refuses_one_before_anything_is_locked() {
        let mut o = offer();
        o.first_frame = false;
        let mut from = clip();
        from.from_image = Some("abc".into());
        assert!(o.shape_for(&from).unwrap_err().contains("cannot start from a picture"));
        assert!(o.shape_for(&clip()).is_ok(), "without one it is an ordinary clip");
        // An offer from a provider that predates the flag is read as taking one.
        let old: VideoOffer = serde_json::from_str(r#"{"durations":[5],"default_seconds":5,"audio":"never","rates":[]}"#).unwrap();
        assert!(old.first_frame);
    }

    #[test]
    fn a_descriptor_without_an_offer_still_reads() {
        let d: ModelDescriptor = serde_json::from_str(r#"{"id":"x","kind":"video","price":{"amount":0.5}}"#).unwrap();
        assert!(d.video.is_none());
        let s = serde_json::to_string(&d).unwrap();
        assert!(!s.contains("video\":"), "absent stays absent on the wire: {s}");
    }

    #[test]
    fn advertised_rates_round_up_to_cents() {
        assert_eq!(ceil_cents(0.141), 0.15);
        assert_eq!(ceil_cents(0.14), 0.14);
        assert_eq!(ceil_cents(0.15), 0.15);
        assert_eq!(ceil_cents(0.3 * 1.15), 0.35);
        assert_eq!(ceil_cents(0.0), 0.0);
        assert_eq!(Price::new(0.141).amount, 0.15);
        let listed = ModelDescriptor {
            id: "m".into(),
            sha256: None,
            kind: crate::JobKind::Llm,
            price: Some(Price {
                amount: 0.141,
                input: Some(0.141),
                output: Some(0.139),
                ..Price::default()
            }),
            video: None,
        };
        assert_eq!(listed.amount(), 0.15);
    }

    #[test]
    fn invoice_and_pay_round_trip() {
        let inv = JobInvoice {
            v: 1,
            job_id: Uuid::nil(),
            amount: 250_000,
            sha256: "ab".into(),
            prompt_tokens: 10,
            completion_tokens: 20,
            cached_tokens: 0,
            top_up: false,
        };
        let raw = serde_json::to_string(&WorkerMessage::JobInvoice(inv.clone())).unwrap();
        match WorkerMessage::parse(&raw).unwrap() {
            WorkerMessage::JobInvoice(got) => assert_eq!(got, inv),
            other => panic!("{other:?}"),
        }

        let pay = JobPay {
            v: 1,
            job_id: Uuid::nil(),
            ticket: crate::payments::SpendTicket {
                client: "0x00000000000000000000000000000000000000a1".into(),
                worker_payout: "0x00000000000000000000000000000000000000b0".into(),
                cumulative: 250_000,
                deadline: 1,
            },
            sig: "0xab".into(),
        };
        let raw = serde_json::to_string(&ClientMessage::JobPay(pay.clone())).unwrap();
        match ClientMessage::parse(&raw).unwrap() {
            ClientMessage::JobPay(got) => assert_eq!(got, pay),
            other => panic!("{other:?}"),
        }
    }
}

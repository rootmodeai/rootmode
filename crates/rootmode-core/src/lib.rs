//! rootmode-core — protocol types, job model, identity and hashing.
//!
//! This crate is deliberately transport-agnostic. The desktop app (or any
//! future worker) speaks [`protocol`] messages over whatever wire it likes;
//! MVP ships a WebSocket client, later versions can swap in libp2p without
//! touching these types.

pub mod canonical;
pub mod hash;
pub mod identity;
pub mod job;
pub mod keyfile;
pub mod payments;
pub mod protocol;
pub mod tokens;

pub use hash::sha256_hex;
pub use identity::Identity;
pub use job::{
    ChatMessage, ImageParams, Job, JobKind, JobPayload, JobStatus, LlmParams, ToolCall, ToolDef,
    VideoParams,
};
pub use protocol::{
    ClientMessage, JobCancel, JobDelta, JobInvoice, JobPay, JobResult, JobResultBody,
    JobStatusUpdate, JobSubmit, ModelDescriptor, PeerAnnounce, Price, WorkerMessage,
    PROTOCOL_VERSION, STOPPED, TOKEN_CHUNK,
};
pub use tokens::TokenUsage;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("unsupported protocol version {got} (this client speaks v{expected})")]
    Version { got: u32, expected: u32 },
    #[error("invalid message: {0}")]
    Invalid(String),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("signature: {0}")]
    Signature(String),
    #[error("key material: {0}")]
    Key(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;

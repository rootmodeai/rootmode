use rootmode_core::CoreError;

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("{0}")]
    Core(#[from] CoreError),
    #[error("config: {0}")]
    Config(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// The local inference backend (vLLM, ComfyUI) failed or is unreachable.
    #[error("backend {backend}: {message}")]
    Backend { backend: String, message: String },
    #[error("network: {0}")]
    Net(String),
    /// The client asked for something this worker will not do.
    #[error("rejected: {0}")]
    Rejected(String),
}

impl WorkerError {
    pub fn backend(backend: impl Into<String>, message: impl std::fmt::Display) -> Self {
        WorkerError::Backend {
            backend: backend.into(),
            message: message.to_string(),
        }
    }
}

pub type Result<T> = std::result::Result<T, WorkerError>;

use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("owned worker cleanup failed: {0}")]
    Cleanup(String),
    #[error("failed to parse message: {0}")]
    Parse(#[from] serde_json::Error),

    #[error("failed to write to worker stdin: {0}")]
    Write(std::io::Error),

    #[error("failed to read from worker stdout: {0}")]
    Read(std::io::Error),

    #[error("worker process exited with code {code}")]
    WorkerExited { code: i32 },

    #[error("worker stdout closed unexpectedly")]
    StdoutClosed,

    #[error("init timeout after {0:?}")]
    InitTimeout(Duration),

    #[error("send timeout after {0:?}")]
    SendTimeout(Duration),

    #[error("shutdown timeout after {0:?}")]
    ShutdownTimeout(Duration),

    #[error("protocol version mismatch: expected {expected}, got {actual}")]
    ProtocolVersionMismatch { expected: String, actual: String },

    /// The worker rejected the initialization request with a structured
    /// protocol error. Unlike an unexpected response, this is a useful
    /// configuration/runtime failure reported by the worker itself.
    #[error("worker rejected init ({code}): {message}")]
    InitRejected {
        code: String,
        message: String,
        retryable: bool,
    },

    #[error("unexpected response type: expected {expected}, got {actual}")]
    UnexpectedResponse { expected: String, actual: String },
}

impl IpcError {
    /// Whether a failure during worker spawn shows that the headless IPC
    /// contract is unusable. Retrying an orb cannot repair this class of
    /// failure, so the supervising daemon must stop for operator attention.
    #[must_use]
    pub const fn is_fatal_init_failure(&self) -> bool {
        matches!(
            self,
            Self::Cleanup(_) | Self::InitRejected { .. }
                | Self::ProtocolVersionMismatch { .. }
                | Self::UnexpectedResponse { .. }
        )
    }
}

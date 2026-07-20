use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
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

    #[error("unexpected response type: expected {expected}, got {actual}")]
    UnexpectedResponse { expected: String, actual: String },
}

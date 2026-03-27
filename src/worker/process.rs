use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::BufReader;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::ipc::error::IpcError;
use crate::ipc::transport::{read_response, write_request};
use crate::ipc::types::{
    ErrorEnvelope, InitConfig, IpcRequest, IpcResponse, ResultStatus, WorkerEvent, PROTOCOL_VERSION,
};

/// Configuration for spawning a worker process.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Path to the worker binary (e.g., heddle-headless).
    pub command: String,
    /// Arguments to pass to the worker binary.
    pub args: Vec<String>,
    /// Working directory for the worker process.
    pub cwd: Option<PathBuf>,
    /// Environment variables to set for the worker process.
    pub env: Vec<(String, String)>,
    /// Model to use for this worker.
    pub model: String,
    /// System prompt for the worker.
    pub system_prompt: String,
    /// Tools available to the worker.
    pub tools: Vec<String>,
    /// Maximum iterations for the agentic loop.
    pub max_iterations: Option<u32>,
    /// Timeout for the init handshake. `None` means no timeout.
    pub init_timeout: Option<Duration>,
    /// Timeout for send (entire response collection). `None` means no timeout.
    pub send_timeout: Option<Duration>,
    /// Timeout for shutdown handshake. `None` means no timeout.
    pub shutdown_timeout: Option<Duration>,
}

/// A running worker process communicating over JSON-line IPC.
pub struct Worker {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    session_id: String,
    send_timeout: Option<Duration>,
    shutdown_timeout: Option<Duration>,
}

impl Worker {
    /// Spawns a new worker process and performs the init handshake.
    ///
    /// # Errors
    ///
    /// Returns `IpcError` if spawning fails, the init handshake fails,
    /// or there's a protocol version mismatch.
    pub async fn spawn(config: &WorkerConfig) -> Result<Self, IpcError> {
        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if let Some(cwd) = &config.cwd {
            cmd.current_dir(cwd);
        }

        for (key, value) in &config.env {
            cmd.env(key, value);
        }

        let mut child = cmd.spawn().map_err(IpcError::Write)?;

        let stdin = child.stdin.take().ok_or(IpcError::StdoutClosed)?;
        let stdout = child.stdout.take().ok_or(IpcError::StdoutClosed)?;
        let reader = BufReader::new(stdout);

        let mut worker = Self {
            child,
            stdin,
            reader,
            session_id: String::new(),
            send_timeout: config.send_timeout,
            shutdown_timeout: config.shutdown_timeout,
        };

        if let Some(dur) = config.init_timeout {
            tokio::time::timeout(dur, worker.init(config))
                .await
                .map_err(|_| IpcError::InitTimeout(dur))??;
        } else {
            worker.init(config).await?;
        }
        Ok(worker)
    }

    /// Sends the init message and validates the response.
    async fn init(&mut self, config: &WorkerConfig) -> Result<(), IpcError> {
        let request = IpcRequest::Init {
            id: "init-1".into(),
            protocol_version: Some(PROTOCOL_VERSION.into()),
            config: InitConfig {
                model: config.model.clone(),
                system_prompt: config.system_prompt.clone(),
                tools: config.tools.clone(),
                max_iterations: config.max_iterations,
                task_id: None,
                worker_id: None,
            },
        };

        write_request(&mut self.stdin, &request).await?;

        let response = read_response(&mut self.reader)
            .await?
            .ok_or(IpcError::StdoutClosed)?;

        match response {
            IpcResponse::InitOk {
                session_id,
                protocol_version,
                error,
                ..
            } => {
                if let Some(ref envelope) = error {
                    return Err(IpcError::UnexpectedResponse {
                        expected: "init_ok without error".into(),
                        actual: envelope.message.clone(),
                    });
                }
                if let Some(version) = &protocol_version {
                    if version != PROTOCOL_VERSION {
                        return Err(IpcError::ProtocolVersionMismatch {
                            expected: PROTOCOL_VERSION.into(),
                            actual: version.clone(),
                        });
                    }
                }
                self.session_id = session_id;
                Ok(())
            }
            IpcResponse::Result {
                status: ResultStatus::Error,
                error,
                ..
            } => Err(IpcError::UnexpectedResponse {
                expected: "init_ok".into(),
                actual: error.map_or_else(|| "unknown error during init".into(), |e| e.message),
            }),
            other => Err(IpcError::UnexpectedResponse {
                expected: "init_ok".into(),
                actual: format!("{other:?}"),
            }),
        }
    }

    /// Sends a message to the worker and collects all events and the final result.
    ///
    /// Returns the collected events and the final `Result` response.
    ///
    /// # Errors
    ///
    /// Returns `IpcError` if writing the request fails, reading responses fails,
    /// or the worker closes stdout unexpectedly.
    pub async fn send(&mut self, id: &str, message: &str) -> Result<SendOutcome, IpcError> {
        if let Some(dur) = self.send_timeout {
            tokio::time::timeout(dur, self.send_inner(id, message))
                .await
                .map_err(|_| IpcError::SendTimeout(dur))?
        } else {
            self.send_inner(id, message).await
        }
    }

    async fn send_inner(&mut self, id: &str, message: &str) -> Result<SendOutcome, IpcError> {
        let request = IpcRequest::Send {
            id: id.into(),
            message: message.into(),
        };

        write_request(&mut self.stdin, &request).await?;

        let mut events = Vec::new();

        loop {
            let response = read_response(&mut self.reader)
                .await?
                .ok_or(IpcError::StdoutClosed)?;

            match response {
                IpcResponse::Event { event, .. } => {
                    events.push(event);
                }
                IpcResponse::Result {
                    id: result_id,
                    status,
                    response,
                    tool_calls_made,
                    usage,
                    iterations,
                    error,
                    ..
                } => {
                    return Ok(SendOutcome {
                        id: result_id,
                        status,
                        response,
                        tool_calls_made,
                        usage,
                        iterations,
                        error,
                        events,
                    });
                }
                other => {
                    return Err(IpcError::UnexpectedResponse {
                        expected: "event or result".into(),
                        actual: format!("{other:?}"),
                    });
                }
            }
        }
    }

    /// Sends a shutdown request and waits for acknowledgment.
    ///
    /// # Errors
    ///
    /// Returns `IpcError` if the shutdown handshake fails.
    pub async fn shutdown(mut self) -> Result<(), IpcError> {
        if let Some(dur) = self.shutdown_timeout {
            tokio::time::timeout(dur, self.shutdown_inner())
                .await
                .map_err(|_| IpcError::ShutdownTimeout(dur))?
        } else {
            self.shutdown_inner().await
        }
    }

    async fn shutdown_inner(&mut self) -> Result<(), IpcError> {
        let request = IpcRequest::Shutdown {
            id: "shutdown-1".into(),
        };

        write_request(&mut self.stdin, &request).await?;

        let response = read_response(&mut self.reader)
            .await?
            .ok_or(IpcError::StdoutClosed)?;

        match response {
            IpcResponse::ShutdownOk { .. } => {
                let _ = self.child.wait().await;
                Ok(())
            }
            other => Err(IpcError::UnexpectedResponse {
                expected: "shutdown_ok".into(),
                actual: format!("{other:?}"),
            }),
        }
    }

    /// Returns the session ID assigned by the worker during init.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

/// The outcome of a `send` call, including streamed events and the final result.
#[derive(Debug)]
pub struct SendOutcome {
    pub id: String,
    pub status: ResultStatus,
    pub response: Option<String>,
    pub tool_calls_made: Vec<crate::ipc::types::ToolCallRecord>,
    pub usage: Option<crate::ipc::types::Usage>,
    pub iterations: u32,
    pub error: Option<ErrorEnvelope>,
    pub events: Vec<WorkerEvent>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_worker_config() -> WorkerConfig {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        WorkerConfig {
            command: "bash".into(),
            args: vec![manifest_dir
                .join("test-fixtures/mock-worker.sh")
                .to_string_lossy()
                .into()],
            cwd: None,
            env: vec![],
            model: "mock/test".into(),
            system_prompt: "You are a test assistant.".into(),
            tools: vec![],
            max_iterations: None,
            init_timeout: None,
            send_timeout: None,
            shutdown_timeout: None,
        }
    }

    fn slow_mock_worker_config(delay_secs: u32) -> WorkerConfig {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        WorkerConfig {
            command: "bash".into(),
            args: vec![manifest_dir
                .join("test-fixtures/mock-worker-slow.sh")
                .to_string_lossy()
                .into()],
            cwd: None,
            env: vec![("MOCK_DELAY".into(), delay_secs.to_string())],
            model: "mock/test".into(),
            system_prompt: "You are a test assistant.".into(),
            tools: vec![],
            max_iterations: None,
            init_timeout: None,
            send_timeout: None,
            shutdown_timeout: None,
        }
    }

    #[tokio::test]
    async fn spawn_and_init_mock_worker() {
        let worker = Worker::spawn(&mock_worker_config()).await.unwrap();
        assert_eq!(worker.session_id(), "mock-sess-001");
    }

    #[tokio::test]
    async fn send_message_to_mock_worker() {
        let mut worker = Worker::spawn(&mock_worker_config()).await.unwrap();
        let outcome = worker.send("msg-1", "hello").await.unwrap();

        assert_eq!(outcome.status, ResultStatus::Ok);
        assert_eq!(outcome.response.as_deref(), Some("Hello from mock worker"));
        assert_eq!(outcome.iterations, 1);
        // Should have received content_delta and usage events
        assert_eq!(outcome.events.len(), 2);
        assert!(matches!(
            &outcome.events[0],
            WorkerEvent::ContentDelta { text } if text == "Hello from mock"
        ));
    }

    #[tokio::test]
    async fn shutdown_mock_worker() {
        let worker = Worker::spawn(&mock_worker_config()).await.unwrap();
        worker.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn full_lifecycle_mock_worker() {
        let mut worker = Worker::spawn(&mock_worker_config()).await.unwrap();
        assert_eq!(worker.session_id(), "mock-sess-001");

        let outcome = worker.send("msg-1", "hello").await.unwrap();
        assert_eq!(outcome.status, ResultStatus::Ok);
        assert!(outcome.response.is_some());
        assert!(outcome.error.is_none());
        assert!(!outcome.events.is_empty());

        worker.shutdown().await.unwrap();
    }

    /// Integration test against real heddle-headless binary.
    /// Only runs when HEDDLE_BINARY is set (skipped in normal test runs).
    #[tokio::test]
    async fn heddle_headless_init_handshake() {
        let binary = match std::env::var("HEDDLE_BINARY") {
            Ok(path) => path,
            Err(_) => return,
        };

        let config = WorkerConfig {
            command: binary,
            args: vec![],
            cwd: None,
            env: vec![],
            model: "openrouter/auto".into(),
            system_prompt: "Say hello".into(),
            tools: vec![],
            max_iterations: Some(1),
            init_timeout: Some(Duration::from_secs(10)),
            send_timeout: None,
            shutdown_timeout: None,
        };

        let worker = Worker::spawn(&config).await.unwrap();
        assert!(!worker.session_id().is_empty());
    }

    /// Full send/receive cycle against real heddle-headless.
    /// Only runs when HEDDLE_BINARY is set (skipped in normal test runs).
    #[tokio::test]
    async fn heddle_headless_send_receive() {
        let binary = match std::env::var("HEDDLE_BINARY") {
            Ok(path) => path,
            Err(_) => return,
        };

        let config = WorkerConfig {
            command: binary,
            args: vec![],
            cwd: None,
            env: vec![],
            model: "openrouter/auto".into(),
            system_prompt: "You are a test assistant. Reply as briefly as possible.".into(),
            tools: vec![],
            max_iterations: Some(1),
            init_timeout: Some(Duration::from_secs(10)),
            send_timeout: Some(Duration::from_secs(30)),
            shutdown_timeout: Some(Duration::from_secs(5)),
        };

        let mut worker = Worker::spawn(&config).await.unwrap();
        assert!(!worker.session_id().is_empty());

        let outcome = worker
            .send("msg-1", "Reply with the word hello")
            .await
            .unwrap();
        assert_eq!(outcome.status, ResultStatus::Ok);
        assert!(outcome.response.is_some(), "Expected a response");
        assert!(!outcome.events.is_empty(), "Expected at least one event");
        assert!(outcome.iterations >= 1);

        worker.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_init_timeout() {
        let mut config = slow_mock_worker_config(5);
        config.init_timeout = Some(Duration::from_millis(100));

        let result: Result<Worker, IpcError> = Worker::spawn(&config).await;
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(
            matches!(err, IpcError::InitTimeout(d) if d == Duration::from_millis(100)),
            "Expected InitTimeout, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn test_send_timeout() {
        // Use the slow mock with delay only on send (init responds immediately)
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let config = WorkerConfig {
            command: "bash".into(),
            args: vec![manifest_dir
                .join("test-fixtures/mock-worker-slow.sh")
                .to_string_lossy()
                .into()],
            cwd: None,
            env: vec![
                ("MOCK_DELAY".into(), "0".into()),
                ("MOCK_SEND_DELAY".into(), "5".into()),
            ],
            model: "mock/test".into(),
            system_prompt: "You are a test assistant.".into(),
            tools: vec![],
            max_iterations: None,
            init_timeout: None,
            send_timeout: Some(Duration::from_millis(100)),
            shutdown_timeout: None,
        };

        let mut worker = Worker::spawn(&config).await.unwrap();
        let result = worker.send("msg-1", "hello").await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, IpcError::SendTimeout(d) if d == Duration::from_millis(100)),
            "Expected SendTimeout, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn test_no_timeout_by_default() {
        // Existing mock_worker_config has None timeouts — should work as before
        let mut worker = Worker::spawn(&mock_worker_config()).await.unwrap();
        let outcome = worker.send("msg-1", "hello").await.unwrap();
        assert_eq!(outcome.status, ResultStatus::Ok);
        worker.shutdown().await.unwrap();
    }
}

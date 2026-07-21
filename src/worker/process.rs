use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::BufReader;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;
use tokio::sync::Mutex as TokioMutex;
use tokio_util::sync::CancellationToken;

use tracing::{debug, info, instrument};

use crate::ipc::error::IpcError;
use crate::ipc::transport::{read_response, write_request};
use crate::ipc::types::{
    AppAttribution, ErrorEnvelope, InitConfig, IpcRequest, IpcResponse, ResultStatus, WorkerEvent,
    PROTOCOL_VERSION,
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
    /// Task ID to send in init config (for trace correlation).
    pub task_id: Option<String>,
    /// Worker ID to send in init config (for trace correlation).
    pub worker_id: Option<String>,
}

/// A running worker process communicating over JSON-line IPC.
#[allow(clippy::struct_field_names)]
pub struct Worker {
    child: Child,
    stdin: Arc<TokioMutex<ChildStdin>>,
    reader: BufReader<ChildStdout>,
    session_id: String,
    /// Orboros-side correlation id, distinct from the heddle-side
    /// `session_id`. Used as a `tracing` span field.
    worker_id: crate::tracing_ctx::WorkerId,
    send_timeout: Option<Duration>,
    shutdown_timeout: Option<Duration>,
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
    /// Model latency reported by the harness (ms).
    pub model_latency_ms: Option<u64>,
    /// Tool latency reported by the harness (ms).
    pub tool_latency_ms: Option<u64>,
    /// Total latency reported by the harness (ms).
    pub total_latency_ms: Option<u64>,
    /// Worker-reported confidence in the result (0.0–1.0), clamped on the
    /// orchestrator side. None when the worker doesn't report one.
    pub confidence: Option<f32>,
}

/// Handle for sending a cancel request to a running worker.
/// Clone-able so it can be held by timeout/budget watchers.
#[derive(Clone)]
pub struct CancelSender {
    stdin: Arc<TokioMutex<ChildStdin>>,
    session_id: String,
}

impl CancelSender {
    /// Sends an IPC cancel request for the given target send ID.
    ///
    /// # Errors
    ///
    /// Returns `IpcError` if writing the cancel request fails.
    pub async fn cancel(&self, target_id: &str) -> Result<(), IpcError> {
        let request = IpcRequest::Cancel {
            id: format!("cancel-{target_id}"),
            target_id: target_id.into(),
        };
        let mut stdin = self.stdin.lock().await;
        write_request(&mut stdin, &request).await
    }

    /// Returns the session ID of the worker.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

impl Worker {
    /// Spawns a new worker process and performs the init handshake.
    ///
    /// # Errors
    ///
    /// Returns `IpcError` if spawning fails, the init handshake fails,
    /// or there's a protocol version mismatch.
    #[instrument(
        name = "worker.spawn",
        skip(config),
        fields(
            command = %config.command,
            model = %config.model,
            worker_id = tracing::field::Empty,
            session_id = tracing::field::Empty,
        )
    )]
    pub async fn spawn(config: &WorkerConfig) -> Result<Self, IpcError> {
        let worker_id = crate::tracing_ctx::WorkerId::new();
        tracing::Span::current().record("worker_id", tracing::field::display(&worker_id));
        debug!("spawning worker subprocess");

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

        let stdin = Arc::new(TokioMutex::new(
            child.stdin.take().ok_or(IpcError::StdoutClosed)?,
        ));
        let stdout = child.stdout.take().ok_or(IpcError::StdoutClosed)?;
        let reader = BufReader::new(stdout);

        let mut worker = Self {
            child,
            stdin,
            reader,
            session_id: String::new(),
            worker_id,
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
        tracing::Span::current().record("session_id", tracing::field::display(&worker.session_id));
        info!(
            session_id = %worker.session_id,
            task_id = ?config.task_id,
            worker_id = ?config.worker_id,
            model = %config.model,
            "worker ready",
        );
        Ok(worker)
    }

    /// Returns the orboros-side correlation id for this worker.
    #[must_use]
    pub fn worker_id(&self) -> crate::tracing_ctx::WorkerId {
        self.worker_id
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
                task_id: config.task_id.clone(),
                worker_id: config.worker_id.clone(),
                app_attribution: Some(AppAttribution {
                    referer: "https://github.com/gitschwifty/orboros".into(),
                    title: "Orboros".into(),
                    categories: Some("cli-agent".into()),
                }),
            },
        };

        {
            let mut stdin = self.stdin.lock().await;
            write_request(&mut stdin, &request).await?;
        }

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
    #[instrument(
        name = "worker.send",
        skip(self, message),
        fields(worker_id = %self.worker_id, session_id = %self.session_id, send_id = %id)
    )]
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
        self.send_inner_with_tx(id, message, None).await
    }

    /// Like `send()`, but forwards each `WorkerEvent` to `event_tx` as it arrives.
    ///
    /// The returned `SendOutcome` still contains the full event vec for callers
    /// that want the totals together with the streamed feed (transcript
    /// persistence, debugging). If the receiver is dropped mid-turn, the
    /// remaining events are still collected into the outcome but not forwarded.
    ///
    /// # Errors
    ///
    /// Returns `IpcError` if writing the send request fails, reading responses
    /// fails, the worker closes stdout unexpectedly, or `send_timeout` elapses.
    #[instrument(
        name = "worker.send_streaming",
        skip(self, message, event_tx),
        fields(worker_id = %self.worker_id, session_id = %self.session_id, send_id = %id)
    )]
    pub async fn send_streaming(
        &mut self,
        id: &str,
        message: &str,
        event_tx: mpsc::Sender<WorkerEvent>,
    ) -> Result<SendOutcome, IpcError> {
        if let Some(dur) = self.send_timeout {
            tokio::time::timeout(dur, self.send_inner_with_tx(id, message, Some(event_tx)))
                .await
                .map_err(|_| IpcError::SendTimeout(dur))?
        } else {
            self.send_inner_with_tx(id, message, Some(event_tx)).await
        }
    }

    async fn send_inner_with_tx(
        &mut self,
        id: &str,
        message: &str,
        mut event_tx: Option<mpsc::Sender<WorkerEvent>>,
    ) -> Result<SendOutcome, IpcError> {
        let request = IpcRequest::Send {
            id: id.into(),
            message: message.into(),
        };

        {
            let mut stdin = self.stdin.lock().await;
            write_request(&mut stdin, &request).await?;
        }

        let mut events = Vec::new();

        loop {
            let response = read_response(&mut self.reader)
                .await?
                .ok_or(IpcError::StdoutClosed)?;

            match response {
                IpcResponse::Event { event, .. } => {
                    if let Some(tx) = event_tx.as_ref() {
                        // Drop the forwarder if the receiver hangs up, but
                        // keep collecting into `events` so the outcome stays
                        // complete.
                        if tx.send(event.clone()).await.is_err() {
                            event_tx = None;
                        }
                    }
                    events.push(event);
                }
                IpcResponse::Result {
                    id: result_id,
                    status,
                    mut response,
                    tool_calls_made,
                    usage,
                    iterations,
                    error,
                    model_latency_ms,
                    tool_latency_ms,
                    total_latency_ms,
                    confidence,
                    ..
                } => {
                    let confidence = resolve_confidence(confidence, &mut response);
                    return Ok(SendOutcome {
                        id: result_id,
                        status,
                        response,
                        tool_calls_made,
                        usage,
                        iterations,
                        error,
                        events,
                        model_latency_ms,
                        tool_latency_ms,
                        total_latency_ms,
                        confidence,
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
    #[instrument(
        name = "worker.shutdown",
        skip(self),
        fields(worker_id = %self.worker_id, session_id = %self.session_id)
    )]
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

        {
            let mut stdin = self.stdin.lock().await;
            write_request(&mut stdin, &request).await?;
        }

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

    /// Attempts graceful shutdown within the grace period, then kills the process.
    /// Always succeeds — errors from shutdown are swallowed.
    pub async fn force_stop(mut self, grace: Duration) {
        let ok = matches!(
            tokio::time::timeout(grace, self.shutdown_inner()).await,
            Ok(Ok(()))
        );
        if !ok {
            let _ = self.child.kill().await;
        }
    }

    /// Returns the session ID assigned by the worker during init.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Returns a `CancelSender` that can send cancel requests to this worker.
    pub fn cancel_sender(&self) -> CancelSender {
        CancelSender {
            stdin: Arc::clone(&self.stdin),
            session_id: self.session_id.clone(),
        }
    }

    /// Like `send()`, but races against a `CancellationToken`.
    /// If the token fires, sends a cancel request and returns the cancelled result.
    ///
    /// # Errors
    ///
    /// Returns `IpcError` if the send fails or the worker does not respond to cancel.
    pub async fn send_cancellable(
        &mut self,
        id: &str,
        message: &str,
        token: CancellationToken,
    ) -> Result<SendOutcome, IpcError> {
        let cancel_sender = self.cancel_sender();
        let id_owned = id.to_string();

        tokio::select! {
            result = self.send(&id_owned, message) => result,
            () = token.cancelled() => {
                // Token fired — try to cancel the in-flight send
                let _ = cancel_sender.cancel(&id_owned).await;
                // Read the response (should be a cancelled result)
                // Give a short timeout for the cancel response
                match tokio::time::timeout(
                    Duration::from_secs(2),
                    self.read_remaining_response()
                ).await {
                    Ok(Ok(outcome)) => Ok(outcome),
                    _ => Err(IpcError::StdoutClosed),
                }
            }
        }
    }

    /// Reads remaining response after a cancel -- collects events until result.
    async fn read_remaining_response(&mut self) -> Result<SendOutcome, IpcError> {
        let mut events = Vec::new();
        loop {
            let response = read_response(&mut self.reader)
                .await?
                .ok_or(IpcError::StdoutClosed)?;
            match response {
                IpcResponse::Event { event, .. } => events.push(event),
                IpcResponse::Result {
                    id,
                    status,
                    mut response,
                    tool_calls_made,
                    usage,
                    iterations,
                    error,
                    model_latency_ms,
                    tool_latency_ms,
                    total_latency_ms,
                    confidence,
                    ..
                } => {
                    let confidence = resolve_confidence(confidence, &mut response);
                    return Ok(SendOutcome {
                        id,
                        status,
                        response,
                        tool_calls_made,
                        usage,
                        iterations,
                        error,
                        events,
                        model_latency_ms,
                        tool_latency_ms,
                        total_latency_ms,
                        confidence,
                    });
                }
                _ => {} // ignore other responses during cancel drain
            }
        }
    }
}

/// Clamps worker-reported confidence into `[0.0, 1.0]`. Returns `None` for
/// out-of-range or non-finite values with a `warn!` — a buggy model
/// emitting `1.2` or `NaN` shouldn't fail the whole orb.
fn clamp_confidence(value: Option<f32>) -> Option<f32> {
    let v = value?;
    if !v.is_finite() || !(0.0..=1.0).contains(&v) {
        tracing::warn!(
            value = v,
            "worker-reported confidence out of range [0.0, 1.0]; discarding"
        );
        return None;
    }
    Some(v)
}

/// Resolves the final confidence value for a `SendOutcome`.
///
/// Prefers the structured IPC field when present. Otherwise scans the
/// response for a trailing `CONFIDENCE: 0.NN` line (case-insensitive)
/// and, if found, removes the line from `response`. Mirrors the same
/// out-of-range handling as [`clamp_confidence`].
///
/// Older heddle workers won't include the IPC field, so the line
/// parser is the forward-compatibility path until the protocol bump
/// makes it required.
pub(crate) fn resolve_confidence(
    ipc_value: Option<f32>,
    response: &mut Option<String>,
) -> Option<f32> {
    if let Some(v) = clamp_confidence(ipc_value) {
        return Some(v);
    }
    let body = response.as_mut()?;
    let (value, rewritten) = extract_confidence_line(body)?;
    *body = rewritten;
    clamp_confidence(Some(value))
}

/// Finds the last `CONFIDENCE: N.NN` line in `text` (case-insensitive
/// on the label, lenient on whitespace) and returns the parsed value
/// plus the text with that line removed. Trailing whitespace is also
/// trimmed off the result.
fn extract_confidence_line(text: &str) -> Option<(f32, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let idx = lines.iter().rposition(|line| {
        let trimmed = line.trim_start();
        trimmed.len() >= 11 && trimmed.as_bytes()[..11].eq_ignore_ascii_case(b"confidence:")
    })?;
    let value_str = lines[idx].trim_start()[11..].trim();
    let value: f32 = value_str.parse().ok()?;
    let mut kept: Vec<&str> = Vec::with_capacity(lines.len() - 1);
    kept.extend_from_slice(&lines[..idx]);
    kept.extend_from_slice(&lines[idx + 1..]);
    let mut out = kept.join("\n");
    let trimmed_end = out.trim_end().len();
    out.truncate(trimmed_end);
    Some((value, out))
}

/// System-prompt addendum asking the worker to self-report confidence
/// as a trailing `CONFIDENCE: 0.NN` line. Append after any task-specific
/// instructions so the line ends up at the bottom of the response.
pub const CONFIDENCE_PROMPT_ADDENDUM: &str = "\n\nAt the end of your response, on its own line, include a single line in this exact format:\n  CONFIDENCE: 0.NN\nwhere 0.NN is your confidence between 0.00 (no confidence) and 1.00 (certain) that the result satisfies the task. Be honest — low confidence routes the result for a second-opinion review.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_confidence_passes_in_range_values() {
        assert_eq!(clamp_confidence(Some(0.0)), Some(0.0));
        assert_eq!(clamp_confidence(Some(0.5)), Some(0.5));
        assert_eq!(clamp_confidence(Some(1.0)), Some(1.0));
    }

    #[test]
    fn clamp_confidence_rejects_out_of_range() {
        assert_eq!(clamp_confidence(Some(-0.1)), None);
        assert_eq!(clamp_confidence(Some(1.5)), None);
    }

    #[test]
    fn clamp_confidence_rejects_non_finite() {
        assert_eq!(clamp_confidence(Some(f32::NAN)), None);
        assert_eq!(clamp_confidence(Some(f32::INFINITY)), None);
        assert_eq!(clamp_confidence(Some(f32::NEG_INFINITY)), None);
    }

    #[test]
    fn clamp_confidence_passes_through_none() {
        assert_eq!(clamp_confidence(None), None);
    }

    #[test]
    fn extract_confidence_line_parses_trailing_line() {
        let text = "Here is the answer.\nIt is 42.\nCONFIDENCE: 0.85";
        let (value, rest) = extract_confidence_line(text).unwrap();
        assert!((value - 0.85).abs() < f32::EPSILON);
        assert_eq!(rest, "Here is the answer.\nIt is 42.");
    }

    #[test]
    fn extract_confidence_line_is_case_insensitive_on_label() {
        let text = "Result.\nconfidence: 0.5";
        let (value, rest) = extract_confidence_line(text).unwrap();
        assert!((value - 0.5).abs() < f32::EPSILON);
        assert_eq!(rest, "Result.");
    }

    #[test]
    fn extract_confidence_line_uses_last_match_when_multiple() {
        let text = "CONFIDENCE: 0.1 (a guess)\nFinal answer.\nCONFIDENCE: 0.9";
        let (value, rest) = extract_confidence_line(text).unwrap();
        assert!((value - 0.9).abs() < f32::EPSILON);
        assert!(rest.contains("CONFIDENCE: 0.1 (a guess)"));
        assert!(rest.ends_with("Final answer."));
    }

    #[test]
    fn extract_confidence_line_returns_none_when_missing() {
        assert!(extract_confidence_line("no confidence here").is_none());
    }

    #[test]
    fn extract_confidence_line_returns_none_for_unparseable_value() {
        assert!(extract_confidence_line("CONFIDENCE: high").is_none());
    }

    #[test]
    fn resolve_confidence_prefers_ipc_value() {
        let mut response = Some("body\nCONFIDENCE: 0.1".to_string());
        let resolved = resolve_confidence(Some(0.9), &mut response);
        assert_eq!(resolved, Some(0.9));
        // Response is NOT modified when IPC field wins.
        assert_eq!(response.as_deref(), Some("body\nCONFIDENCE: 0.1"));
    }

    #[test]
    fn resolve_confidence_falls_back_to_line_parser() {
        let mut response = Some("body\nCONFIDENCE: 0.42".to_string());
        let resolved = resolve_confidence(None, &mut response);
        assert_eq!(resolved, Some(0.42));
        assert_eq!(response.as_deref(), Some("body"));
    }

    #[test]
    fn resolve_confidence_clamps_line_parsed_value() {
        let mut response = Some("body\nCONFIDENCE: 1.5".to_string());
        let resolved = resolve_confidence(None, &mut response);
        // Out-of-range discarded; line is still stripped (the parser
        // matched it, value was just bad).
        assert_eq!(resolved, None);
        assert_eq!(response.as_deref(), Some("body"));
    }

    #[test]
    fn resolve_confidence_returns_none_when_no_signal_anywhere() {
        let mut response = Some("plain response".to_string());
        let resolved = resolve_confidence(None, &mut response);
        assert_eq!(resolved, None);
        assert_eq!(response.as_deref(), Some("plain response"));
    }

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
            task_id: None,
            worker_id: None,
        }
    }

    fn confidence_mock_worker_config() -> WorkerConfig {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        WorkerConfig {
            command: "bash".into(),
            args: vec![manifest_dir
                .join("test-fixtures/mock-worker-confidence.sh")
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
            task_id: None,
            worker_id: None,
        }
    }

    #[tokio::test]
    async fn confidence_from_ipc_field_flows_into_send_outcome() {
        let config = confidence_mock_worker_config();
        let mut worker = Worker::spawn(&config).await.unwrap();
        let outcome = worker.send("req-1", "ping").await.unwrap();
        assert_eq!(outcome.confidence, Some(0.73));
        let _ = worker.shutdown().await;
    }

    fn cancel_mock_worker_config() -> WorkerConfig {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        WorkerConfig {
            command: "bash".into(),
            args: vec![manifest_dir
                .join("test-fixtures/mock-worker-cancel.sh")
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
            task_id: None,
            worker_id: None,
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
            task_id: None,
            worker_id: None,
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
    /// Only runs when `HEDDLE_BINARY` is set (skipped in normal test runs).
    #[tokio::test]
    async fn heddle_headless_init_handshake() {
        let Ok(binary) = std::env::var("HEDDLE_BINARY") else {
            return;
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
            task_id: None,
            worker_id: None,
        };

        let worker = Worker::spawn(&config).await.unwrap();
        assert!(!worker.session_id().is_empty());
    }

    /// Full send/receive cycle against real heddle-headless.
    /// Only runs when `HEDDLE_BINARY` is set (skipped in normal test runs).
    #[tokio::test]
    async fn heddle_headless_send_receive() {
        let Ok(binary) = std::env::var("HEDDLE_BINARY") else {
            return;
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
            task_id: None,
            worker_id: None,
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
            task_id: None,
            worker_id: None,
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

    // ---- Step 1: CancelSender tests ----

    #[tokio::test]
    async fn cancel_sender_is_clone() {
        let worker = Worker::spawn(&mock_worker_config()).await.unwrap();
        let cancel_sender = worker.cancel_sender();
        let _cloned = cancel_sender.clone();
        assert_eq!(cancel_sender.session_id(), "mock-sess-001");
        worker.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancel_during_send() {
        let mut worker = Worker::spawn(&cancel_mock_worker_config()).await.unwrap();
        assert_eq!(worker.session_id(), "cancel-sess-001");

        let cancel_sender = worker.cancel_sender();

        // Start send in a spawned task — the cancel mock won't respond until cancel arrives
        let send_handle = tokio::spawn(async move { worker.send("msg-1", "hello").await });

        // Give the send a moment to write the request
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Send cancel
        cancel_sender.cancel("msg-1").await.unwrap();

        // The send should now complete with a cancelled result
        let outcome = send_handle.await.unwrap().unwrap();
        assert_eq!(outcome.status, ResultStatus::Cancelled);
        assert!(
            outcome
                .error
                .as_ref()
                .is_some_and(|e| e.code == "cancelled"),
            "Expected cancelled error, got: {:?}",
            outcome.error
        );
    }

    #[tokio::test]
    async fn normal_send_works_with_arc_stdin() {
        // Validates the refactor: existing mock worker still works with Arc<Mutex> stdin
        let mut worker = Worker::spawn(&mock_worker_config()).await.unwrap();
        let outcome = worker.send("msg-1", "hello").await.unwrap();
        assert_eq!(outcome.status, ResultStatus::Ok);
        assert_eq!(outcome.response.as_deref(), Some("Hello from mock worker"));
        worker.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancel_after_completion_is_harmless() {
        let mut worker = Worker::spawn(&mock_worker_config()).await.unwrap();
        let cancel_sender = worker.cancel_sender();

        // Complete a normal send
        let outcome = worker.send("msg-1", "hello").await.unwrap();
        assert_eq!(outcome.status, ResultStatus::Ok);

        // Cancel after completion — should not panic, may fail with write error (ok)
        let cancel_result = cancel_sender.cancel("msg-1").await;
        // Either Ok (wrote but worker ignored) or Err (broken pipe) — both fine
        drop(cancel_result);

        worker.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn worker_force_stop_graceful() {
        let worker = Worker::spawn(&mock_worker_config()).await.unwrap();
        // force_stop with generous grace should shutdown gracefully
        worker.force_stop(Duration::from_secs(5)).await;
        // No panic = success
    }

    #[tokio::test]
    async fn worker_force_stop_kills_on_timeout() {
        // Use a worker that won't respond to shutdown (mock-worker-slow with long delay)
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
                ("MOCK_SEND_DELAY".into(), "0".into()),
            ],
            model: "mock/slow".into(),
            system_prompt: "test".into(),
            tools: vec![],
            max_iterations: None,
            init_timeout: None,
            send_timeout: None,
            shutdown_timeout: None,
            task_id: None,
            worker_id: None,
        };
        let worker = Worker::spawn(&config).await.unwrap();
        // Very short grace period — should trigger kill
        worker.force_stop(Duration::from_millis(50)).await;
        // No panic/hang = success
    }

    #[tokio::test]
    async fn send_streaming_forwards_events_in_order_and_returns_outcome() {
        let mut worker = Worker::spawn(&mock_worker_config()).await.unwrap();
        let (tx, mut rx) = mpsc::channel(8);

        let send_fut = worker.send_streaming("msg-stream", "hello", tx);
        let drain_fut = async {
            let mut received = Vec::new();
            while let Some(ev) = rx.recv().await {
                received.push(ev);
            }
            received
        };

        let (outcome_res, received) = tokio::join!(send_fut, drain_fut);
        let outcome = outcome_res.unwrap();

        // Outcome matches the non-streaming `send` semantics.
        assert_eq!(outcome.status, ResultStatus::Ok);
        assert_eq!(outcome.response.as_deref(), Some("Hello from mock worker"));
        assert_eq!(outcome.events.len(), 2);

        // Streamed events arrived in the same order and same count.
        assert_eq!(received.len(), 2);
        assert!(matches!(
            &received[0],
            WorkerEvent::ContentDelta { text } if text == "Hello from mock"
        ));
        assert!(matches!(received[1], WorkerEvent::Usage { .. }));

        worker.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn send_streaming_completes_even_if_receiver_dropped() {
        let mut worker = Worker::spawn(&mock_worker_config()).await.unwrap();
        let (tx, rx) = mpsc::channel::<WorkerEvent>(8);
        // Drop the receiver immediately — sender side should see channel
        // closure on first event and stop forwarding without erroring the send.
        drop(rx);

        let outcome = worker
            .send_streaming("msg-drop-rx", "hello", tx)
            .await
            .unwrap();
        assert_eq!(outcome.status, ResultStatus::Ok);
        // Events are still collected in the outcome despite no consumer.
        assert_eq!(outcome.events.len(), 2);

        worker.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn send_streaming_after_send_uses_same_worker_for_multi_turn() {
        // Demonstrates that streaming and non-streaming sends can be
        // interleaved on a single worker — the precondition for the
        // conversational session loop.
        let mut worker = Worker::spawn(&mock_worker_config()).await.unwrap();

        let first = worker.send("msg-1", "hello").await.unwrap();
        assert_eq!(first.status, ResultStatus::Ok);

        let (tx, mut rx) = mpsc::channel(8);
        let (outcome, received) =
            tokio::join!(worker.send_streaming("msg-2", "world", tx), async {
                let mut got = Vec::new();
                while let Some(ev) = rx.recv().await {
                    got.push(ev);
                }
                got
            });
        let outcome = outcome.unwrap();
        assert_eq!(outcome.status, ResultStatus::Ok);
        assert_eq!(received.len(), 2);

        worker.shutdown().await.unwrap();
    }
}

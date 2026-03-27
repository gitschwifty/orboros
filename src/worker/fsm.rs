use tokio_util::sync::CancellationToken;

use crate::ipc::error::IpcError;
use crate::worker::process::{CancelSender, SendOutcome, Worker, WorkerConfig};

// ---------------------------------------------------------------------------
// Failure taxonomy
// ---------------------------------------------------------------------------

/// Phase in which a timeout occurred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutPhase {
    Init,
    Send,
    Shutdown,
}

/// Classification of a worker failure. Every `IpcError` maps to exactly one class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureClass {
    /// Protocol-level error (parse, version mismatch, unexpected response).
    Protocol { message: String },
    /// A timeout in a specific phase.
    Timeout { phase: TimeoutPhase },
    /// Worker process crashed or I/O failure.
    Crash { message: String },
    /// Task was cancelled.
    Cancelled,
}

impl From<&IpcError> for FailureClass {
    fn from(err: &IpcError) -> Self {
        match err {
            IpcError::Parse(e) => FailureClass::Protocol {
                message: e.to_string(),
            },
            IpcError::ProtocolVersionMismatch { expected, actual } => FailureClass::Protocol {
                message: format!("version mismatch: expected {expected}, got {actual}"),
            },
            IpcError::UnexpectedResponse { expected, actual } => FailureClass::Protocol {
                message: format!("expected {expected}, got {actual}"),
            },
            IpcError::InitTimeout(_) => FailureClass::Timeout {
                phase: TimeoutPhase::Init,
            },
            IpcError::SendTimeout(_) => FailureClass::Timeout {
                phase: TimeoutPhase::Send,
            },
            IpcError::ShutdownTimeout(_) => FailureClass::Timeout {
                phase: TimeoutPhase::Shutdown,
            },
            IpcError::Write(e) | IpcError::Read(e) => FailureClass::Crash {
                message: e.to_string(),
            },
            IpcError::WorkerExited { code } => FailureClass::Crash {
                message: format!("exited with code {code}"),
            },
            IpcError::StdoutClosed => FailureClass::Crash {
                message: "stdout closed".into(),
            },
        }
    }
}

/// Whether (and how) a failed worker should be restarted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPolicy {
    /// Do not restart — the error is deterministic.
    None,
    /// Retry once — the error may be transient.
    RetryOnce,
}

impl FailureClass {
    /// Returns the restart policy for this failure class.
    pub fn restart_policy(&self) -> RestartPolicy {
        match self {
            FailureClass::Protocol { .. } | FailureClass::Cancelled => RestartPolicy::None,
            FailureClass::Timeout { .. } | FailureClass::Crash { .. } => RestartPolicy::RetryOnce,
        }
    }
}

// ---------------------------------------------------------------------------
// Worker state machine
// ---------------------------------------------------------------------------

/// Why the worker stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    Clean,
    Failed(FailureClass),
}

/// States in the worker lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerState {
    Idle,
    Initializing,
    Ready,
    Running,
    Draining,
    Stopped(StopReason),
}

/// Errors from invalid FSM transitions or worker failures.
#[derive(Debug, thiserror::Error)]
pub enum FsmError {
    #[error("invalid transition: cannot {action} from {current:?}")]
    InvalidTransition {
        current: WorkerState,
        action: String,
    },

    #[error("worker failed: {0:?}")]
    WorkerFailed(FailureClass),
}

/// State machine wrapper around [`Worker`].
///
/// Enforces lifecycle ordering (Idle → Ready → Running → Stopped) and
/// classifies every failure for downstream retry logic.
pub struct WorkerFsm {
    state: WorkerState,
    config: WorkerConfig,
    worker: Option<Worker>,
    last_outcome: Option<SendOutcome>,
    failure: Option<FailureClass>,
}

impl WorkerFsm {
    /// Creates a new FSM in the `Idle` state.
    pub fn new(config: WorkerConfig) -> Self {
        Self {
            state: WorkerState::Idle,
            config,
            worker: None,
            last_outcome: None,
            failure: None,
        }
    }

    /// Returns the current state.
    pub fn state(&self) -> &WorkerState {
        &self.state
    }

    /// Returns the session ID if the worker is initialized.
    pub fn session_id(&self) -> Option<&str> {
        self.worker.as_ref().map(Worker::session_id)
    }

    /// Returns the failure class if the worker is stopped due to a failure.
    pub fn failure(&self) -> Option<&FailureClass> {
        self.failure.as_ref()
    }

    /// Returns the outcome of the last `send` call, if any.
    pub fn last_outcome(&self) -> Option<&SendOutcome> {
        self.last_outcome.as_ref()
    }

    /// Returns a `CancelSender` if the worker is alive (has a Worker instance).
    pub fn cancel_handle(&self) -> Option<CancelSender> {
        self.worker.as_ref().map(Worker::cancel_sender)
    }

    /// Spawns the worker process and performs the init handshake.
    ///
    /// Transitions: `Idle` → `Initializing` → `Ready` (or `Stopped` on failure).
    ///
    /// # Errors
    ///
    /// Returns `FsmError::InvalidTransition` if not in `Idle` state.
    /// Returns `FsmError::WorkerFailed` if spawn or init fails.
    pub async fn start(&mut self) -> Result<(), FsmError> {
        if self.state != WorkerState::Idle {
            return Err(FsmError::InvalidTransition {
                current: self.state.clone(),
                action: "start".into(),
            });
        }

        self.state = WorkerState::Initializing;

        match Worker::spawn(&self.config).await {
            Ok(worker) => {
                self.worker = Some(worker);
                self.state = WorkerState::Ready;
                Ok(())
            }
            Err(e) => {
                let class = FailureClass::from(&e);
                self.failure = Some(class.clone());
                self.state = WorkerState::Stopped(StopReason::Failed(class.clone()));
                Err(FsmError::WorkerFailed(class))
            }
        }
    }

    /// Sends a message to the worker and collects the response.
    ///
    /// Transitions: `Ready` → `Running` → `Ready` (or `Stopped` on failure).
    ///
    /// # Errors
    ///
    /// Returns `FsmError::InvalidTransition` if not in `Ready` state.
    /// Returns `FsmError::WorkerFailed` if the send fails.
    ///
    /// # Panics
    ///
    /// Panics if internal state is inconsistent (worker missing while in `Ready`).
    pub async fn send(&mut self, id: &str, message: &str) -> Result<&SendOutcome, FsmError> {
        if self.state != WorkerState::Ready {
            return Err(FsmError::InvalidTransition {
                current: self.state.clone(),
                action: "send".into(),
            });
        }

        self.state = WorkerState::Running;

        let worker = self.worker.as_mut().expect("worker must exist in Ready");
        match worker.send(id, message).await {
            Ok(outcome) => {
                self.last_outcome = Some(outcome);
                self.state = WorkerState::Ready;
                Ok(self.last_outcome.as_ref().unwrap())
            }
            Err(e) => {
                let class = FailureClass::from(&e);
                self.failure = Some(class.clone());
                self.state = WorkerState::Stopped(StopReason::Failed(class.clone()));
                Err(FsmError::WorkerFailed(class))
            }
        }
    }

    /// Sends a message with cancellation support.
    ///
    /// Transitions: `Ready` → `Running` → `Ready` (or `Stopped` on failure).
    ///
    /// # Errors
    ///
    /// Returns `FsmError::InvalidTransition` if not in `Ready` state.
    /// Returns `FsmError::WorkerFailed` if the send fails.
    ///
    /// # Panics
    ///
    /// Panics if internal state is inconsistent (worker missing while in `Ready`).
    pub async fn send_cancellable(
        &mut self,
        id: &str,
        message: &str,
        token: CancellationToken,
    ) -> Result<&SendOutcome, FsmError> {
        if self.state != WorkerState::Ready {
            return Err(FsmError::InvalidTransition {
                current: self.state.clone(),
                action: "send_cancellable".into(),
            });
        }
        self.state = WorkerState::Running;
        let worker = self.worker.as_mut().expect("worker must exist in Ready");
        match worker.send_cancellable(id, message, token).await {
            Ok(outcome) => {
                self.last_outcome = Some(outcome);
                self.state = WorkerState::Ready;
                Ok(self.last_outcome.as_ref().unwrap())
            }
            Err(e) => {
                let class = FailureClass::from(&e);
                self.failure = Some(class.clone());
                self.state = WorkerState::Stopped(StopReason::Failed(class.clone()));
                Err(FsmError::WorkerFailed(class))
            }
        }
    }

    /// Shuts down the worker gracefully.
    ///
    /// Transitions: `Ready` → `Draining` → `Stopped(Clean)` (or `Stopped(Failed)`).
    ///
    /// # Errors
    ///
    /// Returns `FsmError::InvalidTransition` if not in `Ready` state.
    /// Returns `FsmError::WorkerFailed` if the shutdown fails.
    ///
    /// # Panics
    ///
    /// Panics if internal state is inconsistent (worker missing while in `Ready`).
    pub async fn stop(&mut self) -> Result<(), FsmError> {
        if self.state != WorkerState::Ready {
            return Err(FsmError::InvalidTransition {
                current: self.state.clone(),
                action: "stop".into(),
            });
        }

        self.state = WorkerState::Draining;

        let worker = self.worker.take().expect("worker must exist in Ready");
        match worker.shutdown().await {
            Ok(()) => {
                self.state = WorkerState::Stopped(StopReason::Clean);
                Ok(())
            }
            Err(e) => {
                let class = FailureClass::from(&e);
                self.failure = Some(class.clone());
                self.state = WorkerState::Stopped(StopReason::Failed(class.clone()));
                Err(FsmError::WorkerFailed(class))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    // ---- helpers ----------------------------------------------------------

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

    fn bad_binary_config() -> WorkerConfig {
        WorkerConfig {
            command: "/nonexistent/binary".into(),
            args: vec![],
            cwd: None,
            env: vec![],
            model: "bad/model".into(),
            system_prompt: "test".into(),
            tools: vec![],
            max_iterations: None,
            init_timeout: None,
            send_timeout: None,
            shutdown_timeout: None,
        }
    }

    // ---- Step 1: Failure classification tests ----------------------------

    #[test]
    fn classify_parse_error() {
        let raw = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        let err = IpcError::Parse(raw);
        let class = FailureClass::from(&err);
        assert!(matches!(class, FailureClass::Protocol { .. }));
    }

    #[test]
    fn classify_protocol_version_mismatch() {
        let err = IpcError::ProtocolVersionMismatch {
            expected: "0.2.0".into(),
            actual: "0.1.0".into(),
        };
        let class = FailureClass::from(&err);
        assert!(matches!(class, FailureClass::Protocol { .. }));
    }

    #[test]
    fn classify_unexpected_response() {
        let err = IpcError::UnexpectedResponse {
            expected: "init_ok".into(),
            actual: "result".into(),
        };
        let class = FailureClass::from(&err);
        assert!(matches!(class, FailureClass::Protocol { .. }));
    }

    #[test]
    fn classify_init_timeout() {
        let err = IpcError::InitTimeout(Duration::from_secs(5));
        let class = FailureClass::from(&err);
        assert_eq!(
            class,
            FailureClass::Timeout {
                phase: TimeoutPhase::Init
            }
        );
    }

    #[test]
    fn classify_send_timeout() {
        let err = IpcError::SendTimeout(Duration::from_secs(30));
        let class = FailureClass::from(&err);
        assert_eq!(
            class,
            FailureClass::Timeout {
                phase: TimeoutPhase::Send
            }
        );
    }

    #[test]
    fn classify_shutdown_timeout() {
        let err = IpcError::ShutdownTimeout(Duration::from_secs(5));
        let class = FailureClass::from(&err);
        assert_eq!(
            class,
            FailureClass::Timeout {
                phase: TimeoutPhase::Shutdown
            }
        );
    }

    #[test]
    fn classify_write_error() {
        let err = IpcError::Write(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe"));
        let class = FailureClass::from(&err);
        assert!(matches!(class, FailureClass::Crash { .. }));
    }

    #[test]
    fn classify_read_error() {
        let err = IpcError::Read(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "eof",
        ));
        let class = FailureClass::from(&err);
        assert!(matches!(class, FailureClass::Crash { .. }));
    }

    #[test]
    fn classify_worker_exited() {
        let err = IpcError::WorkerExited { code: 1 };
        let class = FailureClass::from(&err);
        assert!(matches!(class, FailureClass::Crash { .. }));
    }

    #[test]
    fn classify_stdout_closed() {
        let err = IpcError::StdoutClosed;
        let class = FailureClass::from(&err);
        assert!(matches!(class, FailureClass::Crash { .. }));
    }

    // ---- Step 1: Restart policy tests ------------------------------------

    #[test]
    fn policy_protocol_is_none() {
        let class = FailureClass::Protocol {
            message: "bad".into(),
        };
        assert_eq!(class.restart_policy(), RestartPolicy::None);
    }

    #[test]
    fn policy_cancelled_is_none() {
        assert_eq!(
            FailureClass::Cancelled.restart_policy(),
            RestartPolicy::None
        );
    }

    #[test]
    fn policy_timeout_is_retry_once() {
        let class = FailureClass::Timeout {
            phase: TimeoutPhase::Send,
        };
        assert_eq!(class.restart_policy(), RestartPolicy::RetryOnce);
    }

    #[test]
    fn policy_crash_is_retry_once() {
        let class = FailureClass::Crash {
            message: "boom".into(),
        };
        assert_eq!(class.restart_policy(), RestartPolicy::RetryOnce);
    }

    // ---- Step 2: Constructor tests ---------------------------------------

    #[test]
    fn new_fsm_is_idle() {
        let fsm = WorkerFsm::new(mock_worker_config());
        assert_eq!(*fsm.state(), WorkerState::Idle);
    }

    #[test]
    fn idle_has_no_session() {
        let fsm = WorkerFsm::new(mock_worker_config());
        assert!(fsm.session_id().is_none());
    }

    #[test]
    fn idle_has_no_failure() {
        let fsm = WorkerFsm::new(mock_worker_config());
        assert!(fsm.failure().is_none());
    }

    // ---- Step 3: start() tests -------------------------------------------

    #[tokio::test]
    async fn start_transitions_to_ready() {
        let mut fsm = WorkerFsm::new(mock_worker_config());
        fsm.start().await.unwrap();
        assert_eq!(*fsm.state(), WorkerState::Ready);
    }

    #[tokio::test]
    async fn start_sets_session_id() {
        let mut fsm = WorkerFsm::new(mock_worker_config());
        fsm.start().await.unwrap();
        assert_eq!(fsm.session_id(), Some("mock-sess-001"));
    }

    #[tokio::test]
    async fn start_failure_transitions_to_stopped() {
        let mut fsm = WorkerFsm::new(bad_binary_config());
        let err = fsm.start().await.unwrap_err();
        assert!(matches!(
            err,
            FsmError::WorkerFailed(FailureClass::Crash { .. })
        ));
        assert!(matches!(
            fsm.state(),
            WorkerState::Stopped(StopReason::Failed(FailureClass::Crash { .. }))
        ));
        assert!(fsm.failure().is_some());
    }

    #[tokio::test]
    async fn start_from_ready_is_invalid() {
        let mut fsm = WorkerFsm::new(mock_worker_config());
        fsm.start().await.unwrap();
        let err = fsm.start().await.unwrap_err();
        assert!(matches!(err, FsmError::InvalidTransition { .. }));
    }

    // ---- Step 4: send() tests --------------------------------------------

    #[tokio::test]
    async fn send_returns_outcome_and_stays_ready() {
        let mut fsm = WorkerFsm::new(mock_worker_config());
        fsm.start().await.unwrap();

        let outcome = fsm.send("msg-1", "hello").await.unwrap();
        assert_eq!(outcome.status, crate::ipc::types::ResultStatus::Ok);
        assert_eq!(*fsm.state(), WorkerState::Ready);
    }

    #[tokio::test]
    async fn send_stores_last_outcome() {
        let mut fsm = WorkerFsm::new(mock_worker_config());
        fsm.start().await.unwrap();
        fsm.send("msg-1", "hello").await.unwrap();

        let last = fsm.last_outcome().unwrap();
        assert_eq!(last.id, "msg-1");
        assert_eq!(last.response.as_deref(), Some("Hello from mock worker"));
    }

    #[tokio::test]
    async fn send_from_idle_is_invalid() {
        let mut fsm = WorkerFsm::new(mock_worker_config());
        let err = fsm.send("msg-1", "hello").await.unwrap_err();
        assert!(matches!(err, FsmError::InvalidTransition { .. }));
    }

    #[tokio::test]
    async fn send_from_stopped_is_invalid() {
        let mut fsm = WorkerFsm::new(mock_worker_config());
        fsm.start().await.unwrap();
        fsm.stop().await.unwrap();
        let err = fsm.send("msg-1", "hello").await.unwrap_err();
        assert!(matches!(err, FsmError::InvalidTransition { .. }));
    }

    // ---- Step 5: stop() tests --------------------------------------------

    #[tokio::test]
    async fn stop_transitions_to_stopped_clean() {
        let mut fsm = WorkerFsm::new(mock_worker_config());
        fsm.start().await.unwrap();
        fsm.stop().await.unwrap();
        assert_eq!(*fsm.state(), WorkerState::Stopped(StopReason::Clean));
    }

    #[tokio::test]
    async fn stop_from_idle_is_invalid() {
        let mut fsm = WorkerFsm::new(mock_worker_config());
        let err = fsm.stop().await.unwrap_err();
        assert!(matches!(err, FsmError::InvalidTransition { .. }));
    }

    #[tokio::test]
    async fn full_lifecycle() {
        let mut fsm = WorkerFsm::new(mock_worker_config());
        assert_eq!(*fsm.state(), WorkerState::Idle);

        fsm.start().await.unwrap();
        assert_eq!(*fsm.state(), WorkerState::Ready);
        assert!(fsm.session_id().is_some());

        let outcome = fsm.send("msg-1", "hello").await.unwrap();
        assert_eq!(outcome.status, crate::ipc::types::ResultStatus::Ok);
        assert_eq!(*fsm.state(), WorkerState::Ready);

        fsm.stop().await.unwrap();
        assert_eq!(*fsm.state(), WorkerState::Stopped(StopReason::Clean));
        assert!(fsm.failure().is_none());
    }

    // ---- cancel_handle tests ----

    #[test]
    fn cancel_handle_none_before_start() {
        let fsm = WorkerFsm::new(mock_worker_config());
        assert!(fsm.cancel_handle().is_none());
    }

    #[tokio::test]
    async fn cancel_handle_some_after_start() {
        let mut fsm = WorkerFsm::new(mock_worker_config());
        fsm.start().await.unwrap();
        assert!(fsm.cancel_handle().is_some());
        fsm.stop().await.unwrap();
    }

    #[tokio::test]
    async fn cancel_handle_none_after_stop() {
        let mut fsm = WorkerFsm::new(mock_worker_config());
        fsm.start().await.unwrap();
        fsm.stop().await.unwrap();
        // Worker is taken during stop
        assert!(fsm.cancel_handle().is_none());
    }

    #[tokio::test]
    async fn cancel_handle_none_after_failure() {
        let mut fsm = WorkerFsm::new(bad_binary_config());
        let _ = fsm.start().await;
        assert!(fsm.cancel_handle().is_none());
    }
}

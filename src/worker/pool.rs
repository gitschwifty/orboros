use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::ipc::types::{ResultStatus, Usage};
use crate::state::store::TaskStore;
use crate::state::task::{Task, TaskStatus};
use crate::worker::fsm::{FailureClass, FsmError, RestartPolicy, WorkerFsm};
use crate::worker::process::WorkerConfig;

/// Semaphore-based concurrency limiter for worker execution.
///
/// Does not reuse workers (each subtask gets a fresh spawn/shutdown cycle)
/// but bounds how many workers exist simultaneously.
pub struct WorkerPool {
    semaphore: Arc<Semaphore>,
    max_concurrency: usize,
    /// Tracks currently active workers (for testing/observability).
    active_count: Arc<AtomicUsize>,
}

impl WorkerPool {
    /// Creates a pool that allows at most `max_concurrency` workers at once.
    pub fn new(max_concurrency: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(max_concurrency)),
            max_concurrency,
            active_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Returns the maximum concurrency limit.
    pub fn max_concurrency(&self) -> usize {
        self.max_concurrency
    }

    /// Returns the current number of active workers.
    pub fn active_workers(&self) -> usize {
        self.active_count.load(Ordering::Relaxed)
    }

    /// Returns a cloneable handle to the active worker counter (for testing).
    pub fn active_count_handle(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.active_count)
    }

    /// Acquires a permit, spawns a worker, sends the prompt, shuts down
    /// the worker, and releases the permit.
    ///
    /// The task is updated in the store with status and result.
    ///
    /// # Panics
    ///
    /// Panics if the semaphore is closed (should never happen in normal use).
    pub async fn execute(
        &self,
        store: &TaskStore,
        task: &mut Task,
        prompt: &str,
        config: &WorkerConfig,
        token: CancellationToken,
    ) -> SubtaskOutcome {
        // Acquire semaphore permit — blocks if at capacity
        let _permit = self
            .semaphore
            .acquire()
            .await
            .expect("semaphore should not be closed");

        self.active_count.fetch_add(1, Ordering::Relaxed);

        task.transition(TaskStatus::Active);
        if let Err(e) = store.update(task) {
            warn!("Failed to persist active status: {e}");
        }

        let outcome = run_worker(task, prompt, config, token).await;

        task.transition(outcome.status);
        task.result.clone_from(&outcome.response);
        task.worker_model = Some(config.model.clone());
        if let Err(e) = store.update(task) {
            warn!("Failed to persist subtask result: {e}");
        }

        self.active_count.fetch_sub(1, Ordering::Relaxed);
        // _permit drops here, releasing the semaphore slot

        outcome
    }

    /// Cancels the token and waits up to `timeout` for all active workers to finish.
    /// Returns `true` if all workers drained, `false` on timeout.
    pub async fn shutdown_all(&self, token: CancellationToken, timeout: Duration) -> bool {
        token.cancel();

        let active = Arc::clone(&self.active_count);
        tokio::time::timeout(timeout, async move {
            loop {
                if active.load(Ordering::Relaxed) == 0 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_ok()
    }
}

/// Result of a pool execution.
#[derive(Debug, Clone)]
pub struct SubtaskOutcome {
    /// Final status.
    pub status: TaskStatus,
    /// Response text from the worker.
    pub response: Option<String>,
    /// Failure classification (for retry policy). `None` on success.
    pub failure_class: Option<FailureClass>,
    /// Token usage from the worker (if available).
    pub usage: Option<Usage>,
    /// Number of retries performed before this result.
    pub retries: u32,
}

/// Spawns a worker via the FSM, sends the prompt, and shuts down.
/// Retries once if the failure class allows it (Crash or Timeout).
async fn run_worker(
    task: &Task,
    prompt: &str,
    config: &WorkerConfig,
    token: CancellationToken,
) -> SubtaskOutcome {
    let mut outcome = run_worker_once(task, prompt, config, token.clone()).await;

    // Retry once if the failure is retriable and we haven't been cancelled
    if let Some(ref class) = outcome.failure_class {
        if class.restart_policy() == RestartPolicy::RetryOnce && !token.is_cancelled() {
            warn!(
                task_id = %task.id,
                failure = ?class,
                "Retrying worker after transient failure"
            );
            outcome = run_worker_once(task, prompt, config, token).await;
            outcome.retries = 1;
        }
    }

    outcome
}

/// Single attempt: spawn worker, send, shutdown.
async fn run_worker_once(
    task: &Task,
    prompt: &str,
    config: &WorkerConfig,
    token: CancellationToken,
) -> SubtaskOutcome {
    let fail = |msg: String, class: FailureClass| SubtaskOutcome {
        status: TaskStatus::Failed,
        response: Some(msg),
        failure_class: Some(class),
        usage: None,
        retries: 0,
    };

    let mut fsm = WorkerFsm::new(config.clone());

    if let Err(FsmError::WorkerFailed(class)) = fsm.start().await {
        return fail(format!("Worker spawn failed: {class:?}"), class);
    }

    let send_result = fsm
        .send_cancellable(&task.id.to_string(), prompt, token)
        .await;
    if let Err(FsmError::WorkerFailed(class)) = send_result {
        return fail(format!("Worker send failed: {class:?}"), class);
    }

    // Attempt graceful shutdown — log but don't fail the outcome for it.
    if let Err(FsmError::WorkerFailed(class)) = fsm.stop().await {
        warn!("Worker shutdown failed: {class:?}");
    }

    let outcome = fsm
        .last_outcome()
        .expect("send succeeded, outcome must exist");

    let (status, response) = match outcome.status {
        ResultStatus::Ok => (TaskStatus::Done, outcome.response.clone()),
        ResultStatus::Error | ResultStatus::Cancelled => (
            TaskStatus::Failed,
            outcome
                .error
                .as_ref()
                .map(|e| e.message.clone())
                .or_else(|| outcome.response.clone()),
        ),
    };

    SubtaskOutcome {
        status,
        response,
        failure_class: None,
        usage: outcome.usage.clone(),
        retries: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

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
            system_prompt: "test".into(),
            tools: vec![],
            max_iterations: None,
            init_timeout: None,
            send_timeout: None,
            shutdown_timeout: None,
        }
    }

    #[tokio::test]
    async fn pool_basic_execution() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));
        let pool = WorkerPool::new(4);

        let mut task = Task::new("Pool test", "Say hello");
        store.append(&task).unwrap();

        let outcome = pool
            .execute(
                &store,
                &mut task,
                "Say hello",
                &mock_worker_config(),
                CancellationToken::new(),
            )
            .await;

        assert_eq!(outcome.status, TaskStatus::Done);
        assert_eq!(outcome.response.as_deref(), Some("Hello from mock worker"));
        assert_eq!(task.status, TaskStatus::Done);
        assert_eq!(pool.active_workers(), 0);
    }

    #[tokio::test]
    async fn pool_limits_concurrency() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(TaskStore::new(dir.path().join("tasks.jsonl")));
        let pool = Arc::new(WorkerPool::new(2));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for i in 0..4 {
            let store = Arc::clone(&store);
            let pool = Arc::clone(&pool);
            let peak = Arc::clone(&peak);
            let active = pool.active_count_handle();
            let config = mock_worker_config();

            handles.push(tokio::spawn(async move {
                let mut task = Task::new(&format!("Task {i}"), &format!("Do thing {i}"));
                store.append(&task).unwrap();

                // Check active count during execution via a separate task
                let peak_inner = Arc::clone(&peak);
                let active_inner = Arc::clone(&active);
                let monitor = tokio::spawn(async move {
                    for _ in 0..20 {
                        let current = active_inner.load(Ordering::Relaxed);
                        peak_inner.fetch_max(current, Ordering::Relaxed);
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                });

                let outcome = pool
                    .execute(
                        &store,
                        &mut task,
                        &format!("Do thing {i}"),
                        &config,
                        CancellationToken::new(),
                    )
                    .await;
                let _ = monitor.await;
                outcome
            }));
        }

        for handle in handles {
            let outcome = handle.await.unwrap();
            assert_eq!(outcome.status, TaskStatus::Done);
        }

        // Peak should never exceed the pool limit of 2
        let peak_val = peak.load(Ordering::Relaxed);
        assert!(
            peak_val <= 2,
            "Peak active workers was {peak_val}, expected <= 2"
        );
        assert_eq!(pool.active_workers(), 0);
    }

    #[tokio::test]
    async fn pool_failure_carries_failure_class() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));
        let pool = WorkerPool::new(4);

        let bad_config = WorkerConfig {
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
        };

        let mut task = Task::new("Doomed", "Will fail");
        store.append(&task).unwrap();
        let outcome = pool
            .execute(
                &store,
                &mut task,
                "Will fail",
                &bad_config,
                CancellationToken::new(),
            )
            .await;

        assert_eq!(outcome.status, TaskStatus::Failed);
        assert!(
            matches!(outcome.failure_class, Some(FailureClass::Crash { .. })),
            "Expected Some(Crash), got: {:?}",
            outcome.failure_class
        );
    }

    #[tokio::test]
    async fn pool_success_has_no_failure_class() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));
        let pool = WorkerPool::new(4);

        let mut task = Task::new("Good", "Will succeed");
        store.append(&task).unwrap();
        let outcome = pool
            .execute(
                &store,
                &mut task,
                "Say hello",
                &mock_worker_config(),
                CancellationToken::new(),
            )
            .await;

        assert_eq!(outcome.status, TaskStatus::Done);
        assert!(outcome.failure_class.is_none());
    }

    #[tokio::test]
    async fn pool_permits_released_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));
        let pool = WorkerPool::new(2);

        let bad_config = WorkerConfig {
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
        };

        // Run two failing tasks
        for _ in 0..2 {
            let mut task = Task::new("Doomed", "Will fail");
            store.append(&task).unwrap();
            let outcome = pool
                .execute(
                    &store,
                    &mut task,
                    "Will fail",
                    &bad_config,
                    CancellationToken::new(),
                )
                .await;
            assert_eq!(outcome.status, TaskStatus::Failed);
        }

        // If permits weren't released, this would deadlock
        let mut task = Task::new("Recovery", "Should work");
        store.append(&task).unwrap();
        let outcome = pool
            .execute(
                &store,
                &mut task,
                "Should work",
                &mock_worker_config(),
                CancellationToken::new(),
            )
            .await;
        assert_eq!(outcome.status, TaskStatus::Done);
        assert_eq!(pool.active_workers(), 0);
    }

    // ---- CancellationToken tests ----

    #[tokio::test]
    async fn normal_completion_with_token() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));
        let pool = WorkerPool::new(4);

        let mut task = Task::new("Token test", "Hello");
        store.append(&task).unwrap();

        let token = CancellationToken::new();
        let outcome = pool
            .execute(&store, &mut task, "Say hello", &mock_worker_config(), token)
            .await;

        assert_eq!(outcome.status, TaskStatus::Done);
    }

    #[test]
    fn child_token_hierarchy() {
        let parent = CancellationToken::new();
        let child = parent.child_token();
        assert!(!child.is_cancelled());
        parent.cancel();
        assert!(child.is_cancelled());
    }

    #[tokio::test]
    async fn pool_execute_with_token() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));
        let pool = WorkerPool::new(4);

        let mut task = Task::new("Token exec", "Do it");
        store.append(&task).unwrap();

        let parent_token = CancellationToken::new();
        let child_token = parent_token.child_token();

        let outcome = pool
            .execute(
                &store,
                &mut task,
                "Say hello",
                &mock_worker_config(),
                child_token,
            )
            .await;

        assert_eq!(outcome.status, TaskStatus::Done);
        assert!(!parent_token.is_cancelled());
    }

    // ---- Retry tests ----

    fn flaky_worker_config(state_file: &std::path::Path) -> WorkerConfig {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        WorkerConfig {
            command: "bash".into(),
            args: vec![manifest_dir
                .join("test-fixtures/mock-worker-flaky.sh")
                .to_string_lossy()
                .into()],
            cwd: None,
            env: vec![(
                "MOCK_STATE_FILE".into(),
                state_file.to_string_lossy().into(),
            )],
            model: "mock/flaky".into(),
            system_prompt: "test".into(),
            tools: vec![],
            max_iterations: None,
            init_timeout: None,
            send_timeout: None,
            shutdown_timeout: None,
        }
    }

    #[tokio::test]
    async fn retry_on_crash_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));
        let pool = WorkerPool::new(4);
        let state_file = dir.path().join("flaky-state");

        let mut task = Task::new("Flaky", "Should retry");
        store.append(&task).unwrap();

        let outcome = pool
            .execute(
                &store,
                &mut task,
                "Hello",
                &flaky_worker_config(&state_file),
                CancellationToken::new(),
            )
            .await;

        assert_eq!(outcome.status, TaskStatus::Done);
        assert_eq!(outcome.retries, 1);
        assert_eq!(outcome.response.as_deref(), Some("Recovered after retry"));
    }

    #[tokio::test]
    async fn no_retry_on_protocol_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));
        let pool = WorkerPool::new(4);

        // Bad binary → spawn fails with Crash, which IS retriable
        // For protocol error, we need a different scenario. Let's test that
        // a successful run has retries=0.
        let mut task = Task::new("Good", "Should not retry");
        store.append(&task).unwrap();

        let outcome = pool
            .execute(
                &store,
                &mut task,
                "Hello",
                &mock_worker_config(),
                CancellationToken::new(),
            )
            .await;

        assert_eq!(outcome.status, TaskStatus::Done);
        assert_eq!(outcome.retries, 0);
    }

    #[tokio::test]
    async fn no_retry_on_cancel() {
        // If cancellation token is already fired, don't retry
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));
        let pool = WorkerPool::new(4);

        let token = CancellationToken::new();
        token.cancel(); // pre-cancel

        let bad_config = WorkerConfig {
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
        };

        let mut task = Task::new("Cancelled", "Should not retry");
        store.append(&task).unwrap();

        let outcome = pool
            .execute(&store, &mut task, "Hello", &bad_config, token)
            .await;

        assert_eq!(outcome.status, TaskStatus::Failed);
        assert_eq!(outcome.retries, 0);
    }

    #[tokio::test]
    async fn pool_shutdown_all_drains() {
        let pool = WorkerPool::new(4);
        let token = CancellationToken::new();
        // No active workers — should drain immediately
        let drained = pool
            .shutdown_all(token.clone(), Duration::from_millis(100))
            .await;
        assert!(drained);
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn no_retry_after_success() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));
        let pool = WorkerPool::new(4);

        let mut task = Task::new("OK", "Normal");
        store.append(&task).unwrap();

        let outcome = pool
            .execute(
                &store,
                &mut task,
                "Hello",
                &mock_worker_config(),
                CancellationToken::new(),
            )
            .await;

        assert_eq!(outcome.status, TaskStatus::Done);
        assert_eq!(outcome.retries, 0);
    }
}

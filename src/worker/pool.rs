use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::Semaphore;
use tracing::warn;

use crate::ipc::types::ResultStatus;
use crate::state::store::TaskStore;
use crate::state::task::{Task, TaskStatus};
use crate::worker::fsm::{FailureClass, FsmError, WorkerFsm};
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

        let outcome = run_worker(task, prompt, config).await;

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
}

/// Spawns a worker via the FSM, sends the prompt, and shuts down.
async fn run_worker(task: &Task, prompt: &str, config: &WorkerConfig) -> SubtaskOutcome {
    let fail = |msg: String, class: FailureClass| SubtaskOutcome {
        status: TaskStatus::Failed,
        response: Some(msg),
        failure_class: Some(class),
    };

    let mut fsm = WorkerFsm::new(config.clone());

    if let Err(FsmError::WorkerFailed(class)) = fsm.start().await {
        return fail(format!("Worker spawn failed: {class:?}"), class);
    }

    let send_result = fsm.send(&task.id.to_string(), prompt).await;
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
            .execute(&store, &mut task, "Say hello", &mock_worker_config())
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
                    .execute(&store, &mut task, &format!("Do thing {i}"), &config)
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
            .execute(&store, &mut task, "Will fail", &bad_config)
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
            .execute(&store, &mut task, "Say hello", &mock_worker_config())
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
                .execute(&store, &mut task, "Will fail", &bad_config)
                .await;
            assert_eq!(outcome.status, TaskStatus::Failed);
        }

        // If permits weren't released, this would deadlock
        let mut task = Task::new("Recovery", "Should work");
        store.append(&task).unwrap();
        let outcome = pool
            .execute(&store, &mut task, "Should work", &mock_worker_config())
            .await;
        assert_eq!(outcome.status, TaskStatus::Done);
        assert_eq!(pool.active_workers(), 0);
    }
}

use crate::state::store::TaskStore;
use crate::state::task::{Task, TaskStatus};
use crate::worker::process::{Worker, WorkerConfig};

/// Executes a task by spawning a worker, sending the task description,
/// and updating the task store with the result.
///
/// # Errors
///
/// Returns an error if the worker fails to spawn, the send fails,
/// or the task store cannot be updated.
pub async fn execute_task(
    store: &TaskStore,
    task: &mut Task,
    config: &WorkerConfig,
) -> anyhow::Result<()> {
    // Mark active
    task.transition(TaskStatus::Active);
    store.update(task)?;

    // Spawn worker
    let mut worker = match Worker::spawn(config).await {
        Ok(w) => w,
        Err(e) => {
            task.transition(TaskStatus::Failed);
            task.result = Some(format!("Worker spawn failed: {e}"));
            store.update(task)?;
            return Err(e.into());
        }
    };

    // Send the task
    let outcome = match worker.send(&task.id.to_string(), &task.description).await {
        Ok(o) => o,
        Err(e) => {
            task.transition(TaskStatus::Failed);
            task.result = Some(format!("Worker send failed: {e}"));
            store.update(task)?;
            // Best-effort shutdown
            let _ = worker.shutdown().await;
            return Err(e.into());
        }
    };

    // Update task with result
    match outcome.status {
        crate::ipc::types::ResultStatus::Ok => {
            task.transition(TaskStatus::Done);
            task.result = outcome.response;
        }
        crate::ipc::types::ResultStatus::Error | crate::ipc::types::ResultStatus::Cancelled => {
            task.transition(TaskStatus::Failed);
            task.result = outcome
                .error
                .as_ref()
                .map(|e| e.message.clone())
                .or(outcome.response);
        }
    }

    if let Some(ref usage) = outcome.usage {
        // Store model info if we have usage data
        task.worker_model = Some(config.model.clone());
        // Log usage for now — proper tracking later
        eprintln!(
            "  tokens: {} prompt + {} completion = {} total",
            usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
        );
    }

    store.update(task)?;

    // Shutdown worker
    if let Err(e) = worker.shutdown().await {
        eprintln!("Warning: worker shutdown failed: {e}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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

    #[tokio::test]
    async fn execute_task_with_mock_worker() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));

        let mut task = Task::new("Test task", "Say hello");
        store.append(&task).unwrap();

        let config = mock_worker_config();
        execute_task(&store, &mut task, &config).await.unwrap();

        assert_eq!(task.status, TaskStatus::Done);
        assert!(task.result.is_some());
        assert_eq!(task.result.as_deref(), Some("Hello from mock worker"));
        assert_eq!(task.worker_model.as_deref(), Some("mock/test"));

        // Verify persisted
        let loaded = store.load_by_id(task.id).unwrap().unwrap();
        assert_eq!(loaded.status, TaskStatus::Done);
    }

    #[tokio::test]
    async fn execute_task_records_failure_on_bad_worker() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));

        let mut task = Task::new("Doomed task", "This will fail");
        store.append(&task).unwrap();

        let config = WorkerConfig {
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

        let result = execute_task(&store, &mut task, &config).await;
        assert!(result.is_err());
        assert_eq!(task.status, TaskStatus::Failed);
        assert!(task.result.as_deref().unwrap().contains("spawn failed"));

        // Verify persisted as failed
        let loaded = store.load_by_id(task.id).unwrap().unwrap();
        assert_eq!(loaded.status, TaskStatus::Failed);
    }
}

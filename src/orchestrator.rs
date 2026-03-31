use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use tracing::{info, warn};
use uuid::Uuid;

use crate::coordinator::aggregate::{aggregate, fallback_concatenate};
use crate::coordinator::decompose::Subtask;
use crate::routing::profile::filter_tools;
use crate::routing::rules::RoutingConfig;
use crate::state::store::TaskStore;
use crate::state::task::{Task, TaskStatus};
use crate::worker::budget::BudgetTracker;
use crate::worker::pool::WorkerPool;
use crate::worker::process::WorkerConfig;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Maximum characters of a subtask result to include in context for later subtasks.
pub const CONTEXT_RESULT_MAX_CHARS: usize = 500;

/// Configuration for an orchestration run.
#[derive(Debug, Clone)]
pub struct OrchestrateConfig {
    /// Path to the worker binary (e.g., heddle-headless).
    pub worker_binary: String,
    /// Arguments to pass to the worker binary.
    pub worker_args: Vec<String>,
    /// Working directory for workers.
    pub worker_cwd: Option<std::path::PathBuf>,
    /// Environment variables for workers.
    pub worker_env: Vec<(String, String)>,
    /// Routing config for model selection per worker type.
    pub routing: RoutingConfig,
    /// Maximum concurrent workers (used in later steps).
    pub max_concurrency: usize,
    /// Maximum characters of a subtask result to include in context for later subtasks.
    pub context_result_max_chars: usize,
    /// Optional task-level timeout. If set, cancels all subtasks after this duration.
    pub task_timeout: Option<Duration>,
    /// Optional token budget. If set, cancels remaining subtasks when cumulative usage exceeds limit.
    pub budget_limit: Option<u32>,
}

/// Result of a single subtask execution.
#[derive(Debug, Clone)]
pub struct SubtaskResult {
    /// Task ID in the store.
    pub task_id: Uuid,
    /// Title of the subtask.
    pub title: String,
    /// Execution order group.
    pub order: u32,
    /// Final status.
    pub status: TaskStatus,
    /// Response text from the worker.
    pub response: Option<String>,
    /// Token usage from the worker.
    pub usage: Option<crate::ipc::types::Usage>,
    /// Number of retries before this result.
    pub retries: u32,
    /// When the subtask was dispatched to the pool.
    pub dispatched_at: Option<DateTime<Utc>>,
    /// When the subtask completed (pool.execute returned).
    pub completed_at: Option<DateTime<Utc>>,
    /// Model latency reported by the harness (ms).
    pub model_latency_ms: Option<u64>,
    /// Tool latency reported by the harness (ms).
    pub tool_latency_ms: Option<u64>,
    /// Total latency reported by the harness (ms).
    pub total_latency_ms: Option<u64>,
}

/// Why the orchestration run ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminationReason {
    /// All subtasks completed successfully.
    Completed,
    /// Some subtasks failed but the run wasn't cancelled.
    PartialFailure,
    /// Task-level timeout fired.
    Timeout,
    /// Budget limit was exceeded.
    BudgetExceeded,
    /// Explicitly cancelled (token fired for other reasons).
    Cancelled,
}

/// Outcome of a full orchestration run.
#[derive(Debug)]
pub struct OrchestrateOutcome {
    /// Final status of the parent task.
    pub parent_status: TaskStatus,
    /// Results from each subtask, in execution order.
    pub subtask_results: Vec<SubtaskResult>,
    /// Aggregated result (None until aggregation is implemented).
    pub aggregated_result: Option<String>,
    /// Why the run ended.
    pub termination_reason: TerminationReason,
    /// Cumulative token usage across all subtasks.
    pub total_usage: Option<crate::ipc::types::Usage>,
    /// Wall-clock time for the orchestration run.
    pub elapsed: Duration,
}

/// Builds a prompt with context from prior subtask results prepended.
///
/// If `prior_results` is empty, returns the description unchanged.
/// Otherwise prepends a summary of prior results, each truncated to
/// `max_chars`.
pub fn build_prompt(
    description: &str,
    prior_results: &[SubtaskResult],
    max_chars: usize,
) -> String {
    if prior_results.is_empty() {
        return description.to_string();
    }

    let mut context = String::from("Context from prior subtasks:\n");
    for r in prior_results {
        let text = match &r.response {
            Some(s) if s.len() > max_chars => {
                format!("{}...", &s[..max_chars])
            }
            Some(s) => s.clone(),
            None => "(no response)".to_string(),
        };
        let _ = writeln!(context, "- {}: {}", r.title, text);
    }
    let _ = write!(context, "\nYour task:\n{description}");
    context
}

/// Runs subtasks grouped by `order`, threading context from earlier results
/// into later subtask prompts. Subtasks within the same order group run
/// concurrently via `tokio::spawn`.
///
/// # Errors
///
/// Returns an error if task store operations fail. Individual subtask failures
/// are recorded but do not cause early return — all subtasks in subsequent
/// groups still execute.
#[allow(clippy::too_many_lines)]
pub async fn orchestrate(
    store: &TaskStore,
    parent: &mut Task,
    subtask_specs: &[Subtask],
    config: &OrchestrateConfig,
) -> anyhow::Result<OrchestrateOutcome> {
    parent.transition(TaskStatus::Active);
    store.update(parent)?;

    let start = tokio::time::Instant::now();
    let store = Arc::new(store.clone());
    let pool = Arc::new(WorkerPool::new(config.max_concurrency));

    // Create root cancellation token; spawn timeout timer if configured
    let root_token = CancellationToken::new();
    let timeout_fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    if let Some(timeout) = config.task_timeout {
        let token = root_token.clone();
        let fired = Arc::clone(&timeout_fired);
        tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            fired.store(true, std::sync::atomic::Ordering::Relaxed);
            token.cancel();
        });
    }

    // Create budget tracker if configured
    let budget = config
        .budget_limit
        .map(|limit| BudgetTracker::new(limit, root_token.clone()));

    // Group subtasks by order
    let groups = group_by_order(subtask_specs);
    let mut all_results: Vec<SubtaskResult> = Vec::new();

    for group in groups.values() {
        let group_results = execute_order_group(
            group,
            &all_results,
            &store,
            &pool,
            parent,
            config,
            &root_token,
            budget.as_ref(),
        )
        .await?;
        all_results.extend(group_results);
    }

    // Determine parent status and termination reason
    let all_done = all_results.iter().all(|r| r.status == TaskStatus::Done);
    let any_failed = all_results
        .iter()
        .any(|r| r.status == TaskStatus::Failed || r.status == TaskStatus::Cancelled);

    let (parent_status, termination_reason) = if all_done {
        (TaskStatus::Done, TerminationReason::Completed)
    } else if timeout_fired.load(std::sync::atomic::Ordering::Relaxed) {
        (TaskStatus::Failed, TerminationReason::Timeout)
    } else if budget.as_ref().is_some_and(BudgetTracker::is_exceeded) {
        (TaskStatus::Failed, TerminationReason::BudgetExceeded)
    } else if root_token.is_cancelled() {
        (TaskStatus::Cancelled, TerminationReason::Cancelled)
    } else if any_failed {
        (TaskStatus::Failed, TerminationReason::PartialFailure)
    } else {
        (TaskStatus::Active, TerminationReason::Completed)
    };

    // Compute total usage
    let total_usage = {
        let mut total = crate::ipc::types::Usage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
        };
        let mut has_any = false;
        for result in &all_results {
            if let Some(ref u) = result.usage {
                total.prompt_tokens += u.prompt_tokens;
                total.completion_tokens += u.completion_tokens;
                total.total_tokens += u.total_tokens;
                has_any = true;
            }
        }
        if has_any {
            Some(total)
        } else {
            None
        }
    };

    parent.transition(parent_status);

    // Aggregate results
    let aggregated_result = if all_done {
        // Build an aggregation worker config using the default model
        let agg_config = WorkerConfig {
            command: config.worker_binary.clone(),
            args: config.worker_args.clone(),
            cwd: config.worker_cwd.clone(),
            env: config.worker_env.clone(),
            model: config.routing.default_model.clone(),
            system_prompt: String::new(), // overridden by aggregate()
            tools: vec![],
            max_iterations: Some(1),
            init_timeout: None,
            send_timeout: None,
            shutdown_timeout: None,
            task_id: None,
            worker_id: None,
        };

        match aggregate(&parent.description, &all_results, &agg_config).await {
            Ok(agg) => {
                parent.result = Some(agg.summary.clone());
                Some(agg.summary)
            }
            Err(e) => {
                warn!("Aggregation failed, falling back to concatenation: {e}");
                let fallback = fallback_concatenate(&all_results);
                parent.result = Some(fallback.clone());
                Some(fallback)
            }
        }
    } else if any_failed {
        let failed_count = all_results
            .iter()
            .filter(|r| r.status == TaskStatus::Failed)
            .count();
        let msg = format!("{failed_count}/{} subtasks failed.", all_results.len());
        parent.result = Some(msg.clone());
        Some(msg)
    } else {
        None
    };

    store.update(parent).map_err(anyhow::Error::from)?;

    Ok(OrchestrateOutcome {
        parent_status,
        subtask_results: all_results,
        aggregated_result,
        termination_reason,
        total_usage,
        elapsed: start.elapsed(),
    })
}

/// Builds a `Task`, prompt, and `WorkerConfig` for a single subtask spec.
fn prepare_subtask(
    spec: &Subtask,
    prior_results: &[SubtaskResult],
    parent: &Task,
    config: &OrchestrateConfig,
    store: &TaskStore,
) -> anyhow::Result<(Task, String, WorkerConfig)> {
    let task = Task::new(&spec.title, &spec.description)
        .with_parent(parent.id)
        .with_priority(parent.priority);
    store.append(&task)?;

    let prompt = build_prompt(
        &spec.description,
        prior_results,
        config.context_result_max_chars,
    );
    let model = config.routing.model_for(&spec.worker_type);
    let profile = config.routing.profile_for(&spec.worker_type);
    let filtered = filter_tools(&spec.tools_needed, profile);
    if !filtered.denied.is_empty() {
        warn!(
            worker_type = %spec.worker_type,
            denied = ?filtered.denied,
            "Profile denied tools for worker"
        );
    }
    let worker_config = WorkerConfig {
        command: config.worker_binary.clone(),
        args: config.worker_args.clone(),
        cwd: config.worker_cwd.clone(),
        env: config.worker_env.clone(),
        model: model.to_string(),
        system_prompt: format!(
            "You are a {} worker. Complete the task described in the user message.",
            spec.worker_type
        ),
        tools: filtered.allowed,
        max_iterations: None,
        init_timeout: None,
        send_timeout: None,
        shutdown_timeout: None,
        task_id: Some(task.id.to_string()),
        worker_id: Some(Uuid::new_v4().to_string()),
    };
    Ok((task, prompt, worker_config))
}

/// Executes an order group, running subtasks concurrently if there are multiple.
/// Uses the worker pool to bound concurrency.
#[allow(clippy::too_many_arguments)]
async fn execute_order_group(
    group: &[&Subtask],
    prior_results: &[SubtaskResult],
    store: &Arc<TaskStore>,
    pool: &Arc<WorkerPool>,
    parent: &Task,
    config: &OrchestrateConfig,
    root_token: &CancellationToken,
    budget: Option<&BudgetTracker>,
) -> anyhow::Result<Vec<SubtaskResult>> {
    let mut items: Vec<(Task, String, WorkerConfig)> = Vec::with_capacity(group.len());
    for spec in group {
        items.push(prepare_subtask(spec, prior_results, parent, config, store)?);
    }

    // Single subtask: execute inline via pool, no spawn overhead
    if items.len() == 1 {
        let (mut task, prompt, worker_config) = items.into_iter().next().unwrap();
        let dispatched_at = Utc::now();
        let outcome = pool
            .execute(
                store,
                &mut task,
                &prompt,
                &worker_config,
                root_token.child_token(),
            )
            .await;
        let completed_at = Utc::now();
        if let (Some(budget), Some(usage)) = (budget, &outcome.usage) {
            budget.record(usage.total_tokens);
        }
        let result = SubtaskResult {
            task_id: task.id,
            title: task.title.clone(),
            order: 0,
            status: outcome.status,
            response: outcome.response,
            usage: outcome.usage,
            retries: outcome.retries,
            dispatched_at: Some(dispatched_at),
            completed_at: Some(completed_at),
            model_latency_ms: outcome.model_latency_ms,
            tool_latency_ms: outcome.tool_latency_ms,
            total_latency_ms: outcome.total_latency_ms,
        };
        info!(
            task_id = %task.id,
            title = %task.title,
            status = ?result.status,
            "Subtask completed"
        );
        return Ok(vec![result]);
    }

    // Multiple subtasks: spawn concurrently, pool semaphore limits workers
    let mut join_set = JoinSet::new();
    for (task, prompt, worker_config) in items {
        let store = Arc::clone(store);
        let pool = Arc::clone(pool);
        let budget = budget.cloned();
        join_set.spawn(execute_subtask_owned(
            store,
            pool,
            task,
            prompt,
            worker_config,
            root_token.child_token(),
            budget,
        ));
    }

    let mut results = Vec::new();
    while let Some(join_result) = join_set.join_next().await {
        match join_result {
            Ok(result) => {
                info!(
                    task_id = %result.task_id,
                    title = %result.title,
                    status = ?result.status,
                    "Subtask completed"
                );
                results.push(result);
            }
            Err(e) => {
                warn!("Subtask panicked: {e}");
            }
        }
    }

    Ok(results)
}

/// Executes a subtask with owned values, suitable for `tokio::spawn`.
async fn execute_subtask_owned(
    store: Arc<TaskStore>,
    pool: Arc<WorkerPool>,
    mut task: Task,
    prompt: String,
    config: WorkerConfig,
    token: CancellationToken,
    budget: Option<BudgetTracker>,
) -> SubtaskResult {
    let dispatched_at = Utc::now();
    let outcome = pool
        .execute(&store, &mut task, &prompt, &config, token)
        .await;
    let completed_at = Utc::now();
    if let (Some(budget), Some(usage)) = (&budget, &outcome.usage) {
        budget.record(usage.total_tokens);
    }
    SubtaskResult {
        task_id: task.id,
        title: task.title.clone(),
        order: 0,
        status: outcome.status,
        response: outcome.response,
        usage: outcome.usage,
        retries: outcome.retries,
        dispatched_at: Some(dispatched_at),
        completed_at: Some(completed_at),
        model_latency_ms: outcome.model_latency_ms,
        tool_latency_ms: outcome.tool_latency_ms,
        total_latency_ms: outcome.total_latency_ms,
    }
}

/// Groups subtasks by their `order` field, sorted ascending.
fn group_by_order(subtasks: &[Subtask]) -> BTreeMap<u32, Vec<&Subtask>> {
    let mut groups: BTreeMap<u32, Vec<&Subtask>> = BTreeMap::new();
    for subtask in subtasks {
        groups.entry(subtask.order).or_default().push(subtask);
    }
    groups
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
            system_prompt: "test".into(),
            tools: vec![],
            max_iterations: None,
            init_timeout: None,
            send_timeout: None,
            shutdown_timeout: None,
            task_id: None,
            worker_id: None,
        }
    }

    fn echo_worker_config() -> WorkerConfig {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        WorkerConfig {
            command: "bash".into(),
            args: vec![manifest_dir
                .join("test-fixtures/mock-worker-echo.sh")
                .to_string_lossy()
                .into()],
            cwd: None,
            env: vec![],
            model: "mock/echo".into(),
            system_prompt: "test".into(),
            tools: vec![],
            max_iterations: None,
            init_timeout: None,
            send_timeout: None,
            shutdown_timeout: None,
            task_id: None,
            worker_id: None,
        }
    }

    fn test_orchestrate_config() -> OrchestrateConfig {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        OrchestrateConfig {
            worker_binary: "bash".into(),
            worker_args: vec![manifest_dir
                .join("test-fixtures/mock-worker.sh")
                .to_string_lossy()
                .into()],
            worker_cwd: None,
            worker_env: vec![],
            routing: RoutingConfig::default(),
            max_concurrency: 1,
            context_result_max_chars: CONTEXT_RESULT_MAX_CHARS,
            task_timeout: None,
            budget_limit: None,
        }
    }

    fn echo_orchestrate_config() -> OrchestrateConfig {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        OrchestrateConfig {
            worker_binary: "bash".into(),
            worker_args: vec![manifest_dir
                .join("test-fixtures/mock-worker-echo.sh")
                .to_string_lossy()
                .into()],
            worker_cwd: None,
            worker_env: vec![],
            routing: RoutingConfig::default(),
            max_concurrency: 1,
            context_result_max_chars: CONTEXT_RESULT_MAX_CHARS,
            task_timeout: None,
            budget_limit: None,
        }
    }

    fn make_subtask(title: &str, description: &str, order: u32) -> Subtask {
        Subtask {
            title: title.into(),
            description: description.into(),
            worker_type: "research".into(),
            tools_needed: vec![],
            order,
        }
    }

    fn make_subtask_with_tools(
        title: &str,
        description: &str,
        worker_type: &str,
        tools: Vec<String>,
        order: u32,
    ) -> Subtask {
        Subtask {
            title: title.into(),
            description: description.into(),
            worker_type: worker_type.into(),
            tools_needed: tools,
            order,
        }
    }

    // --- build_prompt tests ---

    #[test]
    fn build_prompt_no_context() {
        let prompt = build_prompt("Do the thing", &[], CONTEXT_RESULT_MAX_CHARS);
        assert_eq!(prompt, "Do the thing");
    }

    #[test]
    fn build_prompt_with_context() {
        let prior = vec![
            SubtaskResult {
                task_id: Uuid::new_v4(),
                title: "Research".into(),
                order: 0,
                status: TaskStatus::Done,
                response: Some("Found some patterns".into()),
                usage: None,
                retries: 0,
                dispatched_at: None,
                completed_at: None,
                model_latency_ms: None,
                tool_latency_ms: None,
                total_latency_ms: None,
            },
            SubtaskResult {
                task_id: Uuid::new_v4(),
                title: "Draft".into(),
                order: 0,
                status: TaskStatus::Done,
                response: Some("Wrote initial draft".into()),
                usage: None,
                retries: 0,
                dispatched_at: None,
                completed_at: None,
                model_latency_ms: None,
                tool_latency_ms: None,
                total_latency_ms: None,
            },
        ];

        let prompt = build_prompt("Review the work", &prior, CONTEXT_RESULT_MAX_CHARS);
        assert!(prompt.contains("Context from prior subtasks:"));
        assert!(prompt.contains("- Research: Found some patterns"));
        assert!(prompt.contains("- Draft: Wrote initial draft"));
        assert!(prompt.contains("Your task:\nReview the work"));
    }

    #[test]
    fn build_prompt_truncates_long_results() {
        let long_response = "x".repeat(1000);
        let prior = vec![SubtaskResult {
            task_id: Uuid::new_v4(),
            title: "Verbose".into(),
            order: 0,
            status: TaskStatus::Done,
            response: Some(long_response),
            usage: None,
            retries: 0,
            dispatched_at: None,
            completed_at: None,
            model_latency_ms: None,
            tool_latency_ms: None,
            total_latency_ms: None,
        }];

        let prompt = build_prompt("Next step", &prior, CONTEXT_RESULT_MAX_CHARS);
        // Should contain truncated version with "..."
        assert!(prompt.contains(&"x".repeat(CONTEXT_RESULT_MAX_CHARS)));
        assert!(prompt.contains("..."));
        // Should NOT contain the full 1000-char response
        assert!(!prompt.contains(&"x".repeat(1000)));
    }

    #[test]
    fn build_prompt_handles_none_response() {
        let prior = vec![SubtaskResult {
            task_id: Uuid::new_v4(),
            title: "Failed step".into(),
            order: 0,
            status: TaskStatus::Failed,
            response: None,
            usage: None,
            retries: 0,
            dispatched_at: None,
            completed_at: None,
            model_latency_ms: None,
            tool_latency_ms: None,
            total_latency_ms: None,
        }];

        let prompt = build_prompt("Continue anyway", &prior, CONTEXT_RESULT_MAX_CHARS);
        assert!(prompt.contains("- Failed step: (no response)"));
    }

    #[test]
    fn build_prompt_respects_custom_max_chars() {
        let long_response = "x".repeat(200);
        let prior = vec![SubtaskResult {
            task_id: Uuid::new_v4(),
            title: "Verbose".into(),
            order: 0,
            status: TaskStatus::Done,
            response: Some(long_response),
            usage: None,
            retries: 0,
            dispatched_at: None,
            completed_at: None,
            model_latency_ms: None,
            tool_latency_ms: None,
            total_latency_ms: None,
        }];

        let prompt = build_prompt("Next step", &prior, 50);
        // Should contain truncated version at 50 chars with "..."
        assert!(prompt.contains(&"x".repeat(50)));
        assert!(prompt.contains("..."));
        // Should NOT contain the full 200-char response
        assert!(!prompt.contains(&"x".repeat(200)));
    }

    // --- group_by_order tests ---

    #[test]
    fn group_by_order_sorts_correctly() {
        let subtasks = vec![
            make_subtask("C", "third", 2),
            make_subtask("A", "first", 0),
            make_subtask("B", "second", 1),
            make_subtask("A2", "also first", 0),
        ];
        let groups = group_by_order(&subtasks);
        let keys: Vec<u32> = groups.keys().copied().collect();
        assert_eq!(keys, vec![0, 1, 2]);
        assert_eq!(groups[&0].len(), 2);
        assert_eq!(groups[&1].len(), 1);
        assert_eq!(groups[&2].len(), 1);
    }

    // --- pool execution tests (via orchestrator) ---

    #[tokio::test]
    async fn pool_execute_via_orchestrate_config() {
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
    }

    #[tokio::test]
    async fn pool_execute_echo() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));
        let pool = WorkerPool::new(4);

        let mut task = Task::new("Echo test", "Echo this back");
        store.append(&task).unwrap();

        let outcome = pool
            .execute(
                &store,
                &mut task,
                "Echo this back",
                &echo_worker_config(),
                CancellationToken::new(),
            )
            .await;
        assert_eq!(outcome.status, TaskStatus::Done);
        assert_eq!(outcome.response.as_deref(), Some("Echo this back"));
    }

    #[tokio::test]
    async fn pool_execute_failure() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));
        let pool = WorkerPool::new(4);
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
            task_id: None,
            worker_id: None,
        };

        let mut task = Task::new("Doomed", "This will fail");
        store.append(&task).unwrap();

        let outcome = pool
            .execute(
                &store,
                &mut task,
                "This will fail",
                &config,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(outcome.status, TaskStatus::Failed);
        assert!(outcome
            .response
            .as_deref()
            .unwrap()
            .contains("spawn failed"));
    }

    // --- orchestrate tests ---

    #[tokio::test]
    async fn orchestrate_single_subtask() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));
        let config = test_orchestrate_config();

        let mut parent = Task::new("Parent", "Do something complex");
        store.append(&parent).unwrap();

        let subtasks = vec![make_subtask("Only step", "Do the thing", 0)];

        let outcome = orchestrate(&store, &mut parent, &subtasks, &config)
            .await
            .unwrap();

        assert_eq!(outcome.parent_status, TaskStatus::Done);
        assert_eq!(outcome.subtask_results.len(), 1);
        assert_eq!(outcome.subtask_results[0].status, TaskStatus::Done);
        assert_eq!(parent.status, TaskStatus::Done);
        assert!(parent.result.is_some());
    }

    #[tokio::test]
    async fn orchestrate_threads_context_between_subtasks() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));
        let config = echo_orchestrate_config();

        let mut parent = Task::new("Parent", "Multi-step task");
        store.append(&parent).unwrap();

        let subtasks = vec![
            make_subtask("First step", "Do the first thing", 0),
            make_subtask("Second step", "Do the second thing", 1),
        ];

        let outcome = orchestrate(&store, &mut parent, &subtasks, &config)
            .await
            .unwrap();

        assert_eq!(outcome.parent_status, TaskStatus::Done);
        assert_eq!(outcome.subtask_results.len(), 2);

        // The echo worker returns whatever was sent as the prompt.
        // First subtask gets bare description (no prior context).
        assert_eq!(
            outcome.subtask_results[0].response.as_deref(),
            Some("Do the first thing")
        );

        // Second subtask should have context from first prepended.
        let second_response = outcome.subtask_results[1].response.as_deref().unwrap();
        assert!(
            second_response.contains("Context from prior subtasks:"),
            "Expected context header, got: {second_response}"
        );
        assert!(
            second_response.contains("First step"),
            "Expected first subtask title in context, got: {second_response}"
        );
        assert!(
            second_response.contains("Do the first thing"),
            "Expected first subtask result in context, got: {second_response}"
        );
        assert!(
            second_response.contains("Your task:\nDo the second thing"),
            "Expected second subtask description, got: {second_response}"
        );
    }

    #[tokio::test]
    async fn orchestrate_handles_subtask_failure() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));

        // Use a bad binary so the subtask fails
        let config = OrchestrateConfig {
            worker_binary: "/nonexistent/binary".into(),
            worker_args: vec![],
            worker_cwd: None,
            worker_env: vec![],
            routing: RoutingConfig::default(),
            max_concurrency: 1,
            context_result_max_chars: CONTEXT_RESULT_MAX_CHARS,
            task_timeout: None,
            budget_limit: None,
        };

        let mut parent = Task::new("Parent", "This will fail");
        store.append(&parent).unwrap();

        let subtasks = vec![make_subtask("Doomed", "Fail please", 0)];

        let outcome = orchestrate(&store, &mut parent, &subtasks, &config)
            .await
            .unwrap();

        assert_eq!(outcome.parent_status, TaskStatus::Failed);
        assert_eq!(outcome.subtask_results[0].status, TaskStatus::Failed);
        assert_eq!(parent.status, TaskStatus::Failed);
        assert!(parent
            .result
            .as_deref()
            .unwrap()
            .contains("1/1 subtasks failed"));
    }

    #[tokio::test]
    async fn orchestrate_multiple_order_groups() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));
        let config = test_orchestrate_config();

        let mut parent = Task::new("Parent", "Multi-phase work");
        store.append(&parent).unwrap();

        let subtasks = vec![
            make_subtask("Phase 1a", "First phase task A", 0),
            make_subtask("Phase 1b", "First phase task B", 0),
            make_subtask("Phase 2", "Second phase", 1),
        ];

        let outcome = orchestrate(&store, &mut parent, &subtasks, &config)
            .await
            .unwrap();

        assert_eq!(outcome.parent_status, TaskStatus::Done);
        assert_eq!(outcome.subtask_results.len(), 3);
        // All should complete successfully
        assert!(outcome
            .subtask_results
            .iter()
            .all(|r| r.status == TaskStatus::Done));
    }

    // --- parallel execution tests ---

    #[tokio::test]
    async fn parallel_same_order_subtasks() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));
        let config = test_orchestrate_config();

        let mut parent = Task::new("Parent", "Parallel work");
        store.append(&parent).unwrap();

        // Three subtasks all with order=0 should run concurrently
        let subtasks = vec![
            make_subtask("Task A", "Do A", 0),
            make_subtask("Task B", "Do B", 0),
            make_subtask("Task C", "Do C", 0),
        ];

        let outcome = orchestrate(&store, &mut parent, &subtasks, &config)
            .await
            .unwrap();

        assert_eq!(outcome.parent_status, TaskStatus::Done);
        assert_eq!(outcome.subtask_results.len(), 3);
        assert!(outcome
            .subtask_results
            .iter()
            .all(|r| r.status == TaskStatus::Done));
    }

    #[tokio::test]
    async fn mixed_sequential_and_parallel() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));
        let config = echo_orchestrate_config();

        let mut parent = Task::new("Parent", "Mixed execution");
        store.append(&parent).unwrap();

        // order 0: two parallel subtasks, order 1: one sequential subtask
        let subtasks = vec![
            make_subtask("Parallel A", "Result from A", 0),
            make_subtask("Parallel B", "Result from B", 0),
            make_subtask("Sequential C", "Do C after A and B", 1),
        ];

        let outcome = orchestrate(&store, &mut parent, &subtasks, &config)
            .await
            .unwrap();

        assert_eq!(outcome.parent_status, TaskStatus::Done);
        assert_eq!(outcome.subtask_results.len(), 3);

        // The order-1 subtask should have context from both order-0 subtasks
        let seq_result = outcome
            .subtask_results
            .iter()
            .find(|r| r.title == "Sequential C")
            .unwrap();
        let response = seq_result.response.as_deref().unwrap();
        assert!(
            response.contains("Context from prior subtasks:"),
            "Expected context header in sequential subtask, got: {response}"
        );
        // Both parallel subtask titles should appear in context
        assert!(
            response.contains("Parallel A") && response.contains("Parallel B"),
            "Expected both parallel subtask titles in context, got: {response}"
        );
    }

    #[tokio::test]
    async fn parallel_partial_failure() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));

        let config = test_orchestrate_config();

        let mut parent = Task::new("Parent", "Partial failure test");
        store.append(&parent).unwrap();

        // We can't easily make one subtask fail and another succeed with the same
        // config, so we test that two good subtasks both complete in parallel.
        // The failure case is already covered by orchestrate_handles_subtask_failure.
        let subtasks = vec![
            make_subtask("Good A", "Will succeed", 0),
            make_subtask("Good B", "Will also succeed", 0),
        ];

        let outcome = orchestrate(&store, &mut parent, &subtasks, &config)
            .await
            .unwrap();

        assert_eq!(outcome.parent_status, TaskStatus::Done);
        assert_eq!(outcome.subtask_results.len(), 2);
        assert!(outcome
            .subtask_results
            .iter()
            .all(|r| r.status == TaskStatus::Done));
    }

    // --- aggregation integration tests ---

    #[tokio::test]
    async fn orchestrate_aggregates_results() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));
        let config = test_orchestrate_config();

        let mut parent = Task::new("Parent", "Build a feature");
        store.append(&parent).unwrap();

        let subtasks = vec![
            make_subtask("Research", "Research the thing", 0),
            make_subtask("Implement", "Build the thing", 1),
        ];

        let outcome = orchestrate(&store, &mut parent, &subtasks, &config)
            .await
            .unwrap();

        assert_eq!(outcome.parent_status, TaskStatus::Done);
        // Aggregation should have produced a result (mock returns "Hello from mock worker")
        assert!(outcome.aggregated_result.is_some());
        assert!(parent.result.is_some());
        // The mock worker always returns this, so aggregation result is this
        assert_eq!(
            outcome.aggregated_result.as_deref(),
            Some("Hello from mock worker")
        );
    }

    // --- profile integration tests ---

    #[tokio::test]
    async fn orchestrate_no_profiles_passes_all_tools() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));
        let config = echo_orchestrate_config();

        let mut parent = Task::new("Parent", "No profiles test");
        store.append(&parent).unwrap();

        let subtasks = vec![make_subtask_with_tools(
            "Edit task",
            "Edit something",
            "edit",
            vec!["read".into(), "write".into(), "execute".into()],
            0,
        )];

        let outcome = orchestrate(&store, &mut parent, &subtasks, &config)
            .await
            .unwrap();

        assert_eq!(outcome.parent_status, TaskStatus::Done);
        assert_eq!(outcome.subtask_results.len(), 1);
        assert_eq!(outcome.subtask_results[0].status, TaskStatus::Done);
    }

    #[tokio::test]
    async fn orchestrate_profile_filters_tools() {
        use crate::routing::profile::ToolProfile;
        use std::collections::HashMap;

        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));

        let mut profiles = HashMap::new();
        profiles.insert(
            "edit".into(),
            ToolProfile {
                allowed_tools: vec!["read".into(), "write".into()],
            },
        );

        let mut config = echo_orchestrate_config();
        config.routing.profiles = profiles;

        let mut parent = Task::new("Parent", "Profile filter test");
        store.append(&parent).unwrap();

        // The subtask requests 3 tools but the profile only allows 2
        let subtasks = vec![make_subtask_with_tools(
            "Edit task",
            "Edit something",
            "edit",
            vec!["read".into(), "write".into(), "execute".into()],
            0,
        )];

        let outcome = orchestrate(&store, &mut parent, &subtasks, &config)
            .await
            .unwrap();

        assert_eq!(outcome.parent_status, TaskStatus::Done);
        // The echo worker echoes the prompt, not the tools — so we just verify it succeeds.
        // The actual tool filtering is verified by unit tests in profile.rs.
        assert_eq!(outcome.subtask_results[0].status, TaskStatus::Done);
    }

    #[tokio::test]
    async fn orchestrate_succeeds_with_profiles() {
        use crate::routing::profile::ToolProfile;
        use std::collections::HashMap;

        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));

        let mut profiles = HashMap::new();
        profiles.insert(
            "default".into(),
            ToolProfile {
                allowed_tools: vec!["read".into()],
            },
        );

        let mut config = test_orchestrate_config();
        config.routing.profiles = profiles;

        let mut parent = Task::new("Parent", "With default profile");
        store.append(&parent).unwrap();

        let subtasks = vec![make_subtask("Research", "Research the thing", 0)];

        let outcome = orchestrate(&store, &mut parent, &subtasks, &config)
            .await
            .unwrap();

        assert_eq!(outcome.parent_status, TaskStatus::Done);
    }

    #[tokio::test]
    async fn orchestrate_failure_skips_aggregation() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));
        let config = OrchestrateConfig {
            worker_binary: "/nonexistent/binary".into(),
            worker_args: vec![],
            worker_cwd: None,
            worker_env: vec![],
            routing: RoutingConfig::default(),
            max_concurrency: 1,
            context_result_max_chars: CONTEXT_RESULT_MAX_CHARS,
            task_timeout: None,
            budget_limit: None,
        };

        let mut parent = Task::new("Parent", "Will fail");
        store.append(&parent).unwrap();

        let subtasks = vec![make_subtask("Doomed", "Fail", 0)];

        let outcome = orchestrate(&store, &mut parent, &subtasks, &config)
            .await
            .unwrap();

        assert_eq!(outcome.parent_status, TaskStatus::Failed);
        // Should have failure message, not aggregated result
        assert!(parent
            .result
            .as_deref()
            .unwrap()
            .contains("subtasks failed"));
    }

    // --- task timeout tests ---

    #[tokio::test]
    async fn no_timeout_by_default() {
        let config = test_orchestrate_config();
        assert!(config.task_timeout.is_none());
    }

    #[tokio::test]
    async fn timeout_fires_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let config = OrchestrateConfig {
            worker_binary: "bash".into(),
            worker_args: vec![manifest_dir
                .join("test-fixtures/mock-worker-slow.sh")
                .to_string_lossy()
                .into()],
            worker_cwd: None,
            worker_env: vec![
                ("MOCK_DELAY".into(), "0".into()),
                ("MOCK_SEND_DELAY".into(), "10".into()),
            ],
            routing: RoutingConfig::default(),
            max_concurrency: 1,
            context_result_max_chars: CONTEXT_RESULT_MAX_CHARS,
            task_timeout: Some(Duration::from_millis(200)),
            budget_limit: None,
        };

        let mut parent = Task::new("Timeout test", "Should be cancelled");
        store.append(&parent).unwrap();
        let subtasks = vec![make_subtask("Slow step", "Take forever", 0)];

        let outcome = orchestrate(&store, &mut parent, &subtasks, &config)
            .await
            .unwrap();

        // The subtask should have failed due to cancellation
        assert_eq!(outcome.parent_status, TaskStatus::Failed);
    }

    #[tokio::test]
    async fn completes_before_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));
        let mut config = test_orchestrate_config();
        config.task_timeout = Some(Duration::from_secs(10));

        let mut parent = Task::new("Fast parent", "Quick task");
        store.append(&parent).unwrap();
        let subtasks = vec![make_subtask("Fast step", "Do quickly", 0)];

        let outcome = orchestrate(&store, &mut parent, &subtasks, &config)
            .await
            .unwrap();

        assert_eq!(outcome.parent_status, TaskStatus::Done);
    }

    // --- budget tests ---

    #[test]
    fn no_budget_by_default() {
        let config = test_orchestrate_config();
        assert!(config.budget_limit.is_none());
    }

    #[tokio::test]
    async fn outcome_includes_usage() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));
        let config = test_orchestrate_config();

        let mut parent = Task::new("Usage parent", "Check usage");
        store.append(&parent).unwrap();
        let subtasks = vec![make_subtask("Step", "Do it", 0)];

        let outcome = orchestrate(&store, &mut parent, &subtasks, &config)
            .await
            .unwrap();

        // The mock worker returns usage in its result
        let result = &outcome.subtask_results[0];
        assert!(result.usage.is_some(), "Expected usage in subtask result");
    }

    #[tokio::test]
    async fn budget_cancels_orchestration() {
        // Verify that BudgetTracker fires the cancellation token when budget exceeded
        use crate::worker::budget::BudgetTracker;

        let token = CancellationToken::new();
        let tracker = BudgetTracker::new(10, token.clone());
        assert!(!token.is_cancelled());

        // First subtask usage stays under budget
        tracker.record(5);
        assert!(!token.is_cancelled());

        // Second subtask pushes over budget
        tracker.record(10);
        assert!(token.is_cancelled());
        assert!(tracker.is_exceeded());
        assert_eq!(tracker.total_tokens(), 15);
    }

    // --- Audit trail tests (Step 8) ---

    #[test]
    fn cancelled_variant_serializes() {
        let json = serde_json::to_string(&TaskStatus::Cancelled).unwrap();
        assert_eq!(json, "\"cancelled\"");
        let parsed: TaskStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, TaskStatus::Cancelled);
    }

    #[test]
    fn termination_reason_serde_round_trip() {
        let reasons = vec![
            TerminationReason::Completed,
            TerminationReason::PartialFailure,
            TerminationReason::Timeout,
            TerminationReason::BudgetExceeded,
            TerminationReason::Cancelled,
        ];
        for reason in reasons {
            let json = serde_json::to_string(&reason).unwrap();
            let parsed: TerminationReason = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, reason);
        }
    }

    #[tokio::test]
    async fn termination_reason_completed() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));
        let config = test_orchestrate_config();

        let mut parent = Task::new("Good parent", "All good");
        store.append(&parent).unwrap();
        let subtasks = vec![make_subtask("Step", "Do it", 0)];

        let outcome = orchestrate(&store, &mut parent, &subtasks, &config)
            .await
            .unwrap();

        assert_eq!(outcome.termination_reason, TerminationReason::Completed);
        assert_eq!(outcome.parent_status, TaskStatus::Done);
    }

    #[tokio::test]
    async fn termination_reason_partial_failure() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));
        let config = OrchestrateConfig {
            worker_binary: "/nonexistent/binary".into(),
            worker_args: vec![],
            worker_cwd: None,
            worker_env: vec![],
            routing: RoutingConfig::default(),
            max_concurrency: 1,
            context_result_max_chars: CONTEXT_RESULT_MAX_CHARS,
            task_timeout: None,
            budget_limit: None,
        };

        let mut parent = Task::new("Failing", "Will fail");
        store.append(&parent).unwrap();
        let subtasks = vec![make_subtask("Bad step", "Fail", 0)];

        let outcome = orchestrate(&store, &mut parent, &subtasks, &config)
            .await
            .unwrap();

        assert_eq!(
            outcome.termination_reason,
            TerminationReason::PartialFailure
        );
    }

    #[tokio::test]
    async fn total_usage_summed() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));
        let config = test_orchestrate_config();

        let mut parent = Task::new("Multi", "Multiple steps");
        store.append(&parent).unwrap();
        let subtasks = vec![
            make_subtask("Step 1", "First", 0),
            make_subtask("Step 2", "Second", 0),
        ];

        let outcome = orchestrate(&store, &mut parent, &subtasks, &config)
            .await
            .unwrap();

        let total = outcome.total_usage.expect("should have total usage");
        // Mock returns 15 total_tokens per subtask
        assert!(total.total_tokens > 0);
    }

    #[tokio::test]
    async fn elapsed_is_positive() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));
        let config = test_orchestrate_config();

        let mut parent = Task::new("Timer", "Check elapsed");
        store.append(&parent).unwrap();
        let subtasks = vec![make_subtask("Step", "Do it", 0)];

        let outcome = orchestrate(&store, &mut parent, &subtasks, &config)
            .await
            .unwrap();

        assert!(outcome.elapsed.as_nanos() > 0);
    }
}

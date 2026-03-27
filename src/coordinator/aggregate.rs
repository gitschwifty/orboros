use std::fmt::Write as _;

use tracing::{info, warn};

use crate::orchestrator::SubtaskResult;
use crate::worker::process::{Worker, WorkerConfig};

/// Result of an aggregation attempt.
#[derive(Debug)]
pub struct AggregateResult {
    /// Synthesized summary of all subtask results.
    pub summary: String,
    /// Raw response from the aggregator (for debugging).
    pub raw_response: String,
}

const AGGREGATE_SYSTEM_PROMPT: &str = r"You are a result synthesizer for a software development orchestrator called Orboros.

Given an original task and the results of subtasks that were executed to accomplish it, synthesize a coherent, complete answer.

Your response should:
- Directly answer the original task
- Integrate findings from all subtasks into a unified narrative
- Be concise but complete — don't just concatenate the subtask outputs
- Highlight any conflicts or issues found across subtasks
- If subtasks failed, note what was accomplished and what remains

Respond with the synthesized answer only. No meta-commentary about the synthesis process.";

/// Builds the prompt sent to the aggregation worker.
pub fn build_aggregate_prompt(original_task: &str, subtask_results: &[SubtaskResult]) -> String {
    let mut prompt = format!("Original task: {original_task}\n\nSubtask results:\n");
    for (i, r) in subtask_results.iter().enumerate() {
        let status = if r.status == crate::state::task::TaskStatus::Done {
            "completed"
        } else {
            "failed"
        };
        let response = r.response.as_deref().unwrap_or("(no response)");
        let _ = writeln!(prompt, "{}. [{}] {}: {}", i + 1, status, r.title, response);
    }
    let _ = write!(
        prompt,
        "\nSynthesize these results into a coherent, complete answer to the original task."
    );
    prompt
}

/// Synthesizes subtask results into a coherent final answer using an LLM worker.
///
/// # Errors
///
/// Returns an error if the aggregation worker fails or returns no response.
pub async fn aggregate(
    original_task: &str,
    subtask_results: &[SubtaskResult],
    worker_config: &WorkerConfig,
) -> anyhow::Result<AggregateResult> {
    let aggregator_config = WorkerConfig {
        command: worker_config.command.clone(),
        args: worker_config.args.clone(),
        cwd: worker_config.cwd.clone(),
        env: worker_config.env.clone(),
        model: worker_config.model.clone(),
        system_prompt: AGGREGATE_SYSTEM_PROMPT.into(),
        tools: vec![],
        max_iterations: Some(1),
        init_timeout: worker_config.init_timeout,
        send_timeout: worker_config.send_timeout,
        shutdown_timeout: worker_config.shutdown_timeout,
    };

    info!("Spawning aggregation worker");
    let mut worker = Worker::spawn(&aggregator_config).await?;

    let prompt = build_aggregate_prompt(original_task, subtask_results);
    let outcome = worker.send("aggregate-1", &prompt).await?;

    if let Err(e) = worker.shutdown().await {
        warn!("Aggregation worker shutdown failed: {e}");
    }

    if outcome.status != crate::ipc::types::ResultStatus::Ok {
        anyhow::bail!(
            "Aggregation returned error: {}",
            outcome
                .error
                .as_ref()
                .map_or("unknown error", |e| &*e.message)
        );
    }

    let raw_response = outcome
        .response
        .ok_or_else(|| anyhow::anyhow!("Aggregation returned no response"))?;

    info!("Aggregation complete");

    Ok(AggregateResult {
        summary: raw_response.clone(),
        raw_response,
    })
}

/// Fallback: concatenates subtask results with headers when aggregation fails.
pub fn fallback_concatenate(subtask_results: &[SubtaskResult]) -> String {
    subtask_results
        .iter()
        .filter_map(|r| {
            r.response
                .as_ref()
                .map(|resp| format!("## {}\n{}", r.title, resp))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::task::TaskStatus;
    use std::path::PathBuf;
    use uuid::Uuid;

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

    fn make_result(title: &str, response: &str) -> SubtaskResult {
        SubtaskResult {
            task_id: Uuid::new_v4(),
            title: title.into(),
            order: 0,
            status: TaskStatus::Done,
            response: Some(response.into()),
            usage: None,
            retries: 0,
        }
    }

    #[test]
    fn aggregate_prompt_format() {
        let results = vec![
            make_result("Research", "Found three patterns"),
            make_result("Implement", "Created the module"),
        ];

        let prompt = build_aggregate_prompt("Build a feature", &results);
        assert!(prompt.contains("Original task: Build a feature"));
        assert!(prompt.contains("1. [completed] Research: Found three patterns"));
        assert!(prompt.contains("2. [completed] Implement: Created the module"));
        assert!(prompt.contains("Synthesize these results"));
    }

    #[test]
    fn aggregate_prompt_with_failed_subtask() {
        let results = vec![
            make_result("Good step", "Worked fine"),
            SubtaskResult {
                task_id: Uuid::new_v4(),
                title: "Bad step".into(),
                order: 1,
                status: TaskStatus::Failed,
                response: Some("Worker spawn failed".into()),
                usage: None,
                retries: 0,
            },
        ];

        let prompt = build_aggregate_prompt("Do something", &results);
        assert!(prompt.contains("[completed] Good step"));
        assert!(prompt.contains("[failed] Bad step"));
    }

    #[test]
    fn aggregate_prompt_with_no_response() {
        let results = vec![SubtaskResult {
            task_id: Uuid::new_v4(),
            title: "Silent step".into(),
            order: 0,
            status: TaskStatus::Failed,
            response: None,
            usage: None,
            retries: 0,
        }];

        let prompt = build_aggregate_prompt("Task", &results);
        assert!(prompt.contains("(no response)"));
    }

    #[tokio::test]
    async fn aggregate_with_mock_worker() {
        let results = vec![
            make_result("Step 1", "Result one"),
            make_result("Step 2", "Result two"),
        ];

        let result = aggregate("Build a thing", &results, &mock_worker_config())
            .await
            .unwrap();

        // Mock worker always returns "Hello from mock worker"
        assert_eq!(result.summary, "Hello from mock worker");
        assert_eq!(result.raw_response, "Hello from mock worker");
    }

    #[test]
    fn fallback_concatenate_formats_correctly() {
        let results = vec![
            make_result("Research", "Found patterns"),
            make_result("Implement", "Created module"),
        ];

        let output = fallback_concatenate(&results);
        assert!(output.contains("## Research\nFound patterns"));
        assert!(output.contains("## Implement\nCreated module"));
    }

    #[test]
    fn fallback_concatenate_skips_none_responses() {
        let results = vec![
            make_result("Good", "Has response"),
            SubtaskResult {
                task_id: Uuid::new_v4(),
                title: "Bad".into(),
                order: 0,
                status: TaskStatus::Failed,
                response: None,
                usage: None,
                retries: 0,
            },
        ];

        let output = fallback_concatenate(&results);
        assert!(output.contains("## Good"));
        assert!(!output.contains("## Bad"));
    }
}

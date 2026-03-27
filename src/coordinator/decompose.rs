use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::ipc::types::ResultStatus;
use crate::worker::process::{Worker, WorkerConfig};

/// A subtask produced by the coordinator's decomposition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subtask {
    /// Short title for the subtask.
    pub title: String,
    /// Detailed description of what to do.
    pub description: String,
    /// Type of work: research, edit, review, etc.
    pub worker_type: String,
    /// Tools the worker needs for this subtask.
    #[serde(default)]
    pub tools_needed: Vec<String>,
    /// Priority relative to siblings (lower = first).
    #[serde(default = "default_order")]
    pub order: u32,
}

fn default_order() -> u32 {
    0
}

/// Result of a decomposition attempt.
#[derive(Debug)]
pub struct DecomposeResult {
    pub subtasks: Vec<Subtask>,
    /// Raw response from the coordinator (for debugging).
    pub raw_response: String,
}

const DECOMPOSE_SYSTEM_PROMPT: &str = r#"You are a task coordinator for a software development orchestrator called Orboros.

Given a high-level task, break it into ordered subtasks. Each subtask should be specific enough for a single focused worker to complete.

Respond with ONLY a JSON array of subtask objects. No markdown, no explanation, just the JSON array.

Each subtask object must have:
- "title": short title (under 80 chars)
- "description": detailed description of what to do
- "worker_type": one of "research", "edit", "review", "test"
- "tools_needed": array of tool names needed (can be empty)
- "order": execution order (0-based, lower = first, same number = can run in parallel)

Example response:
[
  {"title": "Research auth patterns", "description": "Survey common authentication patterns for REST APIs including JWT, OAuth2, and session-based auth. Summarize pros and cons.", "worker_type": "research", "tools_needed": [], "order": 0},
  {"title": "Implement JWT middleware", "description": "Create a JWT validation middleware that extracts and verifies tokens from the Authorization header.", "worker_type": "edit", "tools_needed": ["read_file", "write_file", "glob"], "order": 1},
  {"title": "Review implementation", "description": "Review the JWT middleware for security issues, edge cases, and adherence to best practices.", "worker_type": "review", "tools_needed": ["read_file", "glob"], "order": 2}
]"#;

/// Decomposes a high-level task into subtasks using an LLM coordinator worker.
///
/// # Errors
///
/// Returns an error if the coordinator worker fails or returns unparseable output.
pub async fn decompose(
    task_description: &str,
    worker_config: &WorkerConfig,
) -> anyhow::Result<DecomposeResult> {
    // Build coordinator config — same binary, but with the decomposition system prompt
    let coordinator_config = WorkerConfig {
        command: worker_config.command.clone(),
        args: worker_config.args.clone(),
        cwd: worker_config.cwd.clone(),
        env: worker_config.env.clone(),
        model: worker_config.model.clone(),
        system_prompt: DECOMPOSE_SYSTEM_PROMPT.into(),
        tools: vec![],           // coordinator doesn't need tools
        max_iterations: Some(1), // single-turn, no tool loop
        init_timeout: worker_config.init_timeout,
        send_timeout: worker_config.send_timeout,
        shutdown_timeout: worker_config.shutdown_timeout,
    };

    info!("Spawning coordinator worker for task decomposition");
    let mut worker = Worker::spawn(&coordinator_config).await?;

    let outcome = worker.send("decompose-1", task_description).await?;

    // Best-effort shutdown
    if let Err(e) = worker.shutdown().await {
        warn!("Coordinator worker shutdown failed: {e}");
    }

    if outcome.status != ResultStatus::Ok {
        anyhow::bail!(
            "Coordinator returned error: {}",
            outcome.error.unwrap_or_else(|| "unknown error".into())
        );
    }

    let raw_response = outcome
        .response
        .ok_or_else(|| anyhow::anyhow!("Coordinator returned no response"))?;

    let subtasks = parse_subtasks(&raw_response)?;

    info!(count = subtasks.len(), "Decomposed into subtasks");

    Ok(DecomposeResult {
        subtasks,
        raw_response,
    })
}

/// Parses the coordinator's JSON response into subtasks.
/// Handles common LLM quirks: markdown code fences, leading text.
fn parse_subtasks(raw: &str) -> anyhow::Result<Vec<Subtask>> {
    // Try direct parse first
    if let Ok(subtasks) = serde_json::from_str::<Vec<Subtask>>(raw.trim()) {
        return Ok(subtasks);
    }

    // Strip markdown code fences if present
    let stripped = strip_code_fences(raw);
    if let Ok(subtasks) = serde_json::from_str::<Vec<Subtask>>(&stripped) {
        return Ok(subtasks);
    }

    // Try to find a JSON array in the response
    if let Some(start) = raw.find('[') {
        if let Some(end) = raw.rfind(']') {
            let slice = &raw[start..=end];
            if let Ok(subtasks) = serde_json::from_str::<Vec<Subtask>>(slice) {
                return Ok(subtasks);
            }
        }
    }

    anyhow::bail!("Failed to parse coordinator response as subtask array. Raw response:\n{raw}")
}

/// Strips ```json ... ``` code fences from LLM output.
fn strip_code_fences(s: &str) -> String {
    let trimmed = s.trim();
    if let Some(rest) = trimmed.strip_prefix("```json") {
        if let Some(content) = rest.strip_suffix("```") {
            return content.trim().to_string();
        }
    }
    if let Some(rest) = trimmed.strip_prefix("```") {
        if let Some(content) = rest.strip_suffix("```") {
            return content.trim().to_string();
        }
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_clean_json() {
        let input = r#"[
            {"title": "Research", "description": "Do research", "worker_type": "research", "tools_needed": [], "order": 0},
            {"title": "Implement", "description": "Write code", "worker_type": "edit", "tools_needed": ["read_file"], "order": 1}
        ]"#;
        let subtasks = parse_subtasks(input).unwrap();
        assert_eq!(subtasks.len(), 2);
        assert_eq!(subtasks[0].title, "Research");
        assert_eq!(subtasks[1].worker_type, "edit");
    }

    #[test]
    fn parse_with_code_fences() {
        let input = r#"```json
[
    {"title": "Test", "description": "Run tests", "worker_type": "test", "tools_needed": [], "order": 0}
]
```"#;
        let subtasks = parse_subtasks(input).unwrap();
        assert_eq!(subtasks.len(), 1);
        assert_eq!(subtasks[0].title, "Test");
    }

    #[test]
    fn parse_with_leading_text() {
        let input = r#"Here are the subtasks:

[{"title": "Review", "description": "Review code", "worker_type": "review", "tools_needed": [], "order": 0}]

Hope that helps!"#;
        let subtasks = parse_subtasks(input).unwrap();
        assert_eq!(subtasks.len(), 1);
    }

    #[test]
    fn parse_with_defaults() {
        let input =
            r#"[{"title": "Minimal", "description": "Minimal task", "worker_type": "research"}]"#;
        let subtasks = parse_subtasks(input).unwrap();
        assert_eq!(subtasks[0].order, 0);
        assert!(subtasks[0].tools_needed.is_empty());
    }

    #[test]
    fn parse_invalid_fails() {
        let result = parse_subtasks("this is not json at all");
        assert!(result.is_err());
    }

    #[test]
    fn strip_fences_json() {
        assert_eq!(strip_code_fences("```json\n[1,2,3]\n```"), "[1,2,3]");
    }

    #[test]
    fn strip_fences_plain() {
        assert_eq!(strip_code_fences("```\n[1,2,3]\n```"), "[1,2,3]");
    }

    #[test]
    fn strip_fences_no_fences() {
        assert_eq!(strip_code_fences("[1,2,3]"), "[1,2,3]");
    }

    #[test]
    fn subtask_round_trip() {
        let subtask = Subtask {
            title: "Test task".into(),
            description: "Do something".into(),
            worker_type: "edit".into(),
            tools_needed: vec!["glob".into()],
            order: 2,
        };
        let json = serde_json::to_string(&subtask).unwrap();
        let parsed: Subtask = serde_json::from_str(&json).unwrap();
        assert_eq!(subtask.title, parsed.title);
        assert_eq!(subtask.order, parsed.order);
    }
}

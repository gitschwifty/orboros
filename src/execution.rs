//! Append-only per-dispatch execution evidence.
//!
//! This is intentionally separate from orb snapshots: an orb retains only its
//! latest execution metadata, while this ledger preserves every phase and
//! retry without growing `orbs.jsonl` records.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::worker::dispatcher::{DispatchOutcome, DispatchStatus, TerminalRetryDiagnostic};

/// Character-level attribution for Orboros-owned prompt construction.
///
/// This deliberately measures only text Orboros injects or constructs. It
/// cannot account for opaque provider/runtime context managed by Heddle.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PromptContextMetrics {
    pub base_user_chars: u32,
    pub task_context_chars: u32,
    pub task_context_overhead_chars: u32,
    pub current_orb_chars: u32,
    pub parent_and_root_chars: u32,
    pub sibling_orbs_chars: u32,
    pub child_orbs_chars: u32,
    pub upstream_dependency_chars: u32,
    pub final_user_prompt_chars: u32,
    pub effective_system_prompt_chars: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionRecord {
    pub orb_id: String,
    pub parent_id: Option<String>,
    pub dispatch_kind: String,
    /// Resolved phase tool-policy key/profile. Missing for historical records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_policy: Option<String>,
    /// Where the policy selection came from, such as `phase_default` or a
    /// benchmark `case_override`. Missing for historical records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_policy_source: Option<String>,
    /// Concrete Heddle tools available to this dispatch after policy and base
    /// ceilings were intersected. Missing is distinct from an explicitly empty
    /// tool inventory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    pub status: String,
    pub dispatched_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_micros: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    #[serde(default)]
    pub retries: u32,
    /// Orboros-owned user/system prompt construction. Missing for historical
    /// records and deliberately excludes opaque Heddle/provider context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_context: Option<PromptContextMetrics>,
    /// Evidence from the one permitted fresh-worker repair of a malformed
    /// decomposition response. Kept on the repair dispatch record so the
    /// original dispatch remains an unmodified account of what it returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decomposition_repair: Option<DecompositionRepairDiagnostic>,
    /// Evidence for a single fresh-worker retry after Heddle reported a
    /// structured terminal loop or iteration limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_retry: Option<TerminalRetryDiagnostic>,
}

/// Attribution for a bounded decomposition JSON repair attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecompositionRepairDiagnostic {
    /// The normal parser failed before this repair worker was started.
    pub initial_parse_error: String,
    /// The original worker had already shut down, so this is always a fresh
    /// session rather than a misleading continuation claim.
    pub same_session_repair_available: bool,
    /// Whether the single permitted fresh-worker repair was dispatched.
    pub repair_attempted: bool,
    /// Whether the repair response passed the normal decomposition parser.
    pub repair_succeeded: bool,
    /// Why the repair response could not be used, when it was attempted but
    /// did not produce a valid plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair_parse_error: Option<String>,
    /// Confidence emitted with the malformed original response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_confidence: Option<f32>,
    /// Confidence emitted by the repair response. This is the confidence used
    /// for a successfully repaired decomposition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repaired_confidence: Option<f32>,
}

impl ExecutionRecord {
    pub fn from_outcome(
        orb: &orbs::orb::Orb,
        dispatch_kind: impl Into<String>,
        tool_policy: impl Into<String>,
        tool_policy_source: Option<String>,
        allowed_tools: Vec<String>,
        outcome: &DispatchOutcome,
        prompt_context: Option<PromptContextMetrics>,
    ) -> Self {
        Self {
            orb_id: orb.id.to_string(),
            parent_id: orb.parent_id.as_ref().map(ToString::to_string),
            dispatch_kind: dispatch_kind.into(),
            tool_policy: Some(tool_policy.into()),
            tool_policy_source,
            allowed_tools: Some(allowed_tools),
            status: match outcome.status {
                DispatchStatus::Done => "done",
                DispatchStatus::Error => "error",
                DispatchStatus::Failed => "failed",
                DispatchStatus::Cancelled => "cancelled",
                DispatchStatus::Aborted => "aborted",
            }
            .into(),
            dispatched_at: outcome.dispatched_at,
            completed_at: outcome.completed_at,
            worker_model: Some(outcome.worker_model.clone()),
            model_latency_ms: outcome.model_latency_ms,
            tool_latency_ms: outcome.tool_latency_ms,
            total_latency_ms: outcome.total_latency_ms,
            assistant_turns: outcome.assistant_turns,
            tool_calls: outcome.tool_calls,
            prompt_tokens: outcome.prompt_tokens,
            completion_tokens: outcome.completion_tokens,
            total_tokens: outcome.total_tokens,
            cost_micros: outcome.cost_micros,
            cache_read_tokens: outcome.cached_tokens,
            cache_write_tokens: outcome.cache_write_tokens,
            retries: outcome.retries,
            prompt_context,
            decomposition_repair: None,
            terminal_retry: outcome.terminal_retry.clone(),
        }
    }
}

/// Durable snapshot of one worker's resolved initial prompts.
///
/// This is deliberately separate from execution outcomes: it remains useful
/// when a worker fails before returning telemetry, and it prevents prompt text
/// from inflating append-only orb snapshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptRecord {
    pub orb_id: String,
    pub parent_id: Option<String>,
    pub dispatch_kind: String,
    pub dispatched_at: DateTime<Utc>,
    pub system_prompt: String,
    pub user_prompt: String,
    pub system_prompt_hash: String,
    pub user_prompt_hash: String,
    /// Provider-reported input tokens for this dispatch, when available.
    /// This may include runtime/provider context not represented in the saved
    /// Orboros prompt snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    pub prompt_context: PromptContextMetrics,
}

impl PromptRecord {
    #[must_use]
    pub fn new(
        orb: &orbs::orb::Orb,
        dispatch_kind: impl Into<String>,
        dispatched_at: DateTime<Utc>,
        system_prompt: String,
        user_prompt: String,
        input_tokens: Option<u64>,
        prompt_context: PromptContextMetrics,
    ) -> Self {
        Self {
            orb_id: orb.id.to_string(),
            parent_id: orb.parent_id.as_ref().map(ToString::to_string),
            dispatch_kind: dispatch_kind.into(),
            system_prompt_hash: crate::prompt::prompt_hash(&system_prompt),
            user_prompt_hash: crate::prompt::prompt_hash(&user_prompt),
            system_prompt,
            user_prompt,
            dispatched_at,
            input_tokens,
            prompt_context,
        }
    }
}

#[derive(Clone)]
pub struct ExecutionStore {
    path: PathBuf,
}
impl ExecutionStore {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn append(&self, record: &ExecutionRecord) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        serde_json::to_writer(&mut file, record).map_err(std::io::Error::other)?;
        file.write_all(b"\n")
    }
    pub fn read_all(&self) -> std::io::Result<Vec<ExecutionRecord>> {
        let Ok(file) = std::fs::File::open(&self.path) else {
            return Ok(vec![]);
        };
        Ok(BufReader::new(file)
            .lines()
            .filter_map(|line| line.ok().and_then(|line| serde_json::from_str(&line).ok()))
            .collect())
    }
}

/// Append-only prompt ledger colocated with the execution ledger.
#[derive(Clone)]
pub struct PromptStore {
    path: PathBuf,
}

impl PromptStore {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn append(&self, record: &PromptRecord) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        serde_json::to_writer(&mut file, record).map_err(std::io::Error::other)?;
        file.write_all(b"\n")
    }

    pub fn read_all(&self) -> std::io::Result<Vec<PromptRecord>> {
        let Ok(file) = std::fs::File::open(&self.path) else {
            return Ok(vec![]);
        };
        Ok(BufReader::new(file)
            .lines()
            .filter_map(|line| line.ok().and_then(|line| serde_json::from_str(&line).ok()))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::ExecutionRecord;

    #[test]
    fn execution_record_reads_historical_rows_without_tool_telemetry() {
        let record: ExecutionRecord = serde_json::from_str(
            r#"{
                "orb_id":"orb-1",
                "parent_id":null,
                "dispatch_kind":"execute",
                "status":"done",
                "dispatched_at":"2026-08-01T00:00:00Z",
                "completed_at":"2026-08-01T00:00:01Z"
            }"#,
        )
        .unwrap();
        assert_eq!(record.tool_policy, None);
        assert_eq!(record.allowed_tools, None);
    }

    #[test]
    fn execution_record_distinguishes_empty_and_unknown_tool_inventory() {
        let record: ExecutionRecord = serde_json::from_str(
            r#"{
                "orb_id":"orb-1",
                "parent_id":null,
                "dispatch_kind":"execute",
                "tool_policy":"execute",
                "allowed_tools":[],
                "status":"done",
                "dispatched_at":"2026-08-01T00:00:00Z",
                "completed_at":"2026-08-01T00:00:01Z"
            }"#,
        )
        .unwrap();
        assert_eq!(record.tool_policy.as_deref(), Some("execute"));
        assert_eq!(record.allowed_tools, Some(Vec::new()));
    }
}

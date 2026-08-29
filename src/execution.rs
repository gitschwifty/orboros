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

use crate::worker::dispatcher::{
    DispatchAttempt, DispatchOutcome, DispatchStatus, TerminalRetryDiagnostic,
};

/// Renders provider cost for operator-facing output while the durable ledger
/// continues to retain exact microdollar values for aggregation.
#[must_use]
pub fn format_cost_usd(cost_micros: Option<u64>) -> String {
    cost_micros.map_or_else(
        || "-".into(),
        |micros| format!("${}.{:06}", micros / 1_000_000, micros % 1_000_000),
    )
}

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
    /// Evidence for bounded repair and fresh retry of a structured runtime
    /// phase response. This is the queue-path counterpart to benchmark
    /// recovery and applies to Speccing, Decomposing, and Reevaluating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_output_recovery: Option<PhaseOutputRecoveryDiagnostic>,
    /// Phase-attempt provenance for a bounded clean retry. Kept separate from
    /// worker-level terminal retries, which are attempts within one dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_retry: Option<PhaseRetryDiagnostic>,
    /// Evidence for a single fresh-worker retry after Heddle reported a
    /// structured terminal loop or iteration limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_retry: Option<TerminalRetryDiagnostic>,
    /// Evidence for a single recovery dispatch after an exhausted terminal
    /// worker retry left a materially changed workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_artifact_recovery: Option<PartialArtifactRecoveryDiagnostic>,
    /// Per-round evidence for a configured Refining loop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refinement_round: Option<RefinementRoundDiagnostic>,
    /// Provenance for one worker dispatch within a Refining round. This is
    /// deliberately attached to every dispatch record rather than inferred
    /// from the mutable orb snapshot, so rejected output and repairs remain
    /// queryable after reset or rollback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refinement_attempt: Option<RefinementAttemptDiagnostic>,
    /// Every worker attempt, including fresh retries that did not produce a
    /// final successful result. Missing on historical records.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<DispatchAttempt>,
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
    /// Whether a clean decomposition retry followed this failed repair.
    #[serde(default)]
    pub fresh_retry_attempted: bool,
    /// Whether that clean retry produced a valid plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fresh_retry_succeeded: Option<bool>,
    /// Why the clean retry could not be used, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fresh_retry_parse_error: Option<String>,
}

/// Attribution for a complete phase retry rather than a repair sub-attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhaseRetryDiagnostic {
    /// The ordinal complete-phase attempt. The original dispatch is attempt 1.
    pub attempt: u32,
    /// Stable eligibility reason, for example `after_invalid_output`.
    pub reason: String,
}

/// Durable provenance for a malformed structured phase response and its
/// bounded recovery. Raw response bodies remain only in worker transcripts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseOutputRecoveryDiagnostic {
    pub phase: String,
    pub initial_validation_error: String,
    pub repair_attempted: bool,
    pub repair_succeeded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair_error: Option<String>,
    pub fresh_retry_attempted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fresh_retry_succeeded: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fresh_retry_error: Option<String>,
}

/// Durable termination evidence for one Refining phase round.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefinementRoundDiagnostic {
    pub round: u32,
    pub max_rounds: u32,
    /// Full-loop automated quality-review ordinal, including the initial
    /// review. This is distinct from the worker retries within a round.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality_review_attempt: Option<u32>,
    /// Configured cap for automated quality-review attempts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_quality_review_attempts: Option<u32>,
    pub material_changed: bool,
    pub model_declared_complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination_reason: Option<String>,
}

/// Stable lineage for a worker dispatch belonging to a Refining round.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefinementAttemptDiagnostic {
    /// The logical refinement round, starting at one.
    pub round: u32,
    /// The normal round attempt ordinal. Format repair is attached to the
    /// normal attempt it repairs and does not advance this counter.
    pub attempt: u32,
    /// `initial`, `completed_round_retry`, or
    /// `after_invalid_refinement_output` for the repair dispatch.
    pub reason: String,
    /// The normal attempt that caused this dispatch, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_attempt: Option<u32>,
}

/// Attribution for the one bounded recovery pass permitted after a failed
/// child dispatch has exhausted its terminal worker retry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartialArtifactRecoveryDiagnostic {
    /// The recovery protocol never retries itself.
    pub recovery_attempted: bool,
    /// Whether a before/after fingerprint showed a real repository change.
    pub workspace_changed: bool,
    /// The assigned repository root inspected by the recovery worker.
    pub workdir: String,
    /// The failed dispatch's final error, retained even if recovery succeeds.
    pub initial_error: Option<String>,
    /// Whether the recovery worker completed its bounded verification pass.
    pub recovery_succeeded: bool,
    /// Transcript/artifact path reported by Heddle for the recovery attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_artifact_path: Option<String>,
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
            phase_output_recovery: None,
            phase_retry: None,
            terminal_retry: outcome.terminal_retry.clone(),
            partial_artifact_recovery: None,
            refinement_round: None,
            refinement_attempt: None,
            attempts: outcome.attempts.clone(),
        }
    }
}

#[cfg(test)]
mod attempt_tests {
    use chrono::Utc;
    use orbs::orb::{Orb, OrbType};

    use super::*;
    use crate::worker::dispatcher::{DispatchAttempt, DispatchOutcome, DispatchStatus};

    #[test]
    fn cost_display_uses_usd_without_losing_micro_precision() {
        assert_eq!(format_cost_usd(Some(472)), "$0.000472");
        assert_eq!(format_cost_usd(Some(1_250_000)), "$1.250000");
        assert_eq!(format_cost_usd(None), "-");
    }

    #[test]
    fn execution_record_retains_every_worker_attempt() {
        let now = Utc::now();
        let outcome = DispatchOutcome {
            status: DispatchStatus::Error,
            retries: 1,
            response: None,
            confidence: None,
            worker_model: "mock/model".into(),
            worker_id: Some("retry-worker".into()),
            session_id: Some("retry-session".into()),
            runtime: None,
            routing: None,
            model_latency_ms: None,
            tool_latency_ms: None,
            total_latency_ms: None,
            assistant_turns: None,
            tool_calls: None,
            prompt_tokens: None,
            completion_tokens: None,
            total_tokens: None,
            cost_micros: None,
            cost_currency: None,
            cached_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            generation_id: None,
            dispatched_at: now,
            completed_at: now,
            prompt_category: None,
            system_prompt_hash: None,
            system_prompt_source: None,
            error: Some("worker send failed: stream decode".into()),
            failure: None,
            terminal_retry: None,
            attempts: vec![
                DispatchAttempt {
                    worker_id: Some("first-worker".into()),
                    session_id: Some("first-session".into()),
                    dispatched_at: now,
                    completed_at: now,
                    status: "error".into(),
                    error: Some("worker send failed: stream decode".into()),
                    failure: None,
                    runtime: None,
                    routing: None,
                    total_tokens: None,
                    cost_micros: None,
                },
                DispatchAttempt {
                    worker_id: Some("retry-worker".into()),
                    session_id: Some("retry-session".into()),
                    dispatched_at: now,
                    completed_at: now,
                    status: "error".into(),
                    error: Some("worker send failed: stream decode".into()),
                    failure: None,
                    runtime: None,
                    routing: None,
                    total_tokens: None,
                    cost_micros: None,
                },
            ],
        };
        let orb = Orb::new("task", "description").with_type(OrbType::Task);
        let record = ExecutionRecord::from_outcome(
            &orb,
            "worker.execute",
            "execute",
            None,
            Vec::new(),
            &outcome,
            None,
        );

        assert_eq!(record.attempts.len(), 2);
        assert_eq!(
            record.attempts[0].session_id.as_deref(),
            Some("first-session")
        );
        assert_eq!(
            record.attempts[1].session_id.as_deref(),
            Some("retry-session")
        );
        let json = serde_json::to_value(record).unwrap();
        assert_eq!(json["attempts"].as_array().unwrap().len(), 2);
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

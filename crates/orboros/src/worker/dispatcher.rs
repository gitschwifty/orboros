//! Orb → worker dispatch (task 60).
//!
//! Single source of truth for spawning a worker against an orb and
//! writing the result back. Replaces the per-phase ad-hoc dispatch
//! pattern that today only `phases::second_opinion::run_reviewer`
//! exercises.
//!
//! Phases produce prompts and apply results; this module owns the
//! spawn/send/shutdown machinery and the lifecycle hook firing
//! (`pre-worker-spawn` / `post-worker-complete` / `post-worker-fail`).

use std::time::Instant;

use chrono::Utc;
use orbs::orb::{ExecutionMeta, Orb, OrbPhase, OrbStatus};
use tracing::{info, instrument, warn};

use crate::hooks::event::HookEvent;
use crate::hooks::runner::{FireCtx, FireOutcome};
use crate::hooks::sink::HookSink;
use crate::worker::process::{Worker, WorkerConfig};

/// Reduced view of a worker's `SendOutcome` keyed to what the orb
/// actually stores. `Aborted` is distinct from `Failed` because it
/// signals a `pre-worker-spawn` hook returning exit 2 — no worker
/// was ever spawned.
#[derive(Debug, Clone, PartialEq)]
pub enum DispatchStatus {
    Done,
    Failed,
    Cancelled,
    /// A `pre-worker-spawn` hook returned exit 2. The orb is left
    /// in its current state — no fields are written.
    Aborted,
}

/// What the dispatcher produces. `apply_dispatch_outcome` consumes
/// this to mutate the orb in place.
#[derive(Debug, Clone, PartialEq)]
pub struct DispatchOutcome {
    pub status: DispatchStatus,
    pub response: Option<String>,
    pub confidence: Option<f32>,
    pub worker_model: String,
    pub worker_id: Option<String>,
    pub model_latency_ms: Option<u64>,
    pub tool_latency_ms: Option<u64>,
    pub total_latency_ms: Option<u64>,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
    pub dispatched_at: chrono::DateTime<Utc>,
    pub completed_at: chrono::DateTime<Utc>,
    pub error: Option<String>,
}

impl DispatchOutcome {
    /// Builds a `DispatchOutcome` representing an aborted spawn —
    /// the dispatcher returns this when a pre-worker-spawn hook
    /// short-circuits. No timing or worker fields are populated;
    /// timestamps are set to "now" for completeness.
    fn aborted(worker_model: String, reason: String) -> Self {
        let now = Utc::now();
        Self {
            status: DispatchStatus::Aborted,
            response: None,
            confidence: None,
            worker_model,
            worker_id: None,
            model_latency_ms: None,
            tool_latency_ms: None,
            total_latency_ms: None,
            prompt_tokens: None,
            completion_tokens: None,
            total_tokens: None,
            dispatched_at: now,
            completed_at: now,
            error: Some(reason),
        }
    }
}

/// Spawns a worker for `orb` with the given prompt + worker config.
/// Fires `pre-worker-spawn` (sync, gating), `post-worker-complete`,
/// and `post-worker-fail` hooks at appropriate points.
///
/// Returns a [`DispatchOutcome`] that the caller passes to
/// [`apply_dispatch_outcome`] to mutate the orb. Splitting these
/// two responsibilities lets callers persist the orb's pre-dispatch
/// state, run the dispatch, then atomically write back the
/// post-dispatch state.
///
/// # Errors
///
/// Returns an `anyhow::Error` only for catastrophic failures that
/// can't be expressed as a dispatch outcome (currently none — IPC
/// failures fold into `DispatchStatus::Failed`).
#[instrument(
    name = "dispatcher.dispatch_orb",
    skip(orb, prompt, worker_config, hooks),
    fields(orb = %orb.id, model = %worker_config.model)
)]
pub async fn dispatch_orb(
    orb: &Orb,
    prompt: &str,
    worker_config: &WorkerConfig,
    hooks: Option<&HookSink>,
) -> anyhow::Result<DispatchOutcome> {
    // 1. pre-worker-spawn — synchronous, gating. Exit 2 from any
    //    matching hook short-circuits before we spawn anything.
    if let Some(sink) = hooks {
        let (outcome, _invocations) =
            sink.fire_blocking(HookEvent::PreWorkerSpawn, FireCtx::for_orb(orb))?;
        if let FireOutcome::Aborted {
            hook_name,
            exit_code,
        } = outcome
        {
            warn!(
                orb = %orb.id,
                hook = %hook_name,
                exit_code,
                "pre-worker-spawn hook aborted dispatch",
            );
            return Ok(DispatchOutcome::aborted(
                worker_config.model.clone(),
                format!("pre-worker-spawn hook `{hook_name}` aborted with exit {exit_code}"),
            ));
        }
    }

    // 2. Spawn / send / shutdown. Best-effort on shutdown.
    let dispatched_at = Utc::now();
    let started = Instant::now();
    let send_id = format!("{}-dispatch", orb.id);

    let outcome = match Worker::spawn(worker_config).await {
        Ok(mut worker) => match worker.send(&send_id, prompt).await {
            Ok(send_outcome) => {
                let _ = worker.shutdown().await;
                let completed_at = Utc::now();
                build_outcome(
                    orb,
                    worker_config,
                    dispatched_at,
                    completed_at,
                    send_outcome,
                )
            }
            Err(e) => {
                let _ = worker.shutdown().await;
                build_failure(
                    worker_config,
                    dispatched_at,
                    Utc::now(),
                    format!("worker send failed: {e}"),
                )
            }
        },
        Err(e) => build_failure(
            worker_config,
            dispatched_at,
            Utc::now(),
            format!("worker spawn failed: {e}"),
        ),
    };

    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    info!(
        orb = %orb.id,
        status = ?outcome.status,
        elapsed_ms,
        "dispatch_orb complete",
    );

    // 3. post-worker-* — async / fire-and-forget. The event picked
    //    depends on the outcome.
    if let Some(sink) = hooks {
        let event = if outcome.status == DispatchStatus::Done {
            HookEvent::PostWorkerComplete
        } else {
            HookEvent::PostWorkerFail
        };
        // Best-effort — don't fail dispatch over a post-hook error.
        if let Err(e) = sink.fire_blocking(event, FireCtx::for_orb(orb)) {
            warn!(
                orb = %orb.id,
                event = %event,
                error = %e,
                "post-worker hook fire failed (non-fatal)",
            );
        }
    }

    Ok(outcome)
}

fn build_outcome(
    _orb: &Orb,
    worker_config: &WorkerConfig,
    dispatched_at: chrono::DateTime<Utc>,
    completed_at: chrono::DateTime<Utc>,
    send: crate::worker::process::SendOutcome,
) -> DispatchOutcome {
    use crate::ipc::types::ResultStatus;
    let status = match send.status {
        ResultStatus::Ok => DispatchStatus::Done,
        ResultStatus::Cancelled => DispatchStatus::Cancelled,
        ResultStatus::Error => DispatchStatus::Failed,
    };
    let error = send.error.as_ref().map(|e| e.message.clone());
    DispatchOutcome {
        status,
        response: send.response,
        confidence: send.confidence,
        worker_model: worker_config.model.clone(),
        worker_id: worker_config.worker_id.clone(),
        model_latency_ms: send.model_latency_ms,
        tool_latency_ms: send.tool_latency_ms,
        total_latency_ms: send.total_latency_ms,
        prompt_tokens: send.usage.as_ref().map(|u| u.prompt_tokens),
        completion_tokens: send.usage.as_ref().map(|u| u.completion_tokens),
        total_tokens: send.usage.as_ref().map(|u| u.total_tokens),
        dispatched_at,
        completed_at,
        error,
    }
}

fn build_failure(
    worker_config: &WorkerConfig,
    dispatched_at: chrono::DateTime<Utc>,
    completed_at: chrono::DateTime<Utc>,
    message: String,
) -> DispatchOutcome {
    DispatchOutcome {
        status: DispatchStatus::Failed,
        response: None,
        confidence: None,
        worker_model: worker_config.model.clone(),
        worker_id: worker_config.worker_id.clone(),
        model_latency_ms: None,
        tool_latency_ms: None,
        total_latency_ms: None,
        prompt_tokens: None,
        completion_tokens: None,
        total_tokens: None,
        dispatched_at,
        completed_at,
        error: Some(message),
    }
}

/// Mutates `orb` in place based on the dispatch outcome. Sets
/// `result`, `confidence`, and `execution` on Done outcomes;
/// records error in `result` on Failed outcomes; transitions the
/// orb's status or phase per its type.
///
/// `Aborted` is a no-op — the orb is left exactly as the caller
/// passed it. Callers can detect abort via the returned outcome.
///
/// Returns an error if the lifecycle transition isn't permitted
/// (caller bug — orb wasn't in Active/Executing).
pub fn apply_dispatch_outcome(
    orb: &mut Orb,
    outcome: &DispatchOutcome,
) -> Result<(), orbs::orb::TransitionError> {
    if outcome.status == DispatchStatus::Aborted {
        return Ok(());
    }

    orb.execution = Some(ExecutionMeta {
        dispatched_at: Some(outcome.dispatched_at),
        completed_at: Some(outcome.completed_at),
        worker_id: outcome.worker_id.clone(),
        worker_model: Some(outcome.worker_model.clone()),
        model_latency_ms: outcome.model_latency_ms,
        tool_latency_ms: outcome.tool_latency_ms,
        total_latency_ms: outcome.total_latency_ms,
        prompt_tokens: outcome.prompt_tokens,
        completion_tokens: outcome.completion_tokens,
        total_tokens: outcome.total_tokens,
        retries: 0,
    });

    match outcome.status {
        DispatchStatus::Done => {
            orb.result = outcome.response.clone();
            orb.confidence = outcome.confidence;
            if orb.orb_type.uses_phase() {
                orb.set_phase(OrbPhase::Done)?;
            } else {
                orb.set_status(OrbStatus::Done)?;
            }
        }
        DispatchStatus::Failed => {
            orb.result = outcome.error.clone().or_else(|| outcome.response.clone());
            if orb.orb_type.uses_phase() {
                orb.set_phase(OrbPhase::Failed)?;
            } else {
                orb.set_status(OrbStatus::Failed)?;
            }
        }
        DispatchStatus::Cancelled => {
            orb.result = outcome.error.clone().or_else(|| outcome.response.clone());
            if orb.orb_type.uses_phase() {
                orb.set_phase(OrbPhase::Cancelled)?;
            } else {
                orb.set_status(OrbStatus::Cancelled)?;
            }
        }
        DispatchStatus::Aborted => unreachable!("handled above"),
    }
    orb.updated_at = Utc::now();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use orbs::orb::{OrbStatus, OrbType};

    fn active_task_orb() -> Orb {
        let mut o = Orb::new("t", "d").with_type(OrbType::Task);
        // Walk through pending → active so the transition table allows
        // the next move.
        o.set_status(OrbStatus::Active).unwrap();
        o
    }

    fn done_outcome() -> DispatchOutcome {
        let now = Utc::now();
        DispatchOutcome {
            status: DispatchStatus::Done,
            response: Some("the answer".into()),
            confidence: Some(0.82),
            worker_model: "anthropic/claude-sonnet-4-6".into(),
            worker_id: Some("w-1".into()),
            model_latency_ms: Some(150),
            tool_latency_ms: Some(20),
            total_latency_ms: Some(170),
            prompt_tokens: Some(100),
            completion_tokens: Some(50),
            total_tokens: Some(150),
            dispatched_at: now,
            completed_at: now,
            error: None,
        }
    }

    fn failed_outcome() -> DispatchOutcome {
        DispatchOutcome {
            error: Some("worker crashed".into()),
            status: DispatchStatus::Failed,
            response: None,
            ..done_outcome()
        }
    }

    // ── apply_dispatch_outcome — Done ─────────────────────────

    #[test]
    fn apply_done_writes_result_and_transitions_task_to_done() {
        let mut o = active_task_orb();
        apply_dispatch_outcome(&mut o, &done_outcome()).unwrap();
        assert_eq!(o.result.as_deref(), Some("the answer"));
        assert_eq!(o.confidence, Some(0.82));
        assert_eq!(o.status, Some(OrbStatus::Done));
        let exec = o.execution.as_ref().unwrap();
        assert_eq!(
            exec.worker_model.as_deref(),
            Some("anthropic/claude-sonnet-4-6")
        );
        assert_eq!(exec.total_tokens, Some(150));
    }

    #[test]
    fn apply_done_transitions_epic_phase_to_done() {
        let mut o = Orb::new("Epic", "x").with_type(OrbType::Epic);
        // pending phase -> we need to walk through; but for this test
        // we want a non-terminal phase that allows transition to Done.
        // The lifecycle allows Executing → Done.
        o.phase = Some(OrbPhase::Executing);
        apply_dispatch_outcome(&mut o, &done_outcome()).unwrap();
        assert_eq!(o.phase, Some(OrbPhase::Done));
    }

    // ── apply_dispatch_outcome — Failed ───────────────────────

    #[test]
    fn apply_failed_records_error_in_result_and_transitions() {
        let mut o = active_task_orb();
        apply_dispatch_outcome(&mut o, &failed_outcome()).unwrap();
        assert_eq!(o.result.as_deref(), Some("worker crashed"));
        assert_eq!(o.status, Some(OrbStatus::Failed));
        assert!(
            o.confidence.is_none(),
            "failed outcomes should not write confidence"
        );
    }

    // ── apply_dispatch_outcome — Cancelled ────────────────────

    #[test]
    fn apply_cancelled_transitions_to_cancelled() {
        let mut o = active_task_orb();
        let outcome = DispatchOutcome {
            status: DispatchStatus::Cancelled,
            error: Some("user requested".into()),
            ..done_outcome()
        };
        apply_dispatch_outcome(&mut o, &outcome).unwrap();
        assert_eq!(o.status, Some(OrbStatus::Cancelled));
        assert_eq!(o.result.as_deref(), Some("user requested"));
    }

    // ── apply_dispatch_outcome — Aborted ──────────────────────

    #[test]
    fn apply_aborted_is_noop() {
        let mut o = active_task_orb();
        let prior_status = o.status;
        let prior_result = o.result.clone();
        let aborted = DispatchOutcome::aborted("m".into(), "hook said no".into());
        apply_dispatch_outcome(&mut o, &aborted).unwrap();
        assert_eq!(o.status, prior_status, "status unchanged");
        assert_eq!(o.result, prior_result, "result unchanged");
        assert!(o.execution.is_none(), "execution not populated");
    }

    // ── DispatchOutcome::aborted shape ────────────────────────

    #[test]
    fn aborted_outcome_carries_reason_and_model() {
        let o = DispatchOutcome::aborted("m1".into(), "blocked by policy".into());
        assert_eq!(o.status, DispatchStatus::Aborted);
        assert_eq!(o.worker_model, "m1");
        assert_eq!(o.error.as_deref(), Some("blocked by policy"));
        assert!(o.response.is_none());
        assert!(o.confidence.is_none());
    }
}

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
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cost_micros: Option<u64>,
    pub cost_currency: Option<String>,
    pub cached_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub generation_id: Option<String>,
    pub dispatched_at: chrono::DateTime<Utc>,
    pub completed_at: chrono::DateTime<Utc>,
    pub prompt_category: Option<String>,
    pub system_prompt_hash: Option<String>,
    pub system_prompt_source: Option<String>,
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
    fields(
        orb = %orb.id,
        title = %orb.title,
        orb_type = ?orb.orb_type,
        phase = ?orb.phase,
        model = %worker_config.model
    )
)]
pub async fn dispatch_orb(
    orb: &Orb,
    prompt: &str,
    worker_config: &WorkerConfig,
    hooks: Option<&HookSink>,
) -> anyhow::Result<DispatchOutcome> {
    // 1. pre-worker-spawn — gating. Exit 2 from any matching hook
    //    short-circuits before we spawn anything. We're already in
    //    async context, so use `fire` directly (not `fire_blocking`,
    //    which would try to nest a runtime).
    if let Some(sink) = hooks {
        let (outcome, _invocations) = sink
            .fire(HookEvent::PreWorkerSpawn, FireCtx::for_orb(orb))
            .await;
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
    info!(
        orb = %orb.id,
        title = %orb.title,
        orb_type = ?orb.orb_type,
        phase = ?orb.phase,
        model = %worker_config.model,
        "dispatch_orb start",
    );

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
        title = %orb.title,
        orb_type = ?orb.orb_type,
        phase = ?orb.phase,
        status = ?outcome.status,
        elapsed_ms,
        prompt_tokens = ?outcome.prompt_tokens,
        completion_tokens = ?outcome.completion_tokens,
        total_tokens = ?outcome.total_tokens,
        cost_micros = ?outcome.cost_micros,
        cost_currency = ?outcome.cost_currency.as_deref(),
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
        // Best-effort — never propagate a post-hook outcome. We're
        // already in async context.
        let _ = sink.fire(event, FireCtx::for_orb(orb)).await;
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
        cost_micros: send.usage.as_ref().and_then(|u| u.cost_micros),
        cost_currency: send.usage.as_ref().and_then(|u| u.cost_currency.clone()),
        cached_tokens: send.usage.as_ref().and_then(|u| u.cached_tokens),
        cache_write_tokens: send.usage.as_ref().and_then(|u| u.cache_write_tokens),
        reasoning_tokens: send.usage.as_ref().and_then(|u| u.reasoning_tokens),
        generation_id: send.usage.as_ref().and_then(|u| u.generation_id.clone()),
        dispatched_at,
        completed_at,
        prompt_category: None,
        system_prompt_hash: None,
        system_prompt_source: None,
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
        cost_micros: None,
        cost_currency: None,
        cached_tokens: None,
        cache_write_tokens: None,
        reasoning_tokens: None,
        generation_id: None,
        dispatched_at,
        completed_at,
        prompt_category: None,
        system_prompt_hash: None,
        system_prompt_source: None,
        error: Some(message),
    }
}

/// Attaches prompt identity to a dispatch outcome before it is
/// applied to the orb's execution metadata.
#[must_use]
pub fn with_prompt_metadata(
    mut outcome: DispatchOutcome,
    category: impl Into<String>,
    system_prompt: &str,
    source: impl Into<String>,
) -> DispatchOutcome {
    outcome.prompt_category = Some(category.into());
    outcome.system_prompt_hash = Some(crate::prompt::prompt_hash(system_prompt));
    outcome.system_prompt_source = Some(source.into());
    outcome
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
///
/// # Errors
///
/// Returns [`orbs::orb::TransitionError`] when the target status or
/// phase transition is invalid for the orb's current lifecycle state.
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
        cost_micros: outcome.cost_micros,
        cost_currency: outcome.cost_currency.clone(),
        cached_tokens: outcome.cached_tokens,
        cache_write_tokens: outcome.cache_write_tokens,
        reasoning_tokens: outcome.reasoning_tokens,
        generation_id: outcome.generation_id.clone(),
        prompt_category: outcome.prompt_category.clone(),
        system_prompt_hash: outcome.system_prompt_hash.clone(),
        system_prompt_source: outcome.system_prompt_source.clone(),
        retries: 0,
    });

    match outcome.status {
        DispatchStatus::Done => {
            orb.result.clone_from(&outcome.response);
            orb.confidence = outcome.confidence;
            if orb.orb_type.uses_phase() {
                let next = next_phase_on_dispatch_success(orb.phase);
                orb.set_phase(next)?;
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

/// Returns the next phase a phase-orb should transition to when its
/// dispatch worker completes successfully. Phase orbs advance
/// through the pipeline (Speccing → Decomposing → ...) rather than
/// jumping straight to Done — only the `Executing` worker actually
/// finishes the orb.
fn next_phase_on_dispatch_success(current: Option<OrbPhase>) -> OrbPhase {
    match current {
        Some(OrbPhase::Speccing) => OrbPhase::Decomposing,
        Some(OrbPhase::Decomposing) => OrbPhase::Refining,
        Some(OrbPhase::Refining) => OrbPhase::Review,
        Some(OrbPhase::Reevaluating) => OrbPhase::Executing,
        // Executing and unexpected phases fall through to Done. The transition
        // table will reject if the move isn't valid, which surfaces
        // a bug rather than silently mis-advancing.
        _ => OrbPhase::Done,
    }
}

/// Builds a base `WorkerConfig` from the layered project config.
/// Reads `OrbConfig.worker_binary` and `OrbConfig.default_model`.
/// Returns an error if the config doesn't specify a worker binary
/// — there's no sensible fallback.
///
/// # Errors
///
/// Returns an error if config load fails or `worker_binary` is
/// unset.
pub fn default_worker_config(
    home: Option<&std::path::Path>,
    project_dir: Option<&std::path::Path>,
) -> anyhow::Result<WorkerConfig> {
    let cfg = crate::config::load_config_with_home(home, project_dir)?;
    let binary = cfg
        .worker_binary
        .ok_or_else(|| anyhow::anyhow!("worker_binary is unset in OrbConfig"))?;
    Ok(WorkerConfig {
        command: binary,
        args: vec![],
        cwd: None,
        env: vec![],
        model: cfg.default_model,
        system_prompt: String::new(),
        tools: vec![],
        max_iterations: None,
        init_timeout: None,
        send_timeout: None,
        shutdown_timeout: None,
        task_id: None,
        worker_id: None,
    })
}

/// Overlays orb-specific fields onto a base `WorkerConfig`:
///   - `orb.preferred_model` overrides the base model when set.
///   - `task_id` is set to the orb's id (heddle uses this for tracing).
///   - `worker_id` is freshly generated for this dispatch.
///   - `system_prompt` becomes the caller-supplied prompt plus the
///     [`crate::worker::process::CONFIDENCE_PROMPT_ADDENDUM`].
#[must_use]
pub fn worker_config_for(orb: &Orb, base: &WorkerConfig, system_prompt: &str) -> WorkerConfig {
    let mut wc = base.clone();
    if let Some(ref model) = orb.preferred_model {
        wc.model.clone_from(model);
    }
    wc.task_id = Some(orb.id.to_string());
    wc.worker_id = Some(uuid::Uuid::new_v4().to_string());
    wc.system_prompt = format!(
        "{system_prompt}{}",
        crate::worker::process::CONFIDENCE_PROMPT_ADDENDUM
    );
    wc
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
            cost_micros: Some(12_345),
            cost_currency: Some("USD".into()),
            cached_tokens: Some(20),
            cache_write_tokens: Some(5),
            reasoning_tokens: Some(7),
            generation_id: Some("gen-123".into()),
            dispatched_at: now,
            completed_at: now,
            prompt_category: Some("worker.execute".into()),
            system_prompt_hash: Some("abc123".into()),
            system_prompt_source: Some("built_in".into()),
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
        assert_eq!(exec.cost_micros, Some(12_345));
        assert_eq!(exec.cost_currency.as_deref(), Some("USD"));
        assert_eq!(exec.cached_tokens, Some(20));
        assert_eq!(exec.cache_write_tokens, Some(5));
        assert_eq!(exec.reasoning_tokens, Some(7));
        assert_eq!(exec.generation_id.as_deref(), Some("gen-123"));
        assert_eq!(exec.prompt_category.as_deref(), Some("worker.execute"));
        assert_eq!(exec.system_prompt_hash.as_deref(), Some("abc123"));
        assert_eq!(exec.system_prompt_source.as_deref(), Some("built_in"));
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

    // ── worker_config_for + default_worker_config ─────────────

    fn base_wc() -> WorkerConfig {
        WorkerConfig {
            command: "heddle".into(),
            args: vec![],
            cwd: None,
            env: vec![],
            model: "default/m".into(),
            system_prompt: String::new(),
            tools: vec![],
            max_iterations: None,
            init_timeout: None,
            send_timeout: None,
            shutdown_timeout: None,
            task_id: None,
            worker_id: None,
        }
    }

    #[test]
    fn worker_config_for_uses_base_model_when_orb_has_no_preference() {
        let orb = active_task_orb();
        let wc = worker_config_for(&orb, &base_wc(), "you are a worker");
        assert_eq!(wc.model, "default/m");
        assert_eq!(wc.task_id.as_deref(), Some(orb.id.as_str()));
        assert!(wc.worker_id.is_some(), "worker_id is freshly generated");
        assert!(wc.system_prompt.starts_with("you are a worker"));
    }

    #[test]
    fn worker_config_for_prefers_orb_model_over_base() {
        let mut orb = active_task_orb();
        orb.preferred_model = Some("orb/model".into());
        let wc = worker_config_for(&orb, &base_wc(), "x");
        assert_eq!(wc.model, "orb/model");
    }

    #[test]
    fn worker_config_for_appends_confidence_addendum() {
        let orb = active_task_orb();
        let wc = worker_config_for(&orb, &base_wc(), "you are a worker");
        assert!(
            wc.system_prompt.contains("CONFIDENCE:"),
            "addendum should be appended: {}",
            wc.system_prompt
        );
    }

    #[test]
    fn worker_config_for_generates_unique_worker_ids_per_call() {
        let orb = active_task_orb();
        let a = worker_config_for(&orb, &base_wc(), "x");
        let b = worker_config_for(&orb, &base_wc(), "x");
        assert_ne!(a.worker_id, b.worker_id);
    }

    #[test]
    fn default_worker_config_errors_when_worker_binary_unset() {
        let home = tempfile::tempdir().unwrap();
        // No config file → default OrbConfig → worker_binary is None.
        let err = default_worker_config(Some(home.path()), None).unwrap_err();
        assert!(
            err.to_string().contains("worker_binary"),
            "expected error to mention worker_binary: {err}"
        );
    }

    #[test]
    fn default_worker_config_reads_binary_and_default_model_from_config() {
        let home = tempfile::tempdir().unwrap();
        let cfg_dir = home.path().join(".orboros");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("config.toml"),
            r#"
default_model = "anthropic/test"
worker_binary = "/usr/local/bin/heddle"
"#,
        )
        .unwrap();
        let wc = default_worker_config(Some(home.path()), None).unwrap();
        assert_eq!(wc.command, "/usr/local/bin/heddle");
        assert_eq!(wc.model, "anthropic/test");
    }
}

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use std::collections::HashMap;

use orbs::dep_store::DepStore;
use orbs::id::OrbId;
use orbs::orb::{Orb, OrbPhase, OrbStatus};
use orbs::orb_store::OrbStore;
use orbs::pipeline::create_pipeline;
use orbs::task::TaskStatus;
use tracing::{debug, instrument};

const MAX_PARTIAL_ARTIFACT_RECOVERY_ITERATIONS: u32 = 8;
const PARTIAL_ARTIFACT_RECOVERY_SYSTEM_PROMPT: &str =
    include_str!("../assets/prompts/recover-partial-artifact.md");
const COMPLETED_DISPATCH_RETRY_INITIAL: Duration = Duration::from_millis(250);
const COMPLETED_DISPATCH_RETRY_MAX: Duration = Duration::from_secs(30);

fn completed_dispatch_retry_allowed(configured_retries: i32, retries_completed: u32) -> bool {
    configured_retries == -1
        || u32::try_from(configured_retries).is_ok_and(|limit| retries_completed < limit)
}

fn completed_dispatch_retry_backoff(retries_completed: u32) -> Duration {
    COMPLETED_DISPATCH_RETRY_INITIAL
        .checked_mul(1_u32 << retries_completed.min(7))
        .unwrap_or(COMPLETED_DISPATCH_RETRY_MAX)
        .min(COMPLETED_DISPATCH_RETRY_MAX)
}

/// Determines the last completed refinement round visible from an orb
/// checkpoint. Restored snapshots retain their original timestamp, which
/// makes the append-only execution ledger a bounded, history-aware view.
fn completed_refinement_round_at_checkpoint(
    records: &[crate::execution::ExecutionRecord],
    orb_id: &str,
    checkpoint: chrono::DateTime<chrono::Utc>,
) -> u32 {
    records
        .iter()
        .filter(|record| record.orb_id == orb_id && record.completed_at <= checkpoint)
        .filter_map(|record| record.refinement_round.as_ref())
        .map(|diagnostic| {
            // A failed round was never applied. Re-run that ordinal rather
            // than treating the diagnostic as completed progress.
            if diagnostic.termination_reason.as_deref() == Some("worker_failed") {
                diagnostic.round.saturating_sub(1)
            } else {
                diagnostic.round
            }
        })
        .max()
        .unwrap_or(0)
}

/// Result of a single tick of the queue loop.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TickResult {
    /// Number of new pipelines started (pipeline-phase orbs detected).
    pub pipelines_started: u32,
    /// Number of orbs moved to active/executing.
    pub orbs_executed: u32,
    /// Number of root orbs completed (all children done).
    pub roots_completed: u32,
    /// Number of waiting orbs sent for re-evaluation.
    pub orbs_reevaluated: u32,
}

impl TickResult {
    /// Returns true if no actions were taken this tick.
    pub fn is_idle(&self) -> bool {
        self.pipelines_started == 0
            && self.orbs_executed == 0
            && self.roots_completed == 0
            && self.orbs_reevaluated == 0
    }
}

/// Result of running the queue in the foreground for a target orb.
#[derive(Debug, Clone)]
pub struct DrainResult {
    /// Target orb id.
    pub target_id: OrbId,
    /// Number of queue cycles performed.
    pub cycles: u32,
    /// Number of workers that completed successfully during the drain.
    pub workers_completed: u32,
    /// The target orb, if it still exists.
    pub target: Option<Orb>,
    /// Why the foreground loop stopped.
    pub reason: DrainStopReason,
}

/// Reason a foreground queue drain stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainStopReason {
    /// The target orb reached Done, Failed, Cancelled, or Tombstone.
    TargetTerminal,
    /// One queue/dispatch cycle completed and the caller did not ask to wait.
    SingleCycle,
    /// The queue became idle before the target reached a terminal state.
    Idle,
    /// The configured maximum cycle count was reached.
    MaxCycles,
    /// The target orb no longer exists in the store.
    MissingTarget,
}

impl DrainResult {
    /// Returns true when the target orb reached a terminal state.
    #[must_use]
    pub fn target_terminal(&self) -> bool {
        self.reason == DrainStopReason::TargetTerminal
    }
}

/// Main daemon loop that drives the orb pipeline.
///
/// Polls stores for work and advances orbs through their lifecycle.
#[derive(Clone)]
pub struct QueueLoop {
    orb_store: OrbStore,
    dep_store: DepStore,
    base_dir: PathBuf,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    hooks: Option<Arc<crate::hooks::HookSink>>,
    review_config: Option<crate::config::ReviewConfig>,
    config_override: Option<crate::config::OrbConfig>,
    prompt_config: Option<crate::config::PromptConfig>,
    tool_policy: Option<crate::routing::profile::PhaseToolPolicy>,
    execution_store: crate::execution::ExecutionStore,
    prompt_store: Option<crate::execution::PromptStore>,
    worker_evidence_dir: PathBuf,
    project_key: Option<String>,
}

impl QueueLoop {
    /// Creates a new `QueueLoop`.
    pub fn new(orb_store: OrbStore, dep_store: DepStore, base_dir: PathBuf) -> Self {
        let execution_path = orb_store
            .path()
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("executions.jsonl");
        let worker_evidence_dir = base_dir.join("logs").join("heddle");
        Self {
            orb_store,
            dep_store,
            base_dir,
            running: Arc::new(AtomicBool::new(true)),
            paused: Arc::new(AtomicBool::new(false)),
            hooks: None,
            review_config: None,
            config_override: None,
            prompt_config: None,
            tool_policy: None,
            execution_store: crate::execution::ExecutionStore::new(execution_path),
            prompt_store: None,
            worker_evidence_dir,
            project_key: None,
        }
    }

    /// Overrides the directory where isolated worker transcripts and runtime
    /// state are written. Registered supervisor projects use their shared
    /// project home so evidence is stable across worktrees.
    #[must_use]
    pub fn with_worker_evidence_dir(mut self, worker_evidence_dir: PathBuf) -> Self {
        self.worker_evidence_dir = worker_evidence_dir;
        self
    }
    #[must_use]
    pub fn with_project_key(mut self, project_key: impl Into<String>) -> Self {
        self.project_key = Some(project_key.into());
        self
    }

    /// Enables durable resolved-prompt capture for an isolated embedded run.
    ///
    /// Benchmark runners opt in so their snapshots survive artifact pruning.
    /// Normal project runs retain compact execution telemetry but do not save
    /// full prompt text by default.
    #[must_use]
    pub fn with_prompt_capture(mut self) -> Self {
        let prompt_path = self
            .orb_store
            .path()
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("prompts.jsonl");
        self.prompt_store = Some(crate::execution::PromptStore::new(prompt_path));
        self
    }

    /// Overrides review behavior for an embedded queue run, such as a
    /// benchmark. Normal daemon callers use the project config loaded from
    /// the queue base directory.
    #[must_use]
    pub fn with_review_config(mut self, review_config: crate::config::ReviewConfig) -> Self {
        self.review_config = Some(review_config);
        self
    }

    /// Uses an already-resolved configuration for an embedded run. This keeps
    /// benchmark phase model selection independent of a fixture's/global config.
    #[must_use]
    pub fn with_config(mut self, config: crate::config::OrbConfig) -> Self {
        self.config_override = Some(config);
        self
    }

    /// Overrides prompt configuration for an embedded run, such as a
    /// benchmark prompt-set experiment.
    #[must_use]
    pub fn with_prompt_config(mut self, prompt_config: crate::config::PromptConfig) -> Self {
        self.prompt_config = Some(prompt_config);
        self
    }

    /// Overrides phase tool policy for an embedded run, such as a benchmark
    /// case. The dispatcher's base worker configuration remains a hard ceiling.
    #[must_use]
    pub fn with_tool_policy(
        mut self,
        tool_policy: crate::routing::profile::PhaseToolPolicy,
    ) -> Self {
        self.tool_policy = Some(tool_policy);
        self
    }

    /// Attaches a `HookSink` so the queue fires `on-queue-tick` after
    /// each non-paused tick.
    #[must_use]
    pub fn with_hooks(mut self, hooks: crate::hooks::HookSink) -> Self {
        self.hooks = Some(Arc::new(hooks));
        self
    }

    /// Pauses the loop. While paused, `tick()` returns immediately with zero counts.
    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    /// Resumes the loop after a pause.
    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
    }

    /// Returns true if the loop is currently paused.
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// Signals the loop to stop.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    /// Returns a clone of the running flag for external monitoring.
    pub fn running_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.running)
    }

    #[must_use]
    pub fn execution_store(&self) -> crate::execution::ExecutionStore {
        self.execution_store.clone()
    }

    /// Performs a single iteration of the queue loop.
    ///
    /// 1. Detects pipeline-phase orbs (Pending epics/features) and creates pipeline dirs.
    /// 2. Detects ready orbs (unblocked) and marks them as Active/Executing.
    /// 3. Detects root orb completion (all children Done).
    /// 4. Detects waiting orbs and triggers re-evaluation.
    ///
    /// # Errors
    ///
    /// Returns an IO error if store operations fail.
    #[instrument(name = "queue.tick", level = "debug", skip(self), fields(orb_count = tracing::field::Empty))]
    pub fn tick(&self) -> std::io::Result<TickResult> {
        if self.paused.load(Ordering::SeqCst) {
            debug!("queue paused; skipping tick");
            return Ok(TickResult::default());
        }

        let mut result = TickResult::default();
        let all_orbs = self.orb_store.load_all()?;
        tracing::Span::current().record("orb_count", all_orbs.len());

        // 1. Pipeline-phase orbs: Pending epics/features need pipeline dirs + speccing
        result.pipelines_started = self.start_pipelines(&all_orbs)?;

        // 2. Ready orbs: unblocked non-terminal orbs → mark as Active/Executing
        result.orbs_executed = self.execute_ready(&all_orbs)?;

        // 3. Root completion: root orbs whose children are all Done
        result.roots_completed = self.complete_roots(&all_orbs)?;

        // 4. Waiting orbs: blocked orbs → trigger re-evaluation
        result.orbs_reevaluated = self.reevaluate_waiting(&all_orbs)?;

        Ok(result)
    }

    /// Async counterpart to `tick()` that fires `pre-phase-transition`
    /// and `post-phase-transition` hooks around each phase change.
    /// Pre-hook exit 2 short-circuits the individual transition; the
    /// rest of the tick continues.
    ///
    /// Status-only transitions (e.g. task Pending→Active) don't fire
    /// phase hooks — no event variant exists for them.
    ///
    /// # Errors
    ///
    /// Returns an IO error if store operations fail.
    pub async fn tick_async(&self) -> std::io::Result<TickResult> {
        if self.paused.load(Ordering::SeqCst) {
            return Ok(TickResult::default());
        }

        let mut result = TickResult::default();
        let all_orbs = self.orb_store.load_all()?;

        result.pipelines_started = self.start_pipelines_with_hooks(&all_orbs).await?;
        result.orbs_executed = self.execute_ready_with_hooks(&all_orbs).await?;
        result.roots_completed = self.complete_roots_with_hooks(&all_orbs).await?;
        result.orbs_reevaluated = self.reevaluate_waiting_with_hooks(&all_orbs).await?;

        Ok(result)
    }

    /// Applies a phase transition with `pre-phase-transition` (gating)
    /// and `post-phase-transition` (informational) hooks fired around
    /// it. Returns `Ok(true)` when the transition completed, `Ok(false)`
    /// when a pre-hook aborted it.
    async fn try_phase_transition(&self, orb: &Orb, target: OrbPhase) -> std::io::Result<bool> {
        use crate::hooks::{FireCtx, FireOutcome, HookEvent};

        if let Some(sink) = &self.hooks {
            let (outcome, _) = sink
                .fire(HookEvent::PrePhaseTransition(target), FireCtx::for_orb(orb))
                .await;
            if let FireOutcome::Aborted {
                hook_name,
                exit_code,
            } = outcome
            {
                tracing::warn!(
                    orb = %orb.id,
                    hook = %hook_name,
                    exit_code,
                    target = ?target,
                    "pre-phase-transition hook aborted",
                );
                return Ok(false);
            }
        }
        let mut updated = orb.clone();
        if matches!(target, OrbPhase::Executing | OrbPhase::ExecutingChildren) {
            updated.execution = None;
        }
        updated.set_phase(target).map_err(std::io::Error::other)?;
        self.orb_store.update(&updated)?;
        if let Some(sink) = &self.hooks {
            let _ = sink
                .fire(
                    HookEvent::PostPhaseTransition(target),
                    FireCtx::for_orb(&updated),
                )
                .await;
        }
        Ok(true)
    }

    /// Hook-aware version of `start_pipelines`. Same control flow but
    /// fires pre/post-phase-transition for each Pending→Speccing move.
    async fn start_pipelines_with_hooks(&self, orbs: &[Orb]) -> std::io::Result<u32> {
        let mut count = 0;
        for orb in orbs {
            if !orb.orb_type.uses_phase() || orb.phase != Some(OrbPhase::Pending) {
                continue;
            }
            create_pipeline(&self.base_dir, orb)?;
            if self.try_phase_transition(orb, OrbPhase::Speccing).await? {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Hook-aware version of `execute_ready`. Fires phase hooks only
    /// for the phase-orb branch (Waiting → ExecutingChildren/Executing); the task-orb
    /// status transition uses the un-hooked path.
    async fn execute_ready_with_hooks(&self, orbs: &[Orb]) -> std::io::Result<u32> {
        let ready_ids = self
            .dep_store
            .ready(orbs)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let mut count = 0;
        for orb in orbs {
            if !ready_ids.contains(&orb.id) {
                continue;
            }
            if blocked_by_parent_review(orb, orbs) {
                continue;
            }
            if orb.orb_type.uses_phase() {
                if orb.phase != Some(OrbPhase::Waiting) {
                    continue;
                }
                let target = if has_children(orb, orbs) {
                    OrbPhase::ExecutingChildren
                } else {
                    OrbPhase::Executing
                };
                if self.try_phase_transition(orb, target).await? {
                    count += 1;
                }
            } else if orb.status == Some(OrbStatus::Pending) {
                let mut updated = orb.clone();
                updated
                    .set_status(OrbStatus::Active)
                    .map_err(std::io::Error::other)?;
                self.orb_store.update(&updated)?;
                count += 1;
            }
        }
        Ok(count)
    }

    /// Hook-aware version of `complete_roots`. Fires phase hooks for
    /// phase-orb root completions; task roots use the un-hooked
    /// status-transition path.
    async fn complete_roots_with_hooks(&self, orbs: &[Orb]) -> std::io::Result<u32> {
        let children_by_parent = index_children_by_parent(orbs);
        let mut count = 0;
        for orb in orbs {
            if orb.effective_status() == TaskStatus::Done
                || orb.effective_status() == TaskStatus::Failed
                || orb.effective_status() == TaskStatus::Cancelled
            {
                continue;
            }
            let Some(children) = children_by_parent.get(&orb.id) else {
                continue;
            };
            if let Some(failed_child) = children
                .iter()
                .find(|child| child.effective_status() == TaskStatus::Failed)
            {
                let mut updated = orb.clone();
                let reason = failed_child.result.as_deref().unwrap_or("child orb failed");
                updated.result = Some(format!(
                    "required child {} failed: {reason}",
                    failed_child.id
                ));
                if orb.orb_type.uses_phase() {
                    updated
                        .set_phase(OrbPhase::Failed)
                        .map_err(std::io::Error::other)?;
                } else {
                    updated
                        .set_status(OrbStatus::Failed)
                        .map_err(std::io::Error::other)?;
                }
                self.orb_store.update(&updated)?;
                count += 1;
                continue;
            }
            let all_children_done = children
                .iter()
                .all(|c| c.effective_status() == TaskStatus::Done);
            if !all_children_done {
                continue;
            }
            if orb.orb_type.uses_phase() {
                let mut phase = orb.phase;
                if phase == Some(OrbPhase::Waiting) {
                    if !self
                        .try_phase_transition(orb, OrbPhase::ExecutingChildren)
                        .await?
                    {
                        continue;
                    }
                    phase = Some(OrbPhase::ExecutingChildren);
                }
                if phase == Some(OrbPhase::ExecutingChildren) {
                    let target = if orb.has_parent_final_work {
                        OrbPhase::Executing
                    } else {
                        OrbPhase::Done
                    };
                    let mut transitioned = orb.clone();
                    transitioned.phase = phase;
                    if self.try_phase_transition(&transitioned, target).await? {
                        count += 1;
                    }
                }
            } else {
                let mut updated = orb.clone();
                updated
                    .set_status(OrbStatus::Done)
                    .map_err(std::io::Error::other)?;
                self.orb_store.update(&updated)?;
                count += 1;
            }
        }
        Ok(count)
    }

    /// Hook-aware version of `reevaluate_waiting`.
    async fn reevaluate_waiting_with_hooks(&self, orbs: &[Orb]) -> std::io::Result<u32> {
        let waiting_ids = self
            .dep_store
            .waiting(orbs)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let mut count = 0;
        for orb in orbs {
            if !waiting_ids.contains(&orb.id) {
                continue;
            }
            if orb.orb_type.uses_phase()
                && orb.phase == Some(OrbPhase::Waiting)
                && self
                    .try_phase_transition(orb, OrbPhase::Reevaluating)
                    .await?
            {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Detects Pending pipeline-phase orbs and creates pipeline directories.
    fn start_pipelines(&self, orbs: &[Orb]) -> std::io::Result<u32> {
        let mut count = 0;
        for orb in orbs {
            if !orb.orb_type.uses_phase() {
                continue;
            }
            if orb.phase != Some(OrbPhase::Pending) {
                continue;
            }

            // Create the pipeline directory
            create_pipeline(&self.base_dir, orb)?;

            // Transition to Speccing
            let mut updated = orb.clone();
            updated
                .set_phase(OrbPhase::Speccing)
                .map_err(std::io::Error::other)?;
            self.orb_store.update(&updated)?;
            count += 1;
        }
        Ok(count)
    }

    /// Marks ready (unblocked) Pending task-type orbs as Active.
    fn execute_ready(&self, orbs: &[Orb]) -> std::io::Result<u32> {
        let ready_ids = self
            .dep_store
            .ready(orbs)
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let mut count = 0;
        for orb in orbs {
            if !ready_ids.contains(&orb.id) {
                continue;
            }
            if blocked_by_parent_review(orb, orbs) {
                continue;
            }

            // Only advance Pending task-type orbs to Active
            if orb.orb_type.uses_phase() {
                // Phase-type orbs in Waiting → Executing
                if orb.phase == Some(OrbPhase::Waiting) {
                    let mut updated = orb.clone();
                    updated.execution = None;
                    let target = if has_children(orb, orbs) {
                        OrbPhase::ExecutingChildren
                    } else {
                        OrbPhase::Executing
                    };
                    updated.set_phase(target).map_err(std::io::Error::other)?;
                    self.orb_store.update(&updated)?;
                    count += 1;
                }
            } else {
                // Task-type orbs in Pending → Active
                if orb.status == Some(OrbStatus::Pending) {
                    let mut updated = orb.clone();
                    updated
                        .set_status(OrbStatus::Active)
                        .map_err(std::io::Error::other)?;
                    self.orb_store.update(&updated)?;
                    count += 1;
                }
            }
        }
        Ok(count)
    }

    /// Detects root orbs whose children are all Done and marks them Done.
    fn complete_roots(&self, orbs: &[Orb]) -> std::io::Result<u32> {
        let children_by_parent = index_children_by_parent(orbs);
        let mut count = 0;

        for orb in orbs {
            if orb.effective_status() == TaskStatus::Done
                || orb.effective_status() == TaskStatus::Failed
                || orb.effective_status() == TaskStatus::Cancelled
            {
                continue;
            }

            let Some(children) = children_by_parent.get(&orb.id) else {
                continue;
            };

            if let Some(failed_child) = children
                .iter()
                .find(|child| child.effective_status() == TaskStatus::Failed)
            {
                let mut updated = orb.clone();
                let reason = failed_child.result.as_deref().unwrap_or("child orb failed");
                updated.result = Some(format!(
                    "required child {} failed: {reason}",
                    failed_child.id
                ));
                if orb.orb_type.uses_phase() {
                    updated
                        .set_phase(OrbPhase::Failed)
                        .map_err(std::io::Error::other)?;
                } else {
                    updated
                        .set_status(OrbStatus::Failed)
                        .map_err(std::io::Error::other)?;
                }
                self.orb_store.update(&updated)?;
                count += 1;
                continue;
            }

            let all_children_done = children
                .iter()
                .all(|c| c.effective_status() == TaskStatus::Done);

            if all_children_done && orb.orb_type.uses_phase() {
                let mut updated = orb.clone();
                if updated.phase == Some(OrbPhase::Waiting) {
                    updated.execution = None;
                    updated
                        .set_phase(OrbPhase::ExecutingChildren)
                        .map_err(std::io::Error::other)?;
                }
                if updated.phase == Some(OrbPhase::ExecutingChildren) {
                    let target = if updated.has_parent_final_work {
                        OrbPhase::Executing
                    } else {
                        OrbPhase::Done
                    };
                    updated.set_phase(target).map_err(std::io::Error::other)?;
                    self.orb_store.update(&updated)?;
                    count += 1;
                }
            } else if all_children_done {
                let mut updated = orb.clone();
                updated
                    .set_status(OrbStatus::Done)
                    .map_err(std::io::Error::other)?;
                self.orb_store.update(&updated)?;
                count += 1;
            }
        }
        Ok(count)
    }

    /// Detects waiting orbs and marks them for re-evaluation.
    fn reevaluate_waiting(&self, orbs: &[Orb]) -> std::io::Result<u32> {
        let waiting_ids = self
            .dep_store
            .waiting(orbs)
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let mut count = 0;
        for orb in orbs {
            if !waiting_ids.contains(&orb.id) {
                continue;
            }

            // Only re-evaluate phase orbs in Waiting
            if orb.orb_type.uses_phase() && orb.phase == Some(OrbPhase::Waiting) {
                let mut updated = orb.clone();
                updated
                    .set_phase(OrbPhase::Reevaluating)
                    .map_err(std::io::Error::other)?;
                self.orb_store.update(&updated)?;
                count += 1;
            }
        }
        Ok(count)
    }

    /// Runs the queue loop until stopped.
    ///
    /// Calls `tick()` in a loop with a short sleep between iterations,
    /// checking the `running` flag each time. After each non-paused
    /// tick, fires the `on-queue-tick` hook (if a `HookSink` is
    /// attached and any hooks match — the matcher rejects orb-bound
    /// rules when no orb is in context).
    ///
    /// # Errors
    ///
    /// Returns an IO error if any tick fails.
    pub async fn run(&self) -> std::io::Result<()> {
        while self.running.load(Ordering::SeqCst) {
            let result = self.tick()?;
            if !self.is_paused() {
                if let Some(sink) = &self.hooks {
                    let ctx = crate::hooks::FireCtx::default();
                    let (_outcome, _invs) =
                        sink.fire(crate::hooks::HookEvent::OnQueueTick, ctx).await;
                    // tick hooks are best-effort — never gate the next tick.
                }
            }
            let _ = result;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        Ok(())
    }

    /// Fires the `on-queue-tick` hook with no orb context.
    /// Best-effort — never returns an error. The matcher rejects
    /// orb-bound rules when no orb is in context, so the daemon
    /// can call this unconditionally after every tick.
    pub async fn fire_on_queue_tick(&self) {
        if self.is_paused() {
            return;
        }
        if let Some(sink) = &self.hooks {
            let ctx = crate::hooks::FireCtx::default();
            let (_outcome, _invs) = sink.fire(crate::hooks::HookEvent::OnQueueTick, ctx).await;
        }
    }

    /// Dispatches every ready orb in parallel, bounded by
    /// `max_concurrency`. Ready orbs are those whose status/phase
    /// puts them in a worker-eligible state AND that haven't been
    /// dispatched yet (i.e. `execution` is None).
    ///
    /// Returns the number of orbs that completed dispatch
    /// successfully (status moved to Done). Failures don't fail the
    /// whole tick — they're persisted on the orb and counted only
    /// in `eprintln!`/tracing output.
    ///
    /// # Errors
    ///
    /// Returns an IO error if the store can't be read at the top.
    /// Individual worker / per-orb errors are captured per-orb.
    #[instrument(name = "queue.dispatch_ready", level = "debug", skip(self, base_worker_config), fields(model = %base_worker_config.model))]
    pub async fn dispatch_ready_orbs(
        &self,
        base_worker_config: &crate::worker::process::WorkerConfig,
        max_concurrency: usize,
    ) -> std::io::Result<u32> {
        self.dispatch_ready_orbs_with_global(base_worker_config, max_concurrency, None)
            .await
    }

    /// Dispatches ready work subject to the project-local limit and an
    /// optional supervisor-wide permit pool.
    pub async fn dispatch_ready_orbs_with_global(
        &self,
        base_worker_config: &crate::worker::process::WorkerConfig,
        max_concurrency: usize,
        global_semaphore: Option<Arc<tokio::sync::Semaphore>>,
    ) -> std::io::Result<u32> {
        use tokio::sync::Semaphore;
        use tokio::task::JoinSet;

        let all_orbs = self.orb_store.load_all()?;
        let all_edges = self
            .dep_store
            .all_edges()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let mut targets: Vec<(Orb, DispatchTarget)> = Vec::new();
        for orb in &all_orbs {
            if !blocked_by_parent_review(orb, &all_orbs) {
                if let Some(t) = dispatch_target_for(orb) {
                    targets.push((orb.clone(), t));
                }
            }
        }
        if targets.is_empty() {
            return Ok(0);
        }

        let mut orb_config = match &self.config_override {
            Some(config) => config.clone(),
            None => {
                crate::config::load_config(Some(&self.base_dir)).map_err(std::io::Error::other)?
            }
        };
        if let Some(review_config) = &self.review_config {
            orb_config.review = review_config.clone();
        }
        let prompt_config = self
            .prompt_config
            .clone()
            .unwrap_or_else(|| orb_config.prompts.clone());
        let prompt_config = load_external_prompt_set(prompt_config, &self.base_dir)?;
        let prompt_resolver =
            crate::prompt::PromptResolver::from_config(prompt_config, Some(&self.base_dir));

        let semaphore = Arc::new(Semaphore::new(max_concurrency.max(1)));
        // `stop()` is a graceful admission barrier: workers that have already
        // started are allowed to finish and persist their outcome, while work
        // still waiting for a permit must not begin during shutdown.
        let running = self.running_flag();
        let context_orbs = Arc::new(all_orbs);
        let context_edges = Arc::new(all_edges);
        let mut join_set = JoinSet::new();

        for (orb, target) in targets {
            let sem = semaphore.clone();
            let global_semaphore = global_semaphore.clone();
            let store = self.orb_store.clone();
            let dep_store = self.dep_store.clone();
            let base_wc = base_worker_config.clone();
            let orb_config = orb_config.clone();
            let prompt_resolver = prompt_resolver.clone();
            let tool_policy = self.tool_policy.clone();
            let context_orbs = Arc::clone(&context_orbs);
            let context_edges = Arc::clone(&context_edges);
            let hooks = self.hooks.as_ref().map(Arc::clone);
            let execution_store = self.execution_store.clone();
            let prompt_store = self.prompt_store.clone();
            let worker_evidence_dir = self.worker_evidence_dir.clone();
            let project_key = self.project_key.clone();
            let running = Arc::clone(&running);
            join_set.spawn(async move {
                if !running.load(Ordering::SeqCst) {
                    return Ok(false);
                }
                let Ok(_permit) = sem.acquire_owned().await else {
                    return Ok(false);
                };
                let _global_permit = match global_semaphore {
                    Some(semaphore) => match semaphore.acquire_owned().await {
                        Ok(permit) => Some(permit),
                        Err(_) => return Ok(false),
                    },
                    None => None,
                };
                if !running.load(Ordering::SeqCst) {
                    return Ok(false);
                }
                let context = DispatchContext {
                    orbs: &context_orbs,
                    edges: &context_edges,
                };
                dispatch_one_owned(
                    store,
                    dep_store,
                    orb,
                    target,
                    &base_wc,
                    &orb_config,
                    &prompt_resolver,
                    tool_policy.as_ref(),
                    context,
                    hooks,
                    execution_store,
                    prompt_store,
                    worker_evidence_dir,
                    project_key,
                )
                .await
            });
        }

        let mut completed = 0u32;
        while let Some(joined) = join_set.join_next().await {
            match joined {
                Ok(Ok(true)) => completed = completed.saturating_add(1),
                Ok(Ok(false)) => {} // dispatched but didn't end Done
                Ok(Err(e)) => tracing::warn!(error = %e, "dispatch_one errored"),
                Err(e) => tracing::warn!(error = %e, "dispatch task panicked"),
            }
        }
        Ok(completed)
    }

    /// Runs queue transitions and worker dispatch in the foreground until a
    /// target orb reaches a terminal state, the queue becomes idle, or the
    /// cycle limit is reached.
    ///
    /// This is the foreground counterpart to the daemon loop: it uses the same
    /// `tick_async` and `dispatch_ready_orbs` calls, but has a target-specific
    /// stopping condition so commands can wait on one orb without starting the
    /// background daemon.
    ///
    /// # Errors
    ///
    /// Returns an IO error if queue ticking, dispatch, hook firing, or store
    /// reads fail.
    pub async fn drain_target(
        &self,
        target_id: &OrbId,
        base_worker_config: &crate::worker::process::WorkerConfig,
        max_concurrency: usize,
        wait: bool,
        max_cycles: u32,
        interval: std::time::Duration,
    ) -> std::io::Result<DrainResult> {
        let max_cycles = max_cycles.max(1);
        let mut cycles = 0u32;
        let mut workers_completed = 0u32;

        loop {
            let Some(before) = self.orb_store.load_by_id(target_id)? else {
                return Ok(DrainResult {
                    target_id: target_id.clone(),
                    cycles,
                    workers_completed,
                    target: None,
                    reason: DrainStopReason::MissingTarget,
                });
            };
            if is_terminal(&before) {
                return Ok(DrainResult {
                    target_id: target_id.clone(),
                    cycles,
                    workers_completed,
                    target: Some(before),
                    reason: DrainStopReason::TargetTerminal,
                });
            }

            let tick = self.tick_async().await?;
            let dispatched = self
                .dispatch_ready_orbs(base_worker_config, max_concurrency)
                .await?;
            self.fire_on_queue_tick().await;

            cycles = cycles.saturating_add(1);
            workers_completed = workers_completed.saturating_add(dispatched);

            let target = self.orb_store.load_by_id(target_id)?;
            if target.as_ref().is_none_or(is_terminal) {
                return Ok(DrainResult {
                    target_id: target_id.clone(),
                    cycles,
                    workers_completed,
                    target,
                    reason: DrainStopReason::TargetTerminal,
                });
            }
            if !wait {
                return Ok(DrainResult {
                    target_id: target_id.clone(),
                    cycles,
                    workers_completed,
                    target,
                    reason: DrainStopReason::SingleCycle,
                });
            }
            if cycles >= max_cycles {
                return Ok(DrainResult {
                    target_id: target_id.clone(),
                    cycles,
                    workers_completed,
                    target,
                    reason: DrainStopReason::MaxCycles,
                });
            }
            if tick.is_idle() && dispatched == 0 {
                return Ok(DrainResult {
                    target_id: target_id.clone(),
                    cycles,
                    workers_completed,
                    target,
                    reason: DrainStopReason::Idle,
                });
            }

            tokio::time::sleep(interval).await;
        }
    }
}

/// Resolves the explicitly selected external prompt set into the ordinary
/// runtime prompt config. Only roles declared by the set are replaced.
fn load_external_prompt_set(
    mut prompt_config: crate::config::PromptConfig,
    base_dir: &Path,
) -> std::io::Result<crate::config::PromptConfig> {
    let Some(path) = prompt_config.prompt_set.clone() else {
        return Ok(prompt_config);
    };
    let path = if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
    };
    let set = crate::bench::prompts::BenchPromptSet::load_from_dir(&path).map_err(|error| {
        std::io::Error::other(format!(
            "failed to load prompts.prompt_set {}: {error}",
            path.display()
        ))
    })?;
    let selected = set.prompt_config();
    // A selected set owns its declared roles; roles it omits retain the
    // ordinary layered project configuration and built-in fallback.
    prompt_config.workers.extend(selected.workers);
    prompt_config.phases.extend(selected.phases);
    Ok(prompt_config)
}

fn is_terminal(orb: &Orb) -> bool {
    matches!(
        orb.effective_status(),
        TaskStatus::Done | TaskStatus::Failed | TaskStatus::Cancelled
    )
}

/// Indexes the slice by `parent_id`, returning a map from each
/// parent's `OrbId` to its child orbs. Lets the tick loop look up
/// children in O(1) instead of paying a full `OrbStore::load_all`
/// replay per orb.
/// Returns true when a parent has not successfully released its children for
/// execution. A phase parent releases descendants only after its
/// Refining/review work completes and it reaches Waiting or an execution
/// phase; failed and cancelled parents never release descendants.
fn blocked_by_parent_review(orb: &Orb, orbs: &[Orb]) -> bool {
    let mut parent_id = orb.parent_id.as_ref();
    while let Some(id) = parent_id {
        let Some(parent) = orbs.iter().find(|candidate| &candidate.id == id) else {
            break;
        };
        let phase_parent_released = matches!(
            parent.phase,
            Some(OrbPhase::Waiting | OrbPhase::Executing | OrbPhase::ExecutingChildren)
        );
        if parent.status == Some(OrbStatus::Review)
            || (parent.orb_type.uses_phase() && !phase_parent_released)
        {
            return true;
        }
        parent_id = parent.parent_id.as_ref();
    }
    false
}

fn index_children_by_parent(orbs: &[Orb]) -> HashMap<&OrbId, Vec<&Orb>> {
    let mut by_parent: HashMap<&OrbId, Vec<&Orb>> = HashMap::new();
    for orb in orbs {
        if let Some(parent_id) = orb.parent_id.as_ref() {
            by_parent.entry(parent_id).or_default().push(orb);
        }
    }
    by_parent
}

fn has_children(orb: &Orb, orbs: &[Orb]) -> bool {
    orbs.iter()
        .any(|candidate| candidate.parent_id.as_ref() == Some(&orb.id))
}

// ── Dispatch helpers (task 60) ───────────────────────────────────

/// What phase / prompt should drive a worker for this orb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatchTarget {
    /// Task or phase-orb in `Executing` — send the orb's description
    /// as the user prompt. Result becomes `orb.result`.
    Execute,
    /// Phase-orb in `Speccing`.
    Speccing,
    /// Phase-orb in `Decomposing`.
    Decomposing,
    /// Phase-orb in `Refining`.
    Refining,
    /// Phase-orb in `Reevaluating`.
    Reevaluating,
}

impl DispatchTarget {
    /// Stable key for a case-level phase override.
    fn tool_policy_key(self) -> &'static str {
        match self {
            Self::Execute => "execute",
            Self::Speccing => "speccing",
            Self::Decomposing => "decomposing",
            Self::Refining => "refining",
            Self::Reevaluating => "reevaluating",
        }
    }

    /// Phase profile name. Planning phases stay read-only; execution (including
    /// parent-final work) receives implementation and verification tools.
    fn tool_profile(self) -> &'static str {
        match self {
            Self::Execute => "execute",
            Self::Speccing | Self::Decomposing | Self::Refining | Self::Reevaluating => "read_only",
        }
    }

    fn prompt_kind(self) -> crate::prompt::PromptKind<'static> {
        match self {
            Self::Execute => crate::prompt::PromptKind::Worker("execute"),
            Self::Speccing => crate::prompt::PromptKind::Phase("speccing"),
            Self::Decomposing => crate::prompt::PromptKind::Phase("decomposing"),
            Self::Refining => crate::prompt::PromptKind::Phase("refining"),
            Self::Reevaluating => crate::prompt::PromptKind::Phase("reevaluating"),
        }
    }

    fn model_role(self) -> crate::config::ModelRole<'static> {
        match self {
            Self::Execute => crate::config::ModelRole::Worker("execute"),
            Self::Speccing => crate::config::ModelRole::Phase("speccing"),
            Self::Decomposing => crate::config::ModelRole::Phase("decomposing"),
            Self::Refining => crate::config::ModelRole::Phase("refining"),
            Self::Reevaluating => crate::config::ModelRole::Phase("reevaluating"),
        }
    }
}

/// Returns `Some(target)` when the orb is in a worker-eligible state
/// AND hasn't been dispatched yet (`execution` is None).
fn dispatch_target_for(orb: &Orb) -> Option<DispatchTarget> {
    if orb.execution.is_some() {
        // Already dispatched — don't redispatch on the same tick.
        return None;
    }
    if orb.orb_type.uses_phase() {
        match orb.phase {
            Some(OrbPhase::Speccing) => Some(DispatchTarget::Speccing),
            Some(OrbPhase::Decomposing) => Some(DispatchTarget::Decomposing),
            Some(OrbPhase::Refining) => Some(DispatchTarget::Refining),
            Some(OrbPhase::Reevaluating) => Some(DispatchTarget::Reevaluating),
            Some(OrbPhase::Executing) => Some(DispatchTarget::Execute),
            _ => None,
        }
    } else if orb.status == Some(OrbStatus::Active) {
        Some(DispatchTarget::Execute)
    } else {
        None
    }
}

/// A deliberately small, content-based snapshot of an assigned workdir.
/// Runtime and VCS metadata are excluded: they are not task artifacts and can
/// change while a worker is running without proving implementation progress.
async fn workspace_fingerprint(workdir: &Path) -> std::io::Result<String> {
    let workdir = workdir.to_path_buf();
    tokio::task::spawn_blocking(move || workspace_fingerprint_blocking(&workdir))
        .await
        .map_err(std::io::Error::other)?
}

fn workspace_fingerprint_blocking(workdir: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    fn visit(root: &Path, dir: &Path, entries: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            if matches!(
                name.to_str(),
                Some(".git" | ".orbs" | ".orboros" | "target")
            ) {
                continue;
            }
            let ty = entry.file_type()?;
            if ty.is_dir() {
                visit(root, &path, entries)?;
            } else if ty.is_file() {
                entries.push(path.strip_prefix(root).unwrap_or(&path).to_path_buf());
            }
        }
        Ok(())
    }

    let mut entries = Vec::new();
    visit(workdir, workdir, &mut entries)?;
    entries.sort();
    let mut hasher = Sha256::new();
    for relative in entries {
        hasher.update(relative.to_string_lossy().as_bytes());
        let mut file = std::fs::File::open(workdir.join(&relative))?;
        let mut buf = [0_u8; 8192];
        loop {
            let bytes = file.read(&mut buf)?;
            if bytes == 0 {
                break;
            }
            hasher.update(&buf[..bytes]);
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn partial_artifact_recovery_is_eligible(
    orb: &Orb,
    target: DispatchTarget,
    outcome: &crate::worker::dispatcher::DispatchOutcome,
    parent_has_acceptance_criteria: bool,
    workspace_before: Option<&str>,
    workspace_after: Option<&str>,
) -> bool {
    target == DispatchTarget::Execute
        && orb.parent_id.is_some()
        && parent_has_acceptance_criteria
        && outcome.status == crate::worker::dispatcher::DispatchStatus::Error
        && outcome.terminal_retry.is_some()
        && workspace_before.is_some_and(|before| Some(before) != workspace_after)
}

async fn recover_partial_artifact(
    orb: &Orb,
    parent_description: &str,
    failed_outcome: &crate::worker::dispatcher::DispatchOutcome,
    user_prompt: &str,
    worker_config: &crate::worker::process::WorkerConfig,
    system_prompt: &str,
    hooks: Option<&crate::hooks::HookSink>,
) -> anyhow::Result<crate::worker::dispatcher::DispatchOutcome> {
    let mut recovery_config = worker_config.clone();
    recovery_config.worker_id = Some(uuid::Uuid::new_v4().to_string());
    recovery_config.max_iterations = Some(
        recovery_config
            .max_iterations
            .unwrap_or(MAX_PARTIAL_ARTIFACT_RECOVERY_ITERATIONS)
            .clamp(1, MAX_PARTIAL_ARTIFACT_RECOVERY_ITERATIONS),
    );
    let failure = failed_outcome
        .error
        .as_deref()
        .unwrap_or("terminal worker retry did not complete");
    recovery_config.system_prompt = system_prompt.into();
    let prompt = format!(
        "Parent acceptance context:\n{parent_description}\n\nAssigned child task:\n{user_prompt}\n\nPrior terminal outcome:\n{failure}"
    );
    crate::worker::dispatcher::dispatch_orb_once(orb, &prompt, &recovery_config, hooks).await
}

/// Owned-argument version of `dispatch_one`, suitable for `tokio::spawn`.
/// Returns `Ok(true)` when the orb ended at Done, `Ok(false)` otherwise.
struct DispatchContext<'a> {
    orbs: &'a [Orb],
    edges: &'a [orbs::dep::DepEdge],
}

async fn dispatch_one_owned(
    store: OrbStore,
    dep_store: DepStore,
    mut orb: Orb,
    target: DispatchTarget,
    base_wc: &crate::worker::process::WorkerConfig,
    model_config: &crate::config::OrbConfig,
    prompt_resolver: &crate::prompt::PromptResolver,
    tool_policy: Option<&crate::routing::profile::PhaseToolPolicy>,
    context: DispatchContext<'_>,
    hooks: Option<Arc<crate::hooks::HookSink>>,
    execution_store: crate::execution::ExecutionStore,
    prompt_store: Option<crate::execution::PromptStore>,
    worker_evidence_dir: PathBuf,
    project_key: Option<String>,
) -> std::io::Result<bool> {
    use crate::worker::dispatcher::{
        apply_dispatch_outcome_with_review, dispatch_orb, worker_config_for_with_model_config,
    };

    let (built_in_system, user) = match target {
        DispatchTarget::Speccing => crate::phases::speccing::build_prompt(&orb),
        DispatchTarget::Decomposing => {
            crate::phases::decompose::build_prompt(&orb, &model_config.models)
        }
        DispatchTarget::Refining => crate::phases::refinement::build_prompt(&orb),
        DispatchTarget::Reevaluating => crate::phases::re_evaluation::build_prompt(&orb, &[]),
        DispatchTarget::Execute => (
            crate::prompt::built_in_worker_system_prompt("execute").to_string(),
            orb.description.clone(),
        ),
    };
    let context_budget = if target == DispatchTarget::Execute && orb.has_parent_final_work {
        crate::prompt_context::PARENT_FINAL_CONTEXT_BUDGET
    } else if orb.parent_id.is_some() {
        crate::prompt_context::CHILD_EXECUTION_CONTEXT_BUDGET
    } else {
        crate::prompt_context::REVIEW_CONTEXT_BUDGET
    };
    let task_context = crate::prompt_context::build_orb_task_context_with_budget(
        &orb,
        context.orbs,
        context.edges,
        context_budget,
    );
    let mut prompt_context = task_context.metrics;
    prompt_context.base_user_chars = u32::try_from(user.chars().count()).unwrap_or(u32::MAX);
    let mut user = crate::prompt_context::append_task_context(&user, &task_context.text);
    prompt_context.final_user_prompt_chars =
        u32::try_from(user.chars().count()).unwrap_or(u32::MAX);

    let prompt_kind = target.prompt_kind();
    let prompt_category = prompt_kind.category();
    let resolved = prompt_resolver
        .resolve_system_prompt(prompt_kind, &built_in_system)
        .map_err(std::io::Error::other)?;
    let system = resolved.system_prompt;
    let prompt_source = resolved.source.label();
    let mut target_base_wc = base_wc.clone();
    let resolved_model = model_config
        .model_resolver()
        .resolve(target.model_role())
        .map_err(std::io::Error::other)?;
    if resolved_model.source != "default_model" {
        target_base_wc.model = resolved_model.model;
    }
    target_base_wc.tools = crate::routing::profile::resolve_phase_tools(
        &model_config.tool_profiles,
        &base_wc.tools,
        target.tool_policy_key(),
        target.tool_profile(),
        tool_policy,
    );
    let mut wc = worker_config_for_with_model_config(&orb, &target_base_wc, &system, model_config)
        .map_err(std::io::Error::other)?;
    let log_root = worker_evidence_dir;
    configure_worker_runtime(&mut wc, &log_root, &orb.id.to_string(), 1)?;
    let effective_system_prompt =
        crate::worker::process::effective_system_prompt(&wc.system_prompt, &wc.tools);
    prompt_context.effective_system_prompt_chars =
        u32::try_from(effective_system_prompt.chars().count()).unwrap_or(u32::MAX);
    tracing::info!(
        orb = %orb.id,
        title = %orb.title,
        target = ?target,
        project = project_key.as_deref().unwrap_or("unregistered"),
        phase = ?orb.phase.unwrap_or(OrbPhase::Pending),
        tools = ?wc.tools,
        "dispatching ready orb",
    );

    let workspace_before = match wc.cwd.as_deref() {
        Some(workdir) => workspace_fingerprint(workdir).await.ok(),
        None => None,
    };
    // This is intentionally above the dispatcher's fixed provider/transport
    // retry. A configured retry repeats a completed failed orb/phase attempt;
    // malformed-output, provider, and bounded-recovery behavior inside one
    // attempt remains unconditional.
    let mut completed_retries = 0_u32;
    let mut outer_attempt = 1_u32;
    let mut outcome = loop {
        let outcome = dispatch_orb(&orb, &user, &wc, hooks.as_deref())
            .await
            .map_err(std::io::Error::other)?;
        let failed = matches!(
            outcome.status,
            crate::worker::dispatcher::DispatchStatus::Error
                | crate::worker::dispatcher::DispatchStatus::Failed
        );
        if !failed
            || !completed_dispatch_retry_allowed(model_config.workers.retries, completed_retries)
        {
            break outcome;
        }
        let mut retry_record = crate::execution::ExecutionRecord::from_outcome(
            &orb,
            prompt_category.clone(),
            target.tool_policy_key(),
            Some("outer_retry".into()),
            wc.tools.clone(),
            &outcome,
            None,
        );
        retry_record.phase_retry = Some(crate::execution::PhaseRetryDiagnostic {
            attempt: outer_attempt,
            reason: "completed_dispatch_failed".into(),
        });
        execution_store.append(&retry_record)?;
        let backoff = completed_dispatch_retry_backoff(completed_retries);
        completed_retries = completed_retries.saturating_add(1);
        outer_attempt = outer_attempt.saturating_add(1);
        configure_worker_runtime(&mut wc, &log_root, &orb.id.to_string(), outer_attempt)?;
        let retry_progress = if model_config.workers.retries == -1 {
            format!("{completed_retries}/unlimited")
        } else {
            format!("{completed_retries}/{}", model_config.workers.retries)
        };
        tracing::warn!(orb = %orb.id, retry = %retry_progress, backoff_ms = backoff.as_millis(), "retrying completed failed orb dispatch");
        tokio::time::sleep(backoff).await;
    };
    outcome.retries = outcome.retries.saturating_add(completed_retries);
    let mut refinement_round = None;
    if target == DispatchTarget::Refining
        && outcome.status == crate::worker::dispatcher::DispatchStatus::Done
    {
        let max_rounds = model_config.refinement.max_rounds;
        // A reset preserves the edits already applied to the orb. Resume the
        // next unfinished round from existing evidence rather than replaying
        // those successful rounds or adding a new mutable orb-state field.
        let refinement_history = execution_store.read_all().unwrap_or_default();
        let resumed_round = completed_refinement_round_at_checkpoint(
            &refinement_history,
            &orb.id.to_string(),
            orb.updated_at,
        );
        let mut round = resumed_round.saturating_add(1).max(1);
        loop {
            let Some(response) = outcome.response.as_deref() else {
                outcome.status = crate::worker::dispatcher::DispatchStatus::Failed;
                outcome.error = Some("refinement completed without a response".into());
                break;
            };
            let Some(plan) = crate::phases::refinement::parse_response(response) else {
                outcome.status = crate::worker::dispatcher::DispatchStatus::Failed;
                outcome.error = Some("refinement response was not a valid plan".into());
                break;
            };
            tracing::info!(orb = %orb.id, refinement_round = round, max_refinement_rounds = max_rounds, "processing refinement round");
            let before = (
                orb.description.clone(),
                orb.design.clone(),
                orb.acceptance_criteria.clone(),
            );
            crate::phases::refinement::apply_plan(&mut orb, &plan);
            let material_changed = before
                != (
                    orb.description.clone(),
                    orb.design.clone(),
                    orb.acceptance_criteria.clone(),
                );
            let reason = if model_config.refinement.stop_on_model_complete && plan.complete {
                Some("model_declared_complete")
            } else if model_config.refinement.stop_on_no_material_change && !material_changed {
                Some("no_material_change")
            } else if round >= max_rounds {
                Some("max_rounds")
            } else {
                None
            };
            let diagnostic = crate::execution::RefinementRoundDiagnostic {
                round,
                max_rounds,
                material_changed,
                model_declared_complete: plan.complete,
                termination_reason: reason.map(str::to_string),
            };
            if reason.is_some() {
                refinement_round = Some(diagnostic);
                break;
            }
            let mut round_record = crate::execution::ExecutionRecord::from_outcome(
                &orb,
                prompt_category.clone(),
                target.tool_policy_key(),
                Some("refinement_round".into()),
                wc.tools.clone(),
                &outcome,
                None,
            );
            round_record.refinement_round = Some(diagnostic);
            execution_store.append(&round_record)?;
            round = round.saturating_add(1);
            wc.worker_id = Some(uuid::Uuid::new_v4().to_string());
            configure_worker_runtime(&mut wc, &log_root, &orb.id.to_string(), round)?;
            let (_, next_user) = crate::phases::refinement::build_prompt(&orb);
            user = crate::prompt_context::append_task_context(&next_user, &task_context.text);
            let mut round_retries = 0_u32;
            outcome = loop {
                let round_outcome = dispatch_orb(&orb, &user, &wc, hooks.as_deref())
                    .await
                    .map_err(std::io::Error::other)?;
                let failed = matches!(
                    round_outcome.status,
                    crate::worker::dispatcher::DispatchStatus::Error
                        | crate::worker::dispatcher::DispatchStatus::Failed
                );
                if !failed
                    || !completed_dispatch_retry_allowed(
                        model_config.workers.retries,
                        round_retries,
                    )
                {
                    break round_outcome;
                }
                let mut retry_record = crate::execution::ExecutionRecord::from_outcome(
                    &orb,
                    prompt_category.clone(),
                    target.tool_policy_key(),
                    Some("outer_retry".into()),
                    wc.tools.clone(),
                    &round_outcome,
                    None,
                );
                retry_record.phase_retry = Some(crate::execution::PhaseRetryDiagnostic {
                    attempt: round_retries.saturating_add(1),
                    reason: "completed_dispatch_failed".into(),
                });
                execution_store.append(&retry_record)?;
                let backoff = completed_dispatch_retry_backoff(round_retries);
                round_retries = round_retries.saturating_add(1);
                configure_worker_runtime(
                    &mut wc,
                    &log_root,
                    &orb.id.to_string(),
                    round.saturating_mul(1_000).saturating_add(round_retries),
                )?;
                tracing::warn!(
                    orb = %orb.id,
                    refinement_round = round,
                    retry = %if model_config.workers.retries == -1 { format!("{round_retries}/unlimited") } else { format!("{round_retries}/{}", model_config.workers.retries) },
                    backoff_ms = backoff.as_millis(),
                    "retrying completed failed refinement round"
                );
                tokio::time::sleep(backoff).await;
            };
            outcome.retries = outcome.retries.saturating_add(round_retries);
            if outcome.status != crate::worker::dispatcher::DispatchStatus::Done {
                refinement_round = Some(crate::execution::RefinementRoundDiagnostic {
                    round,
                    max_rounds,
                    material_changed: false,
                    model_declared_complete: false,
                    termination_reason: Some("worker_failed".into()),
                });
                break;
            }
        }
    }
    if outcome.status == crate::worker::dispatcher::DispatchStatus::Done
        && target == DispatchTarget::Decomposing
        && model_config.models.coordinator_model_choice
    {
        if let Some(response) = outcome.response.as_deref() {
            if let Some(plan) = crate::phases::decompose::parse_response(response) {
                if let Err(error) =
                    crate::phases::decompose::validate_model_options(&plan, &model_config.models)
                {
                    outcome.status = crate::worker::dispatcher::DispatchStatus::Failed;
                    outcome.error = Some(format!("invalid coordinator model choice: {error}"));
                }
            }
        }
    }
    let mut outcome = crate::worker::dispatcher::with_prompt_metadata(
        outcome,
        prompt_category.clone(),
        &effective_system_prompt,
        prompt_source,
    );
    if let Some(prompt_store) = prompt_store {
        prompt_store.append(&crate::execution::PromptRecord::new(
            &orb,
            prompt_category.clone(),
            outcome.dispatched_at,
            effective_system_prompt.clone(),
            user.clone(),
            outcome.prompt_tokens,
            prompt_context.clone(),
        ))?;
    }
    let workspace_after = match wc.cwd.as_deref() {
        Some(workdir) => workspace_fingerprint(workdir).await.ok(),
        None => None,
    };
    let parent_description = orb.parent_id.as_ref().and_then(|parent_id| {
        context
            .orbs
            .iter()
            .find(|candidate| &candidate.id == parent_id)
            .map(|parent| parent.description.as_str())
    });
    let recovery_eligible = partial_artifact_recovery_is_eligible(
        &orb,
        target,
        &outcome,
        parent_description.is_some_and(|description| !description.trim().is_empty()),
        workspace_before.as_deref(),
        workspace_after.as_deref(),
    );

    let mut execution_record = crate::execution::ExecutionRecord::from_outcome(
        &orb,
        prompt_category.clone(),
        target.tool_policy_key(),
        Some(if tool_policy.is_some() {
            "case_override".into()
        } else {
            "phase_default".into()
        }),
        wc.tools.clone(),
        &outcome,
        Some(prompt_context),
    );
    if target == DispatchTarget::Decomposing {
        execution_record.phase_retry = Some(crate::execution::PhaseRetryDiagnostic {
            attempt: 1,
            reason: "initial".into(),
        });
    }
    if let Some(round) = refinement_round {
        execution_record.refinement_round = Some(round);
    }
    if recovery_eligible {
        execution_record.partial_artifact_recovery =
            Some(crate::execution::PartialArtifactRecoveryDiagnostic {
                recovery_attempted: true,
                workspace_changed: true,
                workdir: wc
                    .cwd
                    .as_ref()
                    .map_or_else(String::new, |path| path.display().to_string()),
                initial_error: outcome.error.clone(),
                recovery_succeeded: false,
                recovery_artifact_path: None,
            });
    }
    execution_store.append(&execution_record)?;

    if recovery_eligible {
        let recovery_prompt = prompt_resolver
            .resolve_system_prompt(
                crate::prompt::PromptKind::Phase("partial_artifact_recovery"),
                PARTIAL_ARTIFACT_RECOVERY_SYSTEM_PROMPT,
            )
            .map_err(std::io::Error::other)?;
        let recovery_outcome = recover_partial_artifact(
            &orb,
            parent_description.unwrap_or_default(),
            &outcome,
            &user,
            &wc,
            &recovery_prompt.system_prompt,
            hooks.as_deref(),
        )
        .await
        .map_err(std::io::Error::other)?;
        let recovery_outcome = crate::worker::dispatcher::with_prompt_metadata(
            recovery_outcome,
            format!("{prompt_category}.partial_artifact_recovery"),
            &crate::worker::process::effective_system_prompt(
                &recovery_prompt.system_prompt,
                &wc.tools,
            ),
            recovery_prompt.source.label(),
        );
        let mut recovery_record = crate::execution::ExecutionRecord::from_outcome(
            &orb,
            format!("{}.partial_artifact_recovery", target.tool_policy_key()),
            target.tool_policy_key(),
            Some("bounded_partial_artifact_recovery".into()),
            wc.tools.clone(),
            &recovery_outcome,
            None,
        );
        recovery_record.partial_artifact_recovery =
            Some(crate::execution::PartialArtifactRecoveryDiagnostic {
                recovery_attempted: true,
                workspace_changed: true,
                workdir: wc
                    .cwd
                    .as_ref()
                    .map_or_else(String::new, |path| path.display().to_string()),
                initial_error: outcome.error.clone(),
                recovery_succeeded: recovery_outcome.status
                    == crate::worker::dispatcher::DispatchStatus::Done,
                recovery_artifact_path: recovery_outcome
                    .runtime
                    .as_ref()
                    .map(|runtime| runtime.transcript_path.clone()),
            });
        execution_store.append(&recovery_record)?;
        if recovery_outcome.status == crate::worker::dispatcher::DispatchStatus::Done {
            outcome = recovery_outcome;
        }
    }

    // A successful decomposition is only complete once its children and
    // edges exist.  Materialize before advancing the parent so a crash or a
    // failed write cannot expose a parent in Refining with no child graph.
    // The stores deduplicate child IDs and edges on replay, making a repeated
    // control-plane delivery safe.
    if outcome.status == crate::worker::dispatcher::DispatchStatus::Done
        && target == DispatchTarget::Decomposing
    {
        let response = outcome.response.as_deref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "decomposition completed without a response",
            )
        })?;
        let plan = crate::phases::decompose::parse_response(response).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "decomposition response was not a valid plan",
            )
        })?;
        crate::phases::decompose::validate_model_options(&plan, &model_config.models)
            .map_err(std::io::Error::other)?;
        let result = crate::phases::decompose::materialize_plan(&orb, &plan)
            .map_err(std::io::Error::other)?;
        crate::phases::decompose::apply_decomposition(&result, &store, &dep_store)
            .map_err(std::io::Error::other)?;
        orb.has_parent_final_work = plan.has_parent_final_work;
    }

    apply_dispatch_outcome_with_review(
        &mut orb,
        &outcome,
        model_config.review.review_on_completion,
    )
    .map_err(std::io::Error::other)?;

    if outcome.status == crate::worker::dispatcher::DispatchStatus::Done
        && target == DispatchTarget::Refining
        && orb.phase == Some(OrbPhase::Review)
        && !crate::phases::review::needs_review(&orb, model_config)
    {
        orb.set_phase(OrbPhase::Waiting)
            .map_err(std::io::Error::other)?;
    }

    // For structured phases, also parse the response into a plan and
    // apply it so the orb's design / decomposition / refinement /
    // re-eval fields get populated alongside `result`.
    if outcome.status == crate::worker::dispatcher::DispatchStatus::Done {
        if let Some(ref response) = outcome.response {
            match target {
                DispatchTarget::Speccing => {
                    if let Some(plan) = crate::phases::speccing::parse_response(response) {
                        crate::phases::speccing::apply_plan(&mut orb, &plan);
                    }
                }
                DispatchTarget::Refining => {
                    if let Some(plan) = crate::phases::refinement::parse_response(response) {
                        crate::phases::refinement::apply_plan(&mut orb, &plan);
                    }
                }
                DispatchTarget::Reevaluating => {
                    if let Some(plan) = crate::phases::re_evaluation::parse_response(response) {
                        let _ = crate::phases::re_evaluation::apply_plan(&mut orb, &plan);
                    }
                }
                // Decompose response holds subtasks — applying them
                // creates child orbs, which needs OrbStore + DepStore
                // and is out of scope for this commit.
                DispatchTarget::Decomposing | DispatchTarget::Execute => {}
            }
        }
    }

    store.update(&orb)?;
    Ok(outcome.status == crate::worker::dispatcher::DispatchStatus::Done)
}

/// Requests an isolated Heddle runtime beneath the project-local evidence
/// home. The worker ID and outer retry ordinal make every transcript distinct.
fn configure_worker_runtime(
    worker_config: &mut crate::worker::process::WorkerConfig,
    log_root: &Path,
    orb_id: &str,
    outer_attempt: u32,
) -> std::io::Result<()> {
    let config_path = worker_config
        .runtime
        .as_ref()
        .and_then(|runtime| runtime.config_path.clone());
    let worker_id = worker_config
        .worker_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    worker_config.worker_id = Some(worker_id.clone());
    let attempt_dir = log_root
        .join(orb_id)
        .join(format!("attempt-{outer_attempt}"));
    std::fs::create_dir_all(&attempt_dir)?;
    worker_config.runtime = Some(crate::ipc::types::RuntimePlacementConfig {
        mode: Some(crate::ipc::types::RuntimeMode::Isolated),
        state_root: Some(attempt_dir.join("state").display().to_string()),
        transcript_path: Some(
            attempt_dir
                .join(format!("worker-{worker_id}.jsonl"))
                .display()
                .to_string(),
        ),
        config_path,
        inherit_ambient_config: Some(false),
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use orbs::dep::{DepEdge, EdgeType};
    use orbs::orb::OrbType;

    /// Helper: sets up a temp dir with `orb_store`, `dep_store`, and `base_dir`.
    fn setup() -> (tempfile::TempDir, OrbStore, DepStore, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_path_buf();
        let orb_store = OrbStore::new(base.join("orbs.jsonl"));
        let dep_store = DepStore::new(base.join("deps.jsonl"));
        (tmp, orb_store, dep_store, base)
    }

    #[test]
    fn external_prompt_set_overrides_declared_role_with_private_provenance() {
        let temp = tempfile::tempdir().unwrap();
        let set = temp.path().join("bench/prompts/composable-v1");
        std::fs::create_dir_all(set.join("roles")).unwrap();
        std::fs::write(
            set.join("composition.toml"),
            "[roles.execute]\nfragments = [\"roles/execute.md\"]\n",
        )
        .unwrap();
        std::fs::write(set.join("roles/execute.md"), "private execute prompt").unwrap();
        let config = crate::config::PromptConfig {
            prompt_set: Some("bench/prompts/composable-v1".into()),
            ..Default::default()
        };

        let config = load_external_prompt_set(config, temp.path()).unwrap();
        let resolved = crate::prompt::PromptResolver::from_config(config, Some(temp.path()))
            .resolve_system_prompt(
                crate::prompt::PromptKind::Worker("execute"),
                "built in execute prompt",
            )
            .unwrap();

        assert_eq!(resolved.system_prompt, "private execute prompt");
        assert_eq!(
            resolved.source.label(),
            format!(
                "prompt_set:composable-v1:execute:{}",
                crate::prompt::prompt_hash("private execute prompt")
            )
        );
    }

    #[test]
    fn completed_dispatch_retry_policy_handles_disabled_finite_and_unlimited() {
        assert!(!completed_dispatch_retry_allowed(0, 0));
        assert!(completed_dispatch_retry_allowed(2, 0));
        assert!(completed_dispatch_retry_allowed(2, 1));
        assert!(!completed_dispatch_retry_allowed(2, 2));
        assert!(completed_dispatch_retry_allowed(-1, 10_000));
    }

    #[test]
    fn refinement_checkpoint_retries_failed_round_without_reading_later_history() {
        let first = chrono::Utc::now();
        let failed_second = first + chrono::Duration::seconds(1);
        let later_third = first + chrono::Duration::seconds(2);
        let record = |round, completed_at, termination_reason: Option<&str>| {
            serde_json::from_value(serde_json::json!({
                "orb_id": "orb-refine",
                "parent_id": null,
                "dispatch_kind": "phase.refining",
                "status": termination_reason.map_or("done", |_| "error"),
                "dispatched_at": first,
                "completed_at": completed_at,
                "refinement_round": {
                    "round": round,
                    "max_rounds": 3,
                    "material_changed": true,
                    "model_declared_complete": false,
                    "termination_reason": termination_reason,
                },
            }))
            .unwrap()
        };
        let records = vec![
            record(1, first, None),
            record(2, failed_second, Some("worker_failed")),
            record(3, later_third, Some("max_rounds")),
        ];

        assert_eq!(
            completed_refinement_round_at_checkpoint(&records, "orb-refine", failed_second),
            1,
            "round two must be retried and later round-three evidence ignored"
        );
    }

    #[test]
    fn failed_phase_parent_does_not_release_child_dispatch() {
        let mut parent = Orb::new("Parent", "must finish refining first").with_type(OrbType::Epic);
        parent.set_phase(OrbPhase::Failed).unwrap();
        let child = Orb::new("Child", "must remain blocked")
            .with_parent(parent.id.clone(), Some(parent.id.clone()));

        assert!(blocked_by_parent_review(&child, &[parent]));
    }

    #[test]
    fn waiting_phase_parent_releases_child_dispatch() {
        let mut parent = Orb::new("Parent", "finished refining").with_type(OrbType::Epic);
        parent.phase = Some(OrbPhase::Waiting); // test setup: prior phases completed
        let child = Orb::new("Child", "may execute")
            .with_parent(parent.id.clone(), Some(parent.id.clone()));

        assert!(!blocked_by_parent_review(&child, &[parent]));
    }

    #[test]
    fn worker_runtime_isolated_under_project_log_home() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::worker::process::WorkerConfig {
            command: "mock".into(),
            args: vec![],
            cwd: None,
            env: vec![],
            model: "test".into(),
            system_prompt: String::new(),
            tools: vec![],
            max_iterations: None,
            init_timeout: None,
            send_timeout: None,
            shutdown_timeout: None,
            task_id: None,
            worker_id: Some("worker-1".into()),
            runtime: Some(crate::ipc::types::RuntimePlacementConfig {
                mode: None,
                state_root: None,
                transcript_path: None,
                config_path: Some("/tmp/heddle.toml".into()),
                inherit_ambient_config: None,
            }),
            routing: None,
        };
        configure_worker_runtime(&mut config, dir.path(), "orb-a", 2).unwrap();
        let runtime = config.runtime.unwrap();
        assert_eq!(runtime.mode, Some(crate::ipc::types::RuntimeMode::Isolated));
        assert!(runtime
            .transcript_path
            .unwrap()
            .ends_with("orb-a/attempt-2/worker-worker-1.jsonl"));
        assert_eq!(runtime.config_path.as_deref(), Some("/tmp/heddle.toml"));
    }

    #[test]
    fn custom_worker_evidence_dir_overrides_local_log_default() {
        let (_tmp, orb_store, dep_store, base) = setup();
        let evidence_dir = base.join("shared-project").join("transcripts");
        let queue = QueueLoop::new(orb_store, dep_store, base)
            .with_worker_evidence_dir(evidence_dir.clone());

        assert_eq!(queue.worker_evidence_dir, evidence_dir);
    }

    #[tokio::test]
    async fn workspace_fingerprint_tracks_artifacts_but_ignores_runtime_paths() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("src.txt"), "before").unwrap();
        let before = workspace_fingerprint(temp.path()).await.unwrap();

        std::fs::create_dir_all(temp.path().join("target")).unwrap();
        std::fs::write(temp.path().join("target/runtime.txt"), "ignored").unwrap();
        assert_eq!(workspace_fingerprint(temp.path()).await.unwrap(), before);

        std::fs::write(temp.path().join("src.txt"), "after").unwrap();
        assert_ne!(workspace_fingerprint(temp.path()).await.unwrap(), before);
    }

    // ── tick with empty store ────────────────────────────────────────

    #[test]
    fn tick_with_empty_store_returns_idle() {
        let (_tmp, orb_store, dep_store, base) = setup();
        let ql = QueueLoop::new(orb_store, dep_store, base);

        let result = ql.tick().unwrap();
        assert!(result.is_idle());
        assert_eq!(result, TickResult::default());
    }

    #[tokio::test]
    async fn drain_target_stops_immediately_for_terminal_target() {
        let (_tmp, orb_store, dep_store, base) = setup();
        let mut orb = Orb::new("Done", "Already complete").with_type(OrbType::Task);
        orb.set_status(OrbStatus::Active).unwrap();
        orb.set_status(OrbStatus::Done).unwrap();
        orb_store.append(&orb).unwrap();
        let ql = QueueLoop::new(orb_store, dep_store, base);
        let wc = crate::worker::process::WorkerConfig {
            command: "unused".into(),
            args: vec![],
            cwd: None,
            env: vec![],
            model: "mock/drain".into(),
            system_prompt: String::new(),
            tools: vec![],
            max_iterations: None,
            init_timeout: None,
            send_timeout: None,
            shutdown_timeout: None,
            task_id: None,
            worker_id: None,
            runtime: None,
            routing: None,
        };

        let result = ql
            .drain_target(
                &orb.id,
                &wc,
                1,
                true,
                10,
                std::time::Duration::from_millis(1),
            )
            .await
            .unwrap();

        assert_eq!(result.reason, DrainStopReason::TargetTerminal);
        assert_eq!(result.cycles, 0);
        assert_eq!(result.workers_completed, 0);
    }

    // ── tick detects pipeline orbs ───────────────────────────────────

    #[test]
    fn tick_starts_pipeline_for_pending_epic() {
        let (_tmp, orb_store, dep_store, base) = setup();

        let epic = Orb::new("My epic", "Big feature").with_type(OrbType::Epic);
        assert_eq!(epic.phase, Some(OrbPhase::Pending));
        orb_store.append(&epic).unwrap();

        let ql = QueueLoop::new(orb_store.clone(), dep_store, base.clone());
        let result = ql.tick().unwrap();

        assert_eq!(result.pipelines_started, 1);

        // The epic should now be in Speccing phase
        let updated = orb_store.load_by_id(&epic.id).unwrap().unwrap();
        assert_eq!(updated.phase, Some(OrbPhase::Speccing));

        // Pipeline directory should exist
        assert!(base.join("pipelines").exists());
    }

    #[test]
    fn tick_starts_pipeline_for_pending_feature() {
        let (_tmp, orb_store, dep_store, base) = setup();

        let feature = Orb::new("Auth feature", "Add auth").with_type(OrbType::Feature);
        orb_store.append(&feature).unwrap();

        let ql = QueueLoop::new(orb_store.clone(), dep_store, base);
        let result = ql.tick().unwrap();

        assert_eq!(result.pipelines_started, 1);
        let updated = orb_store.load_by_id(&feature.id).unwrap().unwrap();
        assert_eq!(updated.phase, Some(OrbPhase::Speccing));
    }

    #[test]
    fn tick_ignores_non_pending_epics() {
        let (_tmp, orb_store, dep_store, base) = setup();

        let mut epic = Orb::new("Active epic", "Already running").with_type(OrbType::Epic);
        epic.set_phase(OrbPhase::Speccing).unwrap();
        orb_store.append(&epic).unwrap();

        let ql = QueueLoop::new(orb_store, dep_store, base);
        let result = ql.tick().unwrap();

        assert_eq!(result.pipelines_started, 0);
    }

    #[test]
    fn tick_ignores_tasks_for_pipeline() {
        let (_tmp, orb_store, dep_store, base) = setup();

        let task = Orb::new("Regular task", "No pipeline needed");
        orb_store.append(&task).unwrap();

        let ql = QueueLoop::new(orb_store, dep_store, base);
        let result = ql.tick().unwrap();

        assert_eq!(result.pipelines_started, 0);
    }

    // ── tick detects ready orbs ──────────────────────────────────────

    #[test]
    fn tick_executes_ready_pending_task() {
        let (_tmp, orb_store, dep_store, base) = setup();

        let task = Orb::new("Ready task", "No blockers");
        orb_store.append(&task).unwrap();

        let ql = QueueLoop::new(orb_store.clone(), dep_store, base);
        let result = ql.tick().unwrap();

        assert_eq!(result.orbs_executed, 1);
        let updated = orb_store.load_by_id(&task.id).unwrap().unwrap();
        assert_eq!(updated.status, Some(OrbStatus::Active));
    }

    #[test]
    fn tick_does_not_execute_blocked_task() {
        let (_tmp, orb_store, dep_store, base) = setup();

        let blocker = Orb::new("Blocker", "Must finish first");
        let task = Orb::new("Blocked task", "Waiting on blocker");
        orb_store.append(&blocker).unwrap();
        orb_store.append(&task).unwrap();

        // blocker blocks task
        let edge = DepEdge::new(blocker.id.clone(), task.id.clone(), EdgeType::Blocks);
        dep_store.add_edge(edge).unwrap();

        let ql = QueueLoop::new(orb_store.clone(), dep_store, base);
        let result = ql.tick().unwrap();

        // Blocker should be executed (it's ready), but blocked task should not
        assert_eq!(result.orbs_executed, 1);
        let updated_blocker = orb_store.load_by_id(&blocker.id).unwrap().unwrap();
        assert_eq!(updated_blocker.status, Some(OrbStatus::Active));

        // Re-load to get updated state
        let all_orbs = orb_store.load_all().unwrap();
        let blocked_task = all_orbs.iter().find(|o| o.id == task.id).unwrap();
        assert_eq!(blocked_task.status, Some(OrbStatus::Pending));
    }

    #[test]
    fn tick_executes_waiting_phase_orb() {
        let (_tmp, orb_store, dep_store, base) = setup();

        let mut feature = Orb::new("Waiting feature", "Ready to go").with_type(OrbType::Feature);
        // Bypass step-by-step validation for test setup — we want the orb in
        // Waiting for the purpose of this test, not exercise the pipeline.
        feature.phase = Some(OrbPhase::Waiting);
        orb_store.append(&feature).unwrap();

        let ql = QueueLoop::new(orb_store.clone(), dep_store, base);
        let result = ql.tick().unwrap();

        assert_eq!(result.orbs_executed, 1);
        let updated = orb_store.load_by_id(&feature.id).unwrap().unwrap();
        assert_eq!(updated.phase, Some(OrbPhase::Executing));
    }

    // ── root completion detection ────────────────────────────────────

    #[test]
    fn tick_completes_child_only_root_when_all_children_done() {
        let (_tmp, orb_store, dep_store, base) = setup();

        let mut parent = Orb::new("Parent epic", "Has children").with_type(OrbType::Epic);
        parent.phase = Some(OrbPhase::Waiting); // approved, children complete
        orb_store.append(&parent).unwrap();

        let mut child1 =
            Orb::new("Child 1", "First").with_parent(parent.id.clone(), Some(parent.id.clone()));
        child1.status = Some(OrbStatus::Done); // test setup
        orb_store.append(&child1).unwrap();

        let mut child2 =
            Orb::new("Child 2", "Second").with_parent(parent.id.clone(), Some(parent.id.clone()));
        child2.status = Some(OrbStatus::Done); // test setup
        orb_store.append(&child2).unwrap();

        let ql = QueueLoop::new(orb_store.clone(), dep_store, base);
        let result = ql.tick().unwrap();

        assert_eq!(result.roots_completed, 1);
        let updated = orb_store.load_by_id(&parent.id).unwrap().unwrap();
        assert_eq!(updated.phase, Some(OrbPhase::Done));
    }

    #[test]
    fn tick_starts_parent_final_execution_after_children_done() {
        let (_tmp, orb_store, dep_store, base) = setup();

        let mut parent = Orb::new("Parent epic", "Has children and final work")
            .with_type(OrbType::Epic)
            .with_parent_final_work(true);
        parent.phase = Some(OrbPhase::Waiting);
        parent.execution = Some(orbs::orb::ExecutionMeta {
            worker_model: Some("completed-refinement".into()),
            ..Default::default()
        });
        orb_store.append(&parent).unwrap();

        let mut child =
            Orb::new("Child", "Done").with_parent(parent.id.clone(), Some(parent.id.clone()));
        child.status = Some(OrbStatus::Done);
        orb_store.append(&child).unwrap();

        let ql = QueueLoop::new(orb_store.clone(), dep_store, base);
        let result = ql.tick().unwrap();

        assert_eq!(result.roots_completed, 1);
        let updated = orb_store.load_by_id(&parent.id).unwrap().unwrap();
        assert_eq!(updated.phase, Some(OrbPhase::Executing));
        assert!(
            updated.execution.is_none(),
            "the final epic execution must be eligible after child completion"
        );
    }

    #[test]
    fn tick_does_not_complete_root_with_incomplete_children() {
        let (_tmp, orb_store, dep_store, base) = setup();

        let mut parent = Orb::new("Parent epic", "Has children").with_type(OrbType::Epic);
        parent.phase = Some(OrbPhase::Executing); // test setup; skip pipeline walk
        orb_store.append(&parent).unwrap();

        let mut child1 =
            Orb::new("Child 1", "Done").with_parent(parent.id.clone(), Some(parent.id.clone()));
        child1.status = Some(OrbStatus::Done); // test setup
        orb_store.append(&child1).unwrap();

        let child2 = Orb::new("Child 2", "Still pending")
            .with_parent(parent.id.clone(), Some(parent.id.clone()));
        orb_store.append(&child2).unwrap();

        let ql = QueueLoop::new(orb_store.clone(), dep_store, base);
        let result = ql.tick().unwrap();

        assert_eq!(result.roots_completed, 0);
        let updated = orb_store.load_by_id(&parent.id).unwrap().unwrap();
        assert_eq!(updated.phase, Some(OrbPhase::Executing));
    }

    #[test]
    fn tick_completes_task_parent_when_children_done() {
        let (_tmp, orb_store, dep_store, base) = setup();

        let mut parent = Orb::new("Parent task", "Has subtasks");
        parent.set_status(OrbStatus::Active).unwrap();
        orb_store.append(&parent).unwrap();

        let mut child =
            Orb::new("Subtask", "Done").with_parent(parent.id.clone(), Some(parent.id.clone()));
        child.status = Some(OrbStatus::Done); // test setup
        orb_store.append(&child).unwrap();

        let ql = QueueLoop::new(orb_store.clone(), dep_store, base);
        let result = ql.tick().unwrap();

        assert_eq!(result.roots_completed, 1);
        let updated = orb_store.load_by_id(&parent.id).unwrap().unwrap();
        assert_eq!(updated.status, Some(OrbStatus::Done));
    }

    // ── re-evaluation ────────────────────────────────────────────────

    #[test]
    fn tick_reevaluates_waiting_phase_orbs_with_blockers() {
        let (_tmp, orb_store, dep_store, base) = setup();

        let blocker = Orb::new("Blocker", "Not done yet");
        orb_store.append(&blocker).unwrap();

        let mut feature = Orb::new("Blocked feature", "Waiting").with_type(OrbType::Feature);
        feature.phase = Some(OrbPhase::Waiting); // test setup
        orb_store.append(&feature).unwrap();

        // blocker blocks feature
        let edge = DepEdge::new(blocker.id.clone(), feature.id.clone(), EdgeType::Blocks);
        dep_store.add_edge(edge).unwrap();

        let ql = QueueLoop::new(orb_store.clone(), dep_store, base);
        let result = ql.tick().unwrap();

        assert_eq!(result.orbs_reevaluated, 1);
        let updated = orb_store.load_by_id(&feature.id).unwrap().unwrap();
        assert_eq!(updated.phase, Some(OrbPhase::Reevaluating));
    }

    #[test]
    fn tick_does_not_reevaluate_task_type_orbs() {
        let (_tmp, orb_store, dep_store, base) = setup();

        let blocker = Orb::new("Blocker", "Not done");
        let task = Orb::new("Blocked task", "Task type");
        orb_store.append(&blocker).unwrap();
        orb_store.append(&task).unwrap();

        let edge = DepEdge::new(blocker.id.clone(), task.id.clone(), EdgeType::Blocks);
        dep_store.add_edge(edge).unwrap();

        let ql = QueueLoop::new(orb_store, dep_store, base);
        let result = ql.tick().unwrap();

        // Task-type orbs don't get re-evaluated
        assert_eq!(result.orbs_reevaluated, 0);
    }

    #[tokio::test]
    async fn stopped_queue_does_not_admit_ready_dispatches() {
        let (_tmp, orb_store, dep_store, base) = setup();
        let mut task = Orb::new("Queued task", "must not start during shutdown");
        task.set_status(OrbStatus::Active).unwrap();
        orb_store.append(&task).unwrap();

        let ql = QueueLoop::new(orb_store.clone(), dep_store, base);
        ql.stop();
        let worker = crate::worker::process::WorkerConfig {
            command: "this-command-must-not-run".into(),
            args: vec![],
            cwd: None,
            env: vec![],
            model: "mock/stopped".into(),
            system_prompt: String::new(),
            tools: vec![],
            max_iterations: Some(1),
            init_timeout: None,
            send_timeout: None,
            shutdown_timeout: None,
            task_id: None,
            worker_id: None,
            runtime: None,
            routing: None,
        };

        assert_eq!(ql.dispatch_ready_orbs(&worker, 1).await.unwrap(), 0);
        let updated = orb_store.load_by_id(&task.id).unwrap().unwrap();
        assert_eq!(updated.status, Some(OrbStatus::Active));
        assert!(updated.execution.is_none());
    }

    #[tokio::test]
    async fn dispatch_enforces_read_only_tools_for_speccing() {
        let (_tmp, orb_store, dep_store, base) = setup();
        let tools_path = base.join("received-tools.json");
        let worker_path = base.join("capture-tools.sh");
        std::fs::write(
            &worker_path,
            format!(
                r#"while IFS= read -r line; do
  type=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['type'])")
  id=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['id'])")
  case "$type" in
    init) echo "$line" | python3 -c "import sys,json; print(json.dumps(json.load(sys.stdin)['config']['tools']))" > '{}'; echo "{{\"type\":\"init_ok\",\"id\":\"$id\",\"session_id\":\"s\",\"protocol_version\":\"0.3.0\"}}" ;;
    send) echo "{{\"type\":\"result\",\"id\":\"$id\",\"status\":\"ok\",\"response\":\"done\",\"tool_calls_made\":[],\"iterations\":1}}" ;;
    shutdown) echo "{{\"type\":\"shutdown_ok\",\"id\":\"$id\"}}"; exit 0 ;;
  esac
done
"#,
                tools_path.display(),
            ),
        )
        .unwrap();

        let mut feature = Orb::new("Feature", "Design it").with_type(OrbType::Feature);
        feature.set_phase(OrbPhase::Speccing).unwrap();
        orb_store.append(&feature).unwrap();
        let ql = QueueLoop::new(orb_store, dep_store, base.clone());
        let worker = crate::worker::process::WorkerConfig {
            command: "bash".into(),
            args: vec![worker_path.to_string_lossy().into()],
            cwd: Some(base.clone()),
            env: vec![],
            model: "mock/tools".into(),
            system_prompt: String::new(),
            tools: crate::routing::profile::builtin_tools("execute")
                .iter()
                .map(ToString::to_string)
                .collect(),
            max_iterations: Some(1),
            init_timeout: None,
            send_timeout: None,
            shutdown_timeout: None,
            task_id: None,
            worker_id: None,
            runtime: None,
            routing: None,
        };

        assert_eq!(ql.dispatch_ready_orbs(&worker, 1).await.unwrap(), 1);
        let tools: Vec<String> =
            serde_json::from_str(&std::fs::read_to_string(tools_path).unwrap()).unwrap();
        assert_eq!(tools, ["read_file", "glob", "grep"]);
        assert!(
            !base.join("prompts.jsonl").exists(),
            "normal queue runs must not retain full prompt text by default"
        );
    }

    #[tokio::test]
    async fn runtime_decomposition_creates_children_before_parent_advances() {
        let (_tmp, orb_store, dep_store, base) = setup();
        let worker_path = base.join("decompose.sh");
        std::fs::write(
            &worker_path,
            r#"while IFS= read -r line; do
  type=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['type'])")
  id=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['id'])")
  case "$type" in
    init) echo "{\"type\":\"init_ok\",\"id\":\"$id\",\"session_id\":\"s\",\"protocol_version\":\"0.3.0\"}" ;;
    send) echo "{\"type\":\"result\",\"id\":\"$id\",\"status\":\"ok\",\"response\":\"{\\\"subtasks\\\":[{\\\"title\\\":\\\"first\\\",\\\"description\\\":\\\"do first\\\",\\\"order\\\":1},{\\\"title\\\":\\\"second\\\",\\\"description\\\":\\\"do second\\\",\\\"order\\\":2}],\\\"has_parent_final_work\\\":true}\",\"tool_calls_made\":[],\"iterations\":1}" ;;
    shutdown) echo "{\"type\":\"shutdown_ok\",\"id\":\"$id\"}"; exit 0 ;;
  esac
done"#,
        )
        .unwrap();
        let mut feature = Orb::new("Feature", "decompose me").with_type(OrbType::Feature);
        feature.set_phase(OrbPhase::Speccing).unwrap();
        feature.set_phase(OrbPhase::Decomposing).unwrap();
        orb_store.append(&feature).unwrap();
        let ql = QueueLoop::new(orb_store.clone(), dep_store.clone(), base.clone());
        let worker = crate::worker::process::WorkerConfig {
            command: "bash".into(),
            args: vec![worker_path.to_string_lossy().into()],
            cwd: Some(base),
            env: vec![],
            model: "mock/decompose".into(),
            system_prompt: String::new(),
            tools: vec![],
            max_iterations: Some(1),
            init_timeout: None,
            send_timeout: None,
            shutdown_timeout: None,
            task_id: None,
            worker_id: None,
            runtime: None,
            routing: None,
        };
        assert_eq!(ql.dispatch_ready_orbs(&worker, 1).await.unwrap(), 1);
        let parent = orb_store.load_by_id(&feature.id).unwrap().unwrap();
        assert_eq!(parent.phase, Some(OrbPhase::Refining));
        assert!(parent.has_parent_final_work);
        assert_eq!(orb_store.load_children(&feature.id).unwrap().len(), 2);
        assert!(dep_store
            .all_edges()
            .unwrap()
            .iter()
            .any(|edge| edge.edge_type == EdgeType::DependsOn));
    }

    // ── pause/resume ─────────────────────────────────────────────────

    #[test]
    fn pause_makes_tick_return_idle() {
        let (_tmp, orb_store, dep_store, base) = setup();

        let task = Orb::new("Should not execute", "Paused");
        orb_store.append(&task).unwrap();

        let ql = QueueLoop::new(orb_store, dep_store, base);
        ql.pause();
        assert!(ql.is_paused());

        let result = ql.tick().unwrap();
        assert!(result.is_idle());
    }

    #[test]
    fn resume_after_pause_processes_normally() {
        let (_tmp, orb_store, dep_store, base) = setup();

        let task = Orb::new("Execute after resume", "Was paused");
        orb_store.append(&task).unwrap();

        let ql = QueueLoop::new(orb_store.clone(), dep_store, base);

        ql.pause();
        assert!(ql.is_paused());
        let result = ql.tick().unwrap();
        assert!(result.is_idle());

        ql.resume();
        assert!(!ql.is_paused());
        let result = ql.tick().unwrap();
        assert_eq!(result.orbs_executed, 1);
    }

    // ── TickResult counts ────────────────────────────────────────────

    #[test]
    fn tick_result_counts_multiple_actions() {
        let (_tmp, orb_store, dep_store, base) = setup();

        // One pending epic (pipeline start)
        let epic = Orb::new("Epic", "Big").with_type(OrbType::Epic);
        orb_store.append(&epic).unwrap();

        // Two ready tasks (execute)
        let task1 = Orb::new("Task 1", "First");
        let task2 = Orb::new("Task 2", "Second");
        orb_store.append(&task1).unwrap();
        orb_store.append(&task2).unwrap();

        let ql = QueueLoop::new(orb_store, dep_store, base);
        let result = ql.tick().unwrap();

        assert_eq!(result.pipelines_started, 1);
        assert_eq!(result.orbs_executed, 2);
    }

    #[test]
    fn tick_result_is_idle_default() {
        let result = TickResult::default();
        assert!(result.is_idle());
        assert_eq!(result.pipelines_started, 0);
        assert_eq!(result.orbs_executed, 0);
        assert_eq!(result.roots_completed, 0);
        assert_eq!(result.orbs_reevaluated, 0);
    }

    // ── async run with stop ──────────────────────────────────────────

    #[tokio::test]
    async fn run_stops_when_flag_cleared() {
        let (_tmp, orb_store, dep_store, base) = setup();
        let ql = QueueLoop::new(orb_store, dep_store, base);

        let running = ql.running_flag();

        // Stop immediately
        running.store(false, Ordering::SeqCst);

        // run() should return quickly since running is false
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), ql.run()).await;

        assert!(result.is_ok(), "run() should have stopped promptly");
        assert!(result.unwrap().is_ok());
    }
}

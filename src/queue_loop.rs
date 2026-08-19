use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use std::collections::HashMap;

use orbs::dep_store::DepStore;
use orbs::id::OrbId;
use orbs::orb::{Orb, OrbPhase, OrbStatus};
use orbs::orb_store::OrbStore;
use orbs::pipeline::create_pipeline;
use orbs::task::TaskStatus;
use tracing::{debug, instrument};

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
pub struct QueueLoop {
    orb_store: OrbStore,
    dep_store: DepStore,
    base_dir: PathBuf,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    hooks: Option<Arc<crate::hooks::HookSink>>,
    review_config: Option<crate::config::ReviewConfig>,
    prompt_config: Option<crate::config::PromptConfig>,
    tool_policy: Option<crate::routing::profile::PhaseToolPolicy>,
    execution_store: crate::execution::ExecutionStore,
    prompt_store: Option<crate::execution::PromptStore>,
}

impl QueueLoop {
    /// Creates a new `QueueLoop`.
    pub fn new(orb_store: OrbStore, dep_store: DepStore, base_dir: PathBuf) -> Self {
        let execution_path = orb_store
            .path()
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("executions.jsonl");
        Self {
            orb_store,
            dep_store,
            base_dir,
            running: Arc::new(AtomicBool::new(true)),
            paused: Arc::new(AtomicBool::new(false)),
            hooks: None,
            review_config: None,
            prompt_config: None,
            tool_policy: None,
            execution_store: crate::execution::ExecutionStore::new(execution_path),
            prompt_store: None,
        }
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
    #[instrument(name = "queue.tick", skip(self), fields(orb_count = tracing::field::Empty))]
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
    #[instrument(name = "queue.dispatch_ready", skip(self, base_worker_config), fields(model = %base_worker_config.model))]
    pub async fn dispatch_ready_orbs(
        &self,
        base_worker_config: &crate::worker::process::WorkerConfig,
        max_concurrency: usize,
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

        let mut orb_config =
            crate::config::load_config(Some(&self.base_dir)).map_err(std::io::Error::other)?;
        if let Some(review_config) = &self.review_config {
            orb_config.review = review_config.clone();
        }
        let prompt_config = self
            .prompt_config
            .clone()
            .unwrap_or_else(|| orb_config.prompts.clone());
        let prompt_resolver =
            crate::prompt::PromptResolver::from_config(prompt_config, Some(&self.base_dir));

        let semaphore = Arc::new(Semaphore::new(max_concurrency.max(1)));
        let context_orbs = Arc::new(all_orbs);
        let context_edges = Arc::new(all_edges);
        let mut join_set = JoinSet::new();

        for (orb, target) in targets {
            let sem = semaphore.clone();
            let store = self.orb_store.clone();
            let base_wc = base_worker_config.clone();
            let orb_config = orb_config.clone();
            let prompt_resolver = prompt_resolver.clone();
            let tool_policy = self.tool_policy.clone();
            let context_orbs = Arc::clone(&context_orbs);
            let context_edges = Arc::clone(&context_edges);
            let hooks = self.hooks.as_ref().map(Arc::clone);
            let execution_store = self.execution_store.clone();
            let prompt_store = self.prompt_store.clone();
            join_set.spawn(async move {
                let Ok(_permit) = sem.acquire_owned().await else {
                    return Ok(false);
                };
                let context = DispatchContext {
                    orbs: &context_orbs,
                    edges: &context_edges,
                };
                dispatch_one_owned(
                    store,
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
/// Returns true when an orb is nested under a parent waiting for review.
/// Review is a gate for all descendant execution, not merely a label on the
/// parent; checking the full chain also handles nested feature/task trees.
fn blocked_by_parent_review(orb: &Orb, orbs: &[Orb]) -> bool {
    let mut parent_id = orb.parent_id.as_ref();
    while let Some(id) = parent_id {
        let Some(parent) = orbs.iter().find(|candidate| &candidate.id == id) else {
            break;
        };
        if parent.phase == Some(OrbPhase::Review) || parent.status == Some(OrbStatus::Review) {
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

fn optional_debug<T: std::fmt::Debug>(value: Option<T>) -> String {
    value.map_or_else(|| "none".to_string(), |value| format!("{value:?}"))
}

/// Owned-argument version of `dispatch_one`, suitable for `tokio::spawn`.
/// Returns `Ok(true)` when the orb ended at Done, `Ok(false)` otherwise.
struct DispatchContext<'a> {
    orbs: &'a [Orb],
    edges: &'a [orbs::dep::DepEdge],
}

async fn dispatch_one_owned(
    store: OrbStore,
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
    let user = crate::prompt_context::append_task_context(&user, &task_context.text);
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
    let wc = worker_config_for_with_model_config(&orb, &target_base_wc, &system, model_config)
        .map_err(std::io::Error::other)?;
    let effective_system_prompt =
        crate::worker::process::effective_system_prompt(&wc.system_prompt, &wc.tools);
    prompt_context.effective_system_prompt_chars =
        u32::try_from(effective_system_prompt.chars().count()).unwrap_or(u32::MAX);
    tracing::info!(
        orb = %orb.id,
        title = %orb.title,
        target = ?target,
        phase = %optional_debug(orb.phase),
        tools = ?wc.tools,
        "dispatching ready orb",
    );

    let mut outcome = dispatch_orb(&orb, &user, &wc, hooks.as_deref())
        .await
        .map_err(std::io::Error::other)?;
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
    let outcome = crate::worker::dispatcher::with_prompt_metadata(
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
            effective_system_prompt,
            user,
            outcome.prompt_tokens,
            prompt_context.clone(),
        ))?;
    }
    execution_store.append(&crate::execution::ExecutionRecord::from_outcome(
        &orb,
        prompt_category,
        target.tool_policy_key(),
        Some(if tool_policy.is_some() {
            "case_override".into()
        } else {
            "phase_default".into()
        }),
        wc.tools.clone(),
        &outcome,
        Some(prompt_context),
    ))?;

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

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use orbs::dep_store::DepStore;
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
}

impl QueueLoop {
    /// Creates a new `QueueLoop`.
    pub fn new(orb_store: OrbStore, dep_store: DepStore, base_dir: PathBuf) -> Self {
        Self {
            orb_store,
            dep_store,
            base_dir,
            running: Arc::new(AtomicBool::new(true)),
            paused: Arc::new(AtomicBool::new(false)),
            hooks: None,
        }
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
    /// for the phase-orb branch (Waiting → Executing); the task-orb
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
            if orb.orb_type.uses_phase() {
                if orb.phase == Some(OrbPhase::Waiting)
                    && self.try_phase_transition(orb, OrbPhase::Executing).await?
                {
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
        let mut count = 0;
        for orb in orbs {
            if orb.effective_status() == TaskStatus::Done
                || orb.effective_status() == TaskStatus::Failed
                || orb.effective_status() == TaskStatus::Cancelled
            {
                continue;
            }
            let children = self.orb_store.load_children(&orb.id)?;
            if children.is_empty() {
                continue;
            }
            let all_children_done = children
                .iter()
                .all(|c| c.effective_status() == TaskStatus::Done);
            if !all_children_done {
                continue;
            }
            if orb.orb_type.uses_phase() {
                if self.try_phase_transition(orb, OrbPhase::Done).await? {
                    count += 1;
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

            // Only advance Pending task-type orbs to Active
            if orb.orb_type.uses_phase() {
                // Phase-type orbs in Waiting → Executing
                if orb.phase == Some(OrbPhase::Waiting) {
                    let mut updated = orb.clone();
                    updated
                        .set_phase(OrbPhase::Executing)
                        .map_err(std::io::Error::other)?;
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
        let mut count = 0;

        // Find orbs that have children (potential roots)
        for orb in orbs {
            // Skip non-terminal, already-done, or non-phase orbs
            if orb.effective_status() == TaskStatus::Done
                || orb.effective_status() == TaskStatus::Failed
                || orb.effective_status() == TaskStatus::Cancelled
            {
                continue;
            }

            let children = self.orb_store.load_children(&orb.id)?;
            if children.is_empty() {
                continue;
            }

            let all_children_done = children
                .iter()
                .all(|c| c.effective_status() == TaskStatus::Done);

            if all_children_done {
                let mut updated = orb.clone();
                if orb.orb_type.uses_phase() {
                    updated
                        .set_phase(OrbPhase::Done)
                        .map_err(std::io::Error::other)?;
                } else {
                    updated
                        .set_status(OrbStatus::Done)
                        .map_err(std::io::Error::other)?;
                }
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
        let mut targets: Vec<(Orb, DispatchTarget)> = Vec::new();
        for orb in all_orbs {
            if let Some(t) = dispatch_target_for(&orb) {
                targets.push((orb, t));
            }
        }
        if targets.is_empty() {
            return Ok(0);
        }

        let semaphore = Arc::new(Semaphore::new(max_concurrency.max(1)));
        let mut join_set = JoinSet::new();

        for (orb, target) in targets {
            let permit = match semaphore.clone().acquire_owned().await {
                Ok(p) => p,
                Err(_) => continue,
            };
            let store = self.orb_store.clone();
            let base_wc = base_worker_config.clone();
            let hooks = self.hooks.as_ref().map(Arc::clone);
            join_set.spawn(async move {
                let _permit = permit;
                dispatch_one_owned(store, orb, target, &base_wc, hooks).await
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
}

// ── Dispatch helpers (task 60) ───────────────────────────────────

/// What phase / prompt should drive a worker for this orb.
#[derive(Debug)]
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

/// Owned-argument version of dispatch_one, suitable for `tokio::spawn`.
/// Returns `Ok(true)` when the orb ended at Done, `Ok(false)` otherwise.
async fn dispatch_one_owned(
    store: OrbStore,
    mut orb: Orb,
    target: DispatchTarget,
    base_wc: &crate::worker::process::WorkerConfig,
    hooks: Option<Arc<crate::hooks::HookSink>>,
) -> std::io::Result<bool> {
    use crate::worker::dispatcher::{apply_dispatch_outcome, dispatch_orb, worker_config_for};

    let (system, user) = match target {
        DispatchTarget::Speccing => crate::phases::speccing::build_prompt(&orb),
        DispatchTarget::Decomposing => crate::phases::decompose::build_prompt(&orb),
        DispatchTarget::Refining => crate::phases::refinement::build_prompt(&orb),
        DispatchTarget::Reevaluating => crate::phases::re_evaluation::build_prompt(&orb, &[]),
        DispatchTarget::Execute => (
            "You are a task worker. Complete the task in the user message.".to_string(),
            orb.description.clone(),
        ),
    };
    let wc = worker_config_for(&orb, base_wc, &system);

    let outcome = dispatch_orb(&orb, &user, &wc, hooks.as_deref())
        .await
        .map_err(std::io::Error::other)?;

    apply_dispatch_outcome(&mut orb, &outcome).map_err(std::io::Error::other)?;

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

    /// Helper: sets up a temp dir with orb_store, dep_store, and base_dir.
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
        let blocked = all_orbs.iter().find(|o| o.id == task.id).unwrap();
        assert_eq!(blocked.status, Some(OrbStatus::Pending));
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
    fn tick_completes_root_when_all_children_done() {
        let (_tmp, orb_store, dep_store, base) = setup();

        let mut parent = Orb::new("Parent epic", "Has children").with_type(OrbType::Epic);
        parent.phase = Some(OrbPhase::Executing); // test setup; skip pipeline walk
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

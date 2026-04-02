use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use orbs::dep_store::DepStore;
use orbs::orb::{Orb, OrbPhase, OrbStatus};
use orbs::orb_store::OrbStore;
use orbs::pipeline::create_pipeline;
use orbs::task::TaskStatus;

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
        }
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
    pub fn tick(&self) -> std::io::Result<TickResult> {
        if self.paused.load(Ordering::SeqCst) {
            return Ok(TickResult::default());
        }

        let mut result = TickResult::default();
        let all_orbs = self.orb_store.load_all()?;

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
            updated.set_phase(OrbPhase::Speccing);
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
                    updated.set_phase(OrbPhase::Executing);
                    self.orb_store.update(&updated)?;
                    count += 1;
                }
            } else {
                // Task-type orbs in Pending → Active
                if orb.status == Some(OrbStatus::Pending) {
                    let mut updated = orb.clone();
                    updated.set_status(OrbStatus::Active);
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
                    updated.set_phase(OrbPhase::Done);
                } else {
                    updated.set_status(OrbStatus::Done);
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
                updated.set_phase(OrbPhase::Reevaluating);
                self.orb_store.update(&updated)?;
                count += 1;
            }
        }
        Ok(count)
    }

    /// Runs the queue loop until stopped.
    ///
    /// Calls `tick()` in a loop with a short sleep between iterations,
    /// checking the `running` flag each time.
    ///
    /// # Errors
    ///
    /// Returns an IO error if any tick fails.
    pub async fn run(&self) -> std::io::Result<()> {
        while self.running.load(Ordering::SeqCst) {
            self.tick()?;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        Ok(())
    }
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
        epic.set_phase(OrbPhase::Speccing);
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
        feature.set_phase(OrbPhase::Waiting);
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
        parent.set_phase(OrbPhase::Executing);
        orb_store.append(&parent).unwrap();

        let mut child1 =
            Orb::new("Child 1", "First").with_parent(parent.id.clone(), Some(parent.id.clone()));
        child1.set_status(OrbStatus::Done);
        orb_store.append(&child1).unwrap();

        let mut child2 =
            Orb::new("Child 2", "Second").with_parent(parent.id.clone(), Some(parent.id.clone()));
        child2.set_status(OrbStatus::Done);
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
        parent.set_phase(OrbPhase::Executing);
        orb_store.append(&parent).unwrap();

        let mut child1 =
            Orb::new("Child 1", "Done").with_parent(parent.id.clone(), Some(parent.id.clone()));
        child1.set_status(OrbStatus::Done);
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
        parent.set_status(OrbStatus::Active);
        orb_store.append(&parent).unwrap();

        let mut child =
            Orb::new("Subtask", "Done").with_parent(parent.id.clone(), Some(parent.id.clone()));
        child.set_status(OrbStatus::Done);
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
        feature.set_phase(OrbPhase::Waiting);
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

use orbs::dep::EdgeType;
use orbs::dep_store::DepStore;
use orbs::id::OrbId;
use orbs::orb::{Orb, OrbPhase};
use orbs::orb_store::OrbStore;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Result of re-evaluating an orb's upstream dependencies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReEvalResult {
    /// All blocking deps are done — proceed to execution.
    ReadyToExecute,
    /// Some blocking deps are still pending or active.
    StillWaiting { blocking: Vec<OrbId> },
    /// Upstream results changed the landscape — needs another refinement pass.
    NeedsPatching { reason: String },
    /// Human intervention needed (e.g. a blocking dep failed).
    Escalate { reason: String },
}

// ---------------------------------------------------------------------------
// Logic
// ---------------------------------------------------------------------------

/// Examines the orb's blocking dependencies and determines the re-evaluation outcome.
///
/// - All blocking deps Done -> `ReadyToExecute`
/// - Some blocking deps still pending/active -> `StillWaiting` with list
/// - Any blocking dep Failed -> `Escalate` with reason
/// - Otherwise -> `NeedsPatching` (placeholder for future sophistication)
pub fn check_upstream(orb: &Orb, orb_store: &OrbStore, dep_store: &DepStore) -> ReEvalResult {
    // Get all edges where this orb depends on something (DependsOn: from=orb, to=dep)
    // or where something blocks this orb (Blocks: from=blocker, to=orb).
    let edges_from = dep_store.edges_from(&orb.id).unwrap_or_default();
    let edges_to = dep_store.edges_to(&orb.id).unwrap_or_default();

    // Collect blocking dependency IDs:
    // - DependsOn edges: orb depends on edge.to
    // - Blocks edges targeting this orb: edge.from blocks this orb
    let mut blocking_dep_ids: Vec<OrbId> = Vec::new();

    for edge in &edges_from {
        if edge.edge_type == EdgeType::DependsOn {
            blocking_dep_ids.push(edge.to.clone());
        }
    }
    for edge in &edges_to {
        if edge.edge_type == EdgeType::Blocks {
            blocking_dep_ids.push(edge.from.clone());
        }
    }

    // No blocking deps at all — ready to go.
    if blocking_dep_ids.is_empty() {
        return ReEvalResult::ReadyToExecute;
    }

    let mut still_waiting: Vec<OrbId> = Vec::new();
    let mut any_failed = false;
    let mut failed_reason = String::new();

    for dep_id in &blocking_dep_ids {
        let dep_orb = orb_store.load_by_id(dep_id).ok().flatten();

        match dep_orb {
            Some(dep) => {
                // Check phase-based orbs
                if let Some(phase) = dep.phase {
                    match phase {
                        OrbPhase::Done => {} // satisfied
                        OrbPhase::Failed => {
                            any_failed = true;
                            failed_reason = format!("blocking dependency {} is failed", dep.id);
                        }
                        _ => {
                            still_waiting.push(dep_id.clone());
                        }
                    }
                } else if let Some(status) = dep.status {
                    // Check status-based orbs
                    match status {
                        orbs::orb::OrbStatus::Done => {} // satisfied
                        orbs::orb::OrbStatus::Failed => {
                            any_failed = true;
                            failed_reason = format!("blocking dependency {} is failed", dep.id);
                        }
                        _ => {
                            still_waiting.push(dep_id.clone());
                        }
                    }
                } else {
                    // No phase or status — treat as pending
                    still_waiting.push(dep_id.clone());
                }
            }
            None => {
                // Dep not found — escalate
                return ReEvalResult::Escalate {
                    reason: format!("blocking dependency {dep_id} not found"),
                };
            }
        }
    }

    if any_failed {
        return ReEvalResult::Escalate {
            reason: failed_reason,
        };
    }

    if !still_waiting.is_empty() {
        return ReEvalResult::StillWaiting {
            blocking: still_waiting,
        };
    }

    ReEvalResult::ReadyToExecute
}

/// Transitions an orb from Waiting to Reevaluating.
pub fn begin_reevaluation(orb: &mut Orb) {
    orb.set_phase(OrbPhase::Reevaluating);
}

/// Applies the re-evaluation decision, transitioning the orb to the appropriate phase.
///
/// - `ReadyToExecute` -> Executing
/// - `StillWaiting` -> back to Waiting
/// - `NeedsPatching` -> back to Refining (needs another refinement pass)
/// - `Escalate` -> Review (needs human)
pub fn apply_reeval(orb: &mut Orb, result: &ReEvalResult) {
    match result {
        ReEvalResult::ReadyToExecute => orb.set_phase(OrbPhase::Executing),
        ReEvalResult::StillWaiting { .. } => orb.set_phase(OrbPhase::Waiting),
        ReEvalResult::NeedsPatching { .. } => orb.set_phase(OrbPhase::Refining),
        ReEvalResult::Escalate { .. } => orb.set_phase(OrbPhase::Review),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use orbs::dep::DepEdge;
    use orbs::orb::OrbType;

    use super::*;

    fn feature_orb(title: &str) -> Orb {
        Orb::new(title, "test description").with_type(OrbType::Feature)
    }

    fn make_stores() -> (OrbStore, DepStore) {
        let dir = tempfile::tempdir().unwrap();
        let orb_store = OrbStore::new(dir.path().join("orbs.jsonl"));
        let dep_store = DepStore::new(dir.path().join("deps.jsonl"));
        // Leak the tempdir so it lives long enough for all tests
        std::mem::forget(dir);
        (orb_store, dep_store)
    }

    // ── begin_reevaluation ──────────────────────────────────────

    #[test]
    fn begin_reevaluation_transitions_from_waiting() {
        let mut orb = feature_orb("Auth");
        orb.set_phase(OrbPhase::Waiting);

        begin_reevaluation(&mut orb);
        assert_eq!(orb.phase, Some(OrbPhase::Reevaluating));
    }

    // ── apply_reeval ────────────────────────────────────────────

    #[test]
    fn apply_reeval_ready_to_execute_transitions_to_executing() {
        let mut orb = feature_orb("Auth");
        orb.set_phase(OrbPhase::Reevaluating);

        apply_reeval(&mut orb, &ReEvalResult::ReadyToExecute);
        assert_eq!(orb.phase, Some(OrbPhase::Executing));
    }

    #[test]
    fn apply_reeval_still_waiting_transitions_to_waiting() {
        let mut orb = feature_orb("Auth");
        orb.set_phase(OrbPhase::Reevaluating);

        let result = ReEvalResult::StillWaiting {
            blocking: vec![OrbId::from_raw("orb-dep1")],
        };
        apply_reeval(&mut orb, &result);
        assert_eq!(orb.phase, Some(OrbPhase::Waiting));
    }

    #[test]
    fn apply_reeval_needs_patching_transitions_to_refining() {
        let mut orb = feature_orb("Auth");
        orb.set_phase(OrbPhase::Reevaluating);

        let result = ReEvalResult::NeedsPatching {
            reason: "upstream spec changed".into(),
        };
        apply_reeval(&mut orb, &result);
        assert_eq!(orb.phase, Some(OrbPhase::Refining));
    }

    #[test]
    fn apply_reeval_escalate_transitions_to_review() {
        let mut orb = feature_orb("Auth");
        orb.set_phase(OrbPhase::Reevaluating);

        let result = ReEvalResult::Escalate {
            reason: "dep failed".into(),
        };
        apply_reeval(&mut orb, &result);
        assert_eq!(orb.phase, Some(OrbPhase::Review));
    }

    // ── check_upstream: no deps ─────────────────────────────────

    #[test]
    fn check_upstream_no_deps_returns_ready() {
        let (orb_store, dep_store) = make_stores();
        let orb = feature_orb("Auth");
        orb_store.append(&orb).unwrap();

        let result = check_upstream(&orb, &orb_store, &dep_store);
        assert_eq!(result, ReEvalResult::ReadyToExecute);
    }

    // ── check_upstream: all deps done ───────────────────────────

    #[test]
    fn check_upstream_all_deps_done_returns_ready() {
        let (orb_store, dep_store) = make_stores();

        let mut dep1 = feature_orb("Dep 1");
        dep1.set_phase(OrbPhase::Done);
        orb_store.append(&dep1).unwrap();

        let mut dep2 = feature_orb("Dep 2");
        dep2.set_phase(OrbPhase::Done);
        orb_store.append(&dep2).unwrap();

        let mut orb = feature_orb("Auth");
        orb.set_phase(OrbPhase::Waiting);
        orb_store.append(&orb).unwrap();

        // orb depends on dep1 and dep2
        dep_store
            .add_edge(DepEdge::new(
                orb.id.clone(),
                dep1.id.clone(),
                EdgeType::DependsOn,
            ))
            .unwrap();
        dep_store
            .add_edge(DepEdge::new(
                orb.id.clone(),
                dep2.id.clone(),
                EdgeType::DependsOn,
            ))
            .unwrap();

        let result = check_upstream(&orb, &orb_store, &dep_store);
        assert_eq!(result, ReEvalResult::ReadyToExecute);
    }

    // ── check_upstream: some deps pending ───────────────────────

    #[test]
    fn check_upstream_some_deps_pending_returns_still_waiting() {
        let (orb_store, dep_store) = make_stores();

        let mut dep_done = feature_orb("Dep Done");
        dep_done.set_phase(OrbPhase::Done);
        orb_store.append(&dep_done).unwrap();

        let mut dep_active = feature_orb("Dep Active");
        dep_active.set_phase(OrbPhase::Executing);
        orb_store.append(&dep_active).unwrap();

        let mut orb = feature_orb("Auth");
        orb.set_phase(OrbPhase::Waiting);
        orb_store.append(&orb).unwrap();

        dep_store
            .add_edge(DepEdge::new(
                orb.id.clone(),
                dep_done.id.clone(),
                EdgeType::DependsOn,
            ))
            .unwrap();
        dep_store
            .add_edge(DepEdge::new(
                orb.id.clone(),
                dep_active.id.clone(),
                EdgeType::DependsOn,
            ))
            .unwrap();

        let result = check_upstream(&orb, &orb_store, &dep_store);
        match &result {
            ReEvalResult::StillWaiting { blocking } => {
                assert_eq!(blocking.len(), 1);
                assert_eq!(blocking[0], dep_active.id);
            }
            other => panic!("expected StillWaiting, got {other:?}"),
        }
    }

    // ── check_upstream: failed dep ──────────────────────────────

    #[test]
    fn check_upstream_failed_dep_returns_escalate() {
        let (orb_store, dep_store) = make_stores();

        let mut dep_failed = feature_orb("Dep Failed");
        dep_failed.set_phase(OrbPhase::Failed);
        orb_store.append(&dep_failed).unwrap();

        let mut orb = feature_orb("Auth");
        orb.set_phase(OrbPhase::Waiting);
        orb_store.append(&orb).unwrap();

        dep_store
            .add_edge(DepEdge::new(
                orb.id.clone(),
                dep_failed.id.clone(),
                EdgeType::DependsOn,
            ))
            .unwrap();

        let result = check_upstream(&orb, &orb_store, &dep_store);
        match &result {
            ReEvalResult::Escalate { reason } => {
                assert!(reason.contains("failed"));
                assert!(reason.contains(&dep_failed.id.to_string()));
            }
            other => panic!("expected Escalate, got {other:?}"),
        }
    }

    // ── check_upstream: blocks edge type ────────────────────────

    #[test]
    fn check_upstream_blocks_edge_detected() {
        let (orb_store, dep_store) = make_stores();

        let mut blocker = feature_orb("Blocker");
        blocker.set_phase(OrbPhase::Executing);
        orb_store.append(&blocker).unwrap();

        let mut orb = feature_orb("Auth");
        orb.set_phase(OrbPhase::Waiting);
        orb_store.append(&orb).unwrap();

        // blocker blocks orb (from=blocker, to=orb)
        dep_store
            .add_edge(DepEdge::new(
                blocker.id.clone(),
                orb.id.clone(),
                EdgeType::Blocks,
            ))
            .unwrap();

        let result = check_upstream(&orb, &orb_store, &dep_store);
        match &result {
            ReEvalResult::StillWaiting { blocking } => {
                assert_eq!(blocking.len(), 1);
                assert_eq!(blocking[0], blocker.id);
            }
            other => panic!("expected StillWaiting, got {other:?}"),
        }
    }

    // ── check_upstream: missing dep ─────────────────────────────

    #[test]
    fn check_upstream_missing_dep_returns_escalate() {
        let (orb_store, dep_store) = make_stores();

        let mut orb = feature_orb("Auth");
        orb.set_phase(OrbPhase::Waiting);
        orb_store.append(&orb).unwrap();

        // Dependency on a non-existent orb
        dep_store
            .add_edge(DepEdge::new(
                orb.id.clone(),
                OrbId::from_raw("orb-ghost"),
                EdgeType::DependsOn,
            ))
            .unwrap();

        let result = check_upstream(&orb, &orb_store, &dep_store);
        match &result {
            ReEvalResult::Escalate { reason } => {
                assert!(reason.contains("not found"));
                assert!(reason.contains("orb-ghost"));
            }
            other => panic!("expected Escalate, got {other:?}"),
        }
    }

    // ── check_upstream: status-based deps ───────────────────────

    #[test]
    fn check_upstream_status_based_dep_done() {
        let (orb_store, dep_store) = make_stores();

        // Task-type dep (uses status, not phase)
        let mut dep_task = Orb::new("Task dep", "a task");
        dep_task.set_status(orbs::orb::OrbStatus::Done);
        orb_store.append(&dep_task).unwrap();

        let mut orb = feature_orb("Auth");
        orb.set_phase(OrbPhase::Waiting);
        orb_store.append(&orb).unwrap();

        dep_store
            .add_edge(DepEdge::new(
                orb.id.clone(),
                dep_task.id.clone(),
                EdgeType::DependsOn,
            ))
            .unwrap();

        let result = check_upstream(&orb, &orb_store, &dep_store);
        assert_eq!(result, ReEvalResult::ReadyToExecute);
    }

    #[test]
    fn check_upstream_status_based_dep_failed() {
        let (orb_store, dep_store) = make_stores();

        let mut dep_task = Orb::new("Task dep", "a task");
        dep_task.set_status(orbs::orb::OrbStatus::Failed);
        orb_store.append(&dep_task).unwrap();

        let mut orb = feature_orb("Auth");
        orb.set_phase(OrbPhase::Waiting);
        orb_store.append(&orb).unwrap();

        dep_store
            .add_edge(DepEdge::new(
                orb.id.clone(),
                dep_task.id.clone(),
                EdgeType::DependsOn,
            ))
            .unwrap();

        let result = check_upstream(&orb, &orb_store, &dep_store);
        match &result {
            ReEvalResult::Escalate { reason } => {
                assert!(reason.contains("failed"));
            }
            other => panic!("expected Escalate, got {other:?}"),
        }
    }

    // ── non-blocking deps ignored ───────────────────────────────

    #[test]
    fn check_upstream_related_edges_ignored() {
        let (orb_store, dep_store) = make_stores();

        // A related orb that is still executing — should NOT block
        let mut related = feature_orb("Related");
        related.set_phase(OrbPhase::Executing);
        orb_store.append(&related).unwrap();

        let mut orb = feature_orb("Auth");
        orb.set_phase(OrbPhase::Waiting);
        orb_store.append(&orb).unwrap();

        dep_store
            .add_edge(DepEdge::new(
                orb.id.clone(),
                related.id.clone(),
                EdgeType::Related,
            ))
            .unwrap();

        let result = check_upstream(&orb, &orb_store, &dep_store);
        assert_eq!(result, ReEvalResult::ReadyToExecute);
    }

    // ── end-to-end flow ─────────────────────────────────────────

    #[test]
    fn full_reevaluation_flow_deps_done() {
        let (orb_store, dep_store) = make_stores();

        let mut dep = feature_orb("Dep");
        dep.set_phase(OrbPhase::Done);
        orb_store.append(&dep).unwrap();

        let mut orb = feature_orb("Auth");
        orb.set_phase(OrbPhase::Waiting);
        orb_store.append(&orb).unwrap();

        dep_store
            .add_edge(DepEdge::new(
                orb.id.clone(),
                dep.id.clone(),
                EdgeType::DependsOn,
            ))
            .unwrap();

        // 1. Begin re-evaluation
        begin_reevaluation(&mut orb);
        assert_eq!(orb.phase, Some(OrbPhase::Reevaluating));

        // 2. Check upstream
        let result = check_upstream(&orb, &orb_store, &dep_store);
        assert_eq!(result, ReEvalResult::ReadyToExecute);

        // 3. Apply result
        apply_reeval(&mut orb, &result);
        assert_eq!(orb.phase, Some(OrbPhase::Executing));
    }

    #[test]
    fn full_reevaluation_flow_deps_not_done() {
        let (orb_store, dep_store) = make_stores();

        let mut dep = feature_orb("Dep");
        dep.set_phase(OrbPhase::Executing);
        orb_store.append(&dep).unwrap();

        let mut orb = feature_orb("Auth");
        orb.set_phase(OrbPhase::Waiting);
        orb_store.append(&orb).unwrap();

        dep_store
            .add_edge(DepEdge::new(
                orb.id.clone(),
                dep.id.clone(),
                EdgeType::DependsOn,
            ))
            .unwrap();

        // 1. Begin re-evaluation
        begin_reevaluation(&mut orb);
        assert_eq!(orb.phase, Some(OrbPhase::Reevaluating));

        // 2. Check upstream — still waiting
        let result = check_upstream(&orb, &orb_store, &dep_store);
        match &result {
            ReEvalResult::StillWaiting { blocking } => {
                assert_eq!(blocking.len(), 1);
            }
            other => panic!("expected StillWaiting, got {other:?}"),
        }

        // 3. Apply result — back to waiting
        apply_reeval(&mut orb, &result);
        assert_eq!(orb.phase, Some(OrbPhase::Waiting));
    }

    #[test]
    fn full_reevaluation_flow_dep_failed_escalates() {
        let (orb_store, dep_store) = make_stores();

        let mut dep = feature_orb("Dep");
        dep.set_phase(OrbPhase::Failed);
        orb_store.append(&dep).unwrap();

        let mut orb = feature_orb("Auth");
        orb.set_phase(OrbPhase::Waiting);
        orb_store.append(&orb).unwrap();

        dep_store
            .add_edge(DepEdge::new(
                orb.id.clone(),
                dep.id.clone(),
                EdgeType::DependsOn,
            ))
            .unwrap();

        begin_reevaluation(&mut orb);
        let result = check_upstream(&orb, &orb_store, &dep_store);

        match &result {
            ReEvalResult::Escalate { .. } => {}
            other => panic!("expected Escalate, got {other:?}"),
        }

        apply_reeval(&mut orb, &result);
        assert_eq!(orb.phase, Some(OrbPhase::Review));
    }
}

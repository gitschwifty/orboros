use std::path::PathBuf;

use orbs::dep::{DepEdge, EdgeType};
use orbs::dep_store::DepStore;
use orbs::orb::{Orb, OrbPhase, OrbType};
use orbs::orb_store::OrbStore;
use orbs::pipeline::{self, PipelineDir};

/// Result of decomposing a parent orb into children.
#[derive(Debug)]
pub struct DecomposeResult {
    /// Child orbs created from the parent.
    pub children: Vec<Orb>,
    /// Dependency edges (parent/child + ordering edges).
    pub edges: Vec<DepEdge>,
}

/// Decomposes a parent orb (epic/feature in Decomposing phase) into child orbs.
///
/// Creates child orbs with hierarchical IDs (`parent.id.child(N)`), sets
/// `parent_id` and `root_id`, and produces parent/child + ordering dep edges.
///
/// For now, this is a stub: it splits the parent's description by lines
/// (or creates placeholder children). Real LLM decomposition comes later.
///
/// # Errors
///
/// Returns an error if the parent is not in the Decomposing phase.
pub fn decompose_orb(
    parent: &Orb,
    _store: &OrbStore,
    _dep_store: &DepStore,
) -> anyhow::Result<DecomposeResult> {
    anyhow::ensure!(
        parent.phase == Some(OrbPhase::Decomposing),
        "parent orb must be in Decomposing phase, got {:?}",
        parent.phase
    );

    let root_id = parent.root_id.clone().unwrap_or_else(|| parent.id.clone());

    // Stub decomposition: split description into subtasks by non-empty lines,
    // or create a single placeholder child if there's nothing to split.
    let lines: Vec<&str> = parent
        .description
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();

    let subtask_specs: Vec<(String, String)> = if lines.len() > 1 {
        lines
            .iter()
            .enumerate()
            .map(|(i, line)| {
                (
                    format!("{} - subtask {}", parent.title, i + 1),
                    (*line).to_string(),
                )
            })
            .collect()
    } else {
        vec![(
            format!("{} - implementation", parent.title),
            parent.description.clone(),
        )]
    };

    let mut children = Vec::with_capacity(subtask_specs.len());
    let mut edges = Vec::new();

    for (i, (title, description)) in subtask_specs.into_iter().enumerate() {
        let child_id = parent.id.child(u32::try_from(i + 1).unwrap_or(u32::MAX));

        let mut child = Orb::new(&title, &description).with_type(OrbType::Task);
        // Override the auto-generated ID with the hierarchical child ID
        child.id = child_id.clone();
        child.parent_id = Some(parent.id.clone());
        child.root_id = Some(root_id.clone());

        // Parent→Child edge
        edges.push(DepEdge::new(
            parent.id.clone(),
            child_id.clone(),
            EdgeType::Parent,
        ));

        // Child→Parent edge (reverse)
        edges.push(DepEdge::new(
            child_id.clone(),
            parent.id.clone(),
            EdgeType::Child,
        ));

        // Sequential ordering: child N+1 depends_on child N
        if i > 0 {
            let prev_id = parent.id.child(u32::try_from(i).unwrap_or(u32::MAX));
            edges.push(DepEdge::new(child_id.clone(), prev_id, EdgeType::DependsOn));
        }

        children.push(child);
    }

    Ok(DecomposeResult { children, edges })
}

/// Persists decomposition results: appends children to the orb store and
/// edges to the dep store.
///
/// # Errors
///
/// Returns an error if writing to stores fails.
pub fn apply_decomposition(
    result: &DecomposeResult,
    store: &OrbStore,
    dep_store: &DepStore,
) -> anyhow::Result<()> {
    for child in &result.children {
        store
            .append(child)
            .map_err(|e| anyhow::anyhow!("failed to append child orb: {e}"))?;
    }

    for edge in &result.edges {
        dep_store
            .add_edge(edge.clone())
            .map_err(|e| anyhow::anyhow!("failed to add dep edge: {e}"))?;
    }

    Ok(())
}

/// Takes a snapshot of the current pipeline state into `snapshots/decomposition/`.
///
/// # Errors
///
/// Returns an error if the snapshot operation fails.
pub fn snapshot_decomposition(pipeline_dir: &PipelineDir) -> anyhow::Result<PathBuf> {
    pipeline::snapshot(pipeline_dir, "decomposition")
        .map_err(|e| anyhow::anyhow!("failed to snapshot decomposition: {e}"))
}

/// Transitions an orb from Speccing to Decomposing.
///
/// Returns `false` if the orb is not in the Speccing phase.
pub fn begin_decomposing(orb: &mut Orb) -> bool {
    if orb.phase != Some(OrbPhase::Speccing) {
        return false;
    }
    orb.set_phase(OrbPhase::Decomposing);
    true
}

/// Transitions an orb from Decomposing to Refining.
///
/// Returns `false` if the orb is not in the Decomposing phase.
pub fn finish_decomposing(orb: &mut Orb) -> bool {
    if orb.phase != Some(OrbPhase::Decomposing) {
        return false;
    }
    orb.set_phase(OrbPhase::Refining);
    true
}

#[cfg(test)]
mod tests {
    use orbs::id::OrbId;

    use super::*;

    fn feature_orb(title: &str, desc: &str) -> Orb {
        let mut orb = Orb::new(title, desc).with_type(OrbType::Feature);
        orb.id = OrbId::from_raw("orb-feat1");
        orb
    }

    fn tmp_stores() -> (tempfile::TempDir, OrbStore, DepStore) {
        let dir = tempfile::tempdir().unwrap();
        let orb_store = OrbStore::new(dir.path().join("orbs.jsonl"));
        let dep_store = DepStore::new(dir.path().join("deps.jsonl"));
        (dir, orb_store, dep_store)
    }

    // ── phase transitions ────────────────────────────────────

    #[test]
    fn begin_decomposing_from_speccing() {
        let mut orb = feature_orb("Auth", "Implement auth");
        orb.set_phase(OrbPhase::Speccing);

        assert!(begin_decomposing(&mut orb));
        assert_eq!(orb.phase, Some(OrbPhase::Decomposing));
    }

    #[test]
    fn begin_decomposing_from_non_speccing_fails() {
        let mut orb = feature_orb("Auth", "Implement auth");
        // Phase is Pending (default for feature)
        assert!(!begin_decomposing(&mut orb));
        assert_eq!(orb.phase, Some(OrbPhase::Pending));
    }

    #[test]
    fn finish_decomposing_transitions_to_refining() {
        let mut orb = feature_orb("Auth", "Implement auth");
        orb.set_phase(OrbPhase::Decomposing);

        assert!(finish_decomposing(&mut orb));
        assert_eq!(orb.phase, Some(OrbPhase::Refining));
    }

    #[test]
    fn finish_decomposing_from_non_decomposing_fails() {
        let mut orb = feature_orb("Auth", "Implement auth");
        orb.set_phase(OrbPhase::Speccing);

        assert!(!finish_decomposing(&mut orb));
        assert_eq!(orb.phase, Some(OrbPhase::Speccing));
    }

    // ── decompose_orb ────────────────────────────────────────

    #[test]
    fn decompose_rejects_non_decomposing_phase() {
        let (_dir, store, dep_store) = tmp_stores();
        let orb = feature_orb("Auth", "Implement auth");
        // Phase is Pending, not Decomposing

        let result = decompose_orb(&orb, &store, &dep_store);
        assert!(result.is_err());
    }

    #[test]
    fn decompose_creates_children_with_hierarchical_ids() {
        let (_dir, store, dep_store) = tmp_stores();
        let mut orb = feature_orb("Auth", "Step one\nStep two\nStep three");
        orb.set_phase(OrbPhase::Decomposing);

        let result = decompose_orb(&orb, &store, &dep_store).unwrap();

        assert_eq!(result.children.len(), 3);
        assert_eq!(result.children[0].id, OrbId::from_raw("orb-feat1.1"));
        assert_eq!(result.children[1].id, OrbId::from_raw("orb-feat1.2"));
        assert_eq!(result.children[2].id, OrbId::from_raw("orb-feat1.3"));
    }

    #[test]
    fn decompose_sets_parent_and_root_ids() {
        let (_dir, store, dep_store) = tmp_stores();
        let mut orb = feature_orb("Auth", "Step one\nStep two");
        orb.set_phase(OrbPhase::Decomposing);

        let result = decompose_orb(&orb, &store, &dep_store).unwrap();

        for child in &result.children {
            assert_eq!(child.parent_id, Some(OrbId::from_raw("orb-feat1")));
            // root_id falls back to parent's own ID when root_id is None
            assert_eq!(child.root_id, Some(OrbId::from_raw("orb-feat1")));
        }
    }

    #[test]
    fn decompose_propagates_explicit_root_id() {
        let (_dir, store, dep_store) = tmp_stores();
        let mut orb = feature_orb("Auth", "Step one\nStep two");
        orb.root_id = Some(OrbId::from_raw("orb-epic1"));
        orb.set_phase(OrbPhase::Decomposing);

        let result = decompose_orb(&orb, &store, &dep_store).unwrap();

        for child in &result.children {
            assert_eq!(
                child.root_id,
                Some(OrbId::from_raw("orb-epic1")),
                "child should inherit the parent's explicit root_id"
            );
        }
    }

    #[test]
    fn decompose_creates_parent_child_edges() {
        let (_dir, store, dep_store) = tmp_stores();
        let mut orb = feature_orb("Auth", "Step one\nStep two");
        orb.set_phase(OrbPhase::Decomposing);

        let result = decompose_orb(&orb, &store, &dep_store).unwrap();

        // Should have Parent edges (parent→child) and Child edges (child→parent)
        let parent_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::Parent)
            .collect();
        let child_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::Child)
            .collect();

        assert_eq!(parent_edges.len(), 2);
        assert_eq!(child_edges.len(), 2);

        // Parent→Child edges: from=parent, to=child
        for edge in &parent_edges {
            assert_eq!(edge.from, OrbId::from_raw("orb-feat1"));
        }
        // Child→Parent edges: from=child, to=parent
        for edge in &child_edges {
            assert_eq!(edge.to, OrbId::from_raw("orb-feat1"));
        }
    }

    #[test]
    fn decompose_creates_sequential_ordering_edges() {
        let (_dir, store, dep_store) = tmp_stores();
        let mut orb = feature_orb("Auth", "Step one\nStep two\nStep three");
        orb.set_phase(OrbPhase::Decomposing);

        let result = decompose_orb(&orb, &store, &dep_store).unwrap();

        let depends_on_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::DependsOn)
            .collect();

        // child2 depends_on child1, child3 depends_on child2
        assert_eq!(depends_on_edges.len(), 2);

        // child2 (orb-feat1.2) depends_on child1 (orb-feat1.1)
        assert!(depends_on_edges
            .iter()
            .any(|e| e.from == OrbId::from_raw("orb-feat1.2")
                && e.to == OrbId::from_raw("orb-feat1.1")));

        // child3 (orb-feat1.3) depends_on child2 (orb-feat1.2)
        assert!(depends_on_edges
            .iter()
            .any(|e| e.from == OrbId::from_raw("orb-feat1.3")
                && e.to == OrbId::from_raw("orb-feat1.2")));
    }

    #[test]
    fn decompose_single_line_creates_one_child() {
        let (_dir, store, dep_store) = tmp_stores();
        let mut orb = feature_orb("Auth", "Implement the whole thing");
        orb.set_phase(OrbPhase::Decomposing);

        let result = decompose_orb(&orb, &store, &dep_store).unwrap();

        assert_eq!(result.children.len(), 1);
        assert_eq!(result.children[0].id, OrbId::from_raw("orb-feat1.1"));

        // No depends_on edges when there's only one child
        let depends_on: Vec<_> = result
            .edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::DependsOn)
            .collect();
        assert!(depends_on.is_empty());
    }

    // ── apply_decomposition ──────────────────────────────────

    #[test]
    fn apply_decomposition_persists_children_and_edges() {
        let (_dir, store, dep_store) = tmp_stores();
        let mut orb = feature_orb("Auth", "Step one\nStep two");
        orb.set_phase(OrbPhase::Decomposing);

        let result = decompose_orb(&orb, &store, &dep_store).unwrap();
        apply_decomposition(&result, &store, &dep_store).unwrap();

        // Children should be in the orb store
        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 2);

        // Edges should be in the dep store
        let edges = dep_store.all_edges().unwrap();
        assert!(!edges.is_empty());
    }

    #[test]
    fn apply_decomposition_children_loadable_by_parent() {
        let (_dir, store, dep_store) = tmp_stores();
        let mut orb = feature_orb("Auth", "Step one\nStep two\nStep three");
        orb.set_phase(OrbPhase::Decomposing);

        let result = decompose_orb(&orb, &store, &dep_store).unwrap();
        apply_decomposition(&result, &store, &dep_store).unwrap();

        let children = store.load_children(&OrbId::from_raw("orb-feat1")).unwrap();
        assert_eq!(children.len(), 3);
    }

    // ── snapshot_decomposition ───────────────────────────────

    #[test]
    fn snapshot_decomposition_creates_snapshot_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let orb = Orb::new("Snap test", "Test snapshotting").with_type(OrbType::Feature);
        let pipeline_dir = pipeline::create_pipeline(tmp.path(), &orb).unwrap();

        // Add some data
        let store = pipeline_dir.orb_store();
        store.append(&orb).unwrap();

        let snap_path = snapshot_decomposition(&pipeline_dir).unwrap();

        assert!(snap_path.exists());
        assert!(snap_path.join("orbs.jsonl").exists());
        assert!(
            snap_path
                .to_str()
                .unwrap()
                .contains("snapshots/decomposition"),
            "snapshot should be in snapshots/decomposition/"
        );
    }

    // ── end-to-end flow ──────────────────────────────────────

    #[test]
    fn full_decomposition_flow() {
        let (_dir, store, dep_store) = tmp_stores();
        let mut orb = feature_orb("Auth flow", "Design auth\nImplement login\nAdd tests");

        // 1. Start in Speccing, transition to Decomposing
        orb.set_phase(OrbPhase::Speccing);
        assert!(begin_decomposing(&mut orb));
        assert_eq!(orb.phase, Some(OrbPhase::Decomposing));

        // 2. Decompose
        let result = decompose_orb(&orb, &store, &dep_store).unwrap();
        assert_eq!(result.children.len(), 3);

        // 3. Apply
        apply_decomposition(&result, &store, &dep_store).unwrap();

        // 4. Verify children in store
        let children = store.load_children(&OrbId::from_raw("orb-feat1")).unwrap();
        assert_eq!(children.len(), 3);

        // 5. Verify edges
        let edges = dep_store.all_edges().unwrap();
        // 3 parent + 3 child + 2 depends_on = 8
        assert_eq!(edges.len(), 8);

        // 6. Transition to Refining
        assert!(finish_decomposing(&mut orb));
        assert_eq!(orb.phase, Some(OrbPhase::Refining));
    }
}

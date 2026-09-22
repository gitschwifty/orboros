use std::fmt::Write as _;
use std::path::PathBuf;

use orbs::dep::{DepEdge, EdgeType};
use orbs::dep_store::DepStore;
use orbs::id::OrbId;
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
    let parent_id = result
        .children
        .first()
        .and_then(|child| child.parent_id.as_ref())
        .ok_or_else(|| anyhow::anyhow!("decomposition must contain parented children"))?;
    let existing = store.load_all()?;
    let existing_edges = dep_store.all_edges()?;
    for child in &result.children {
        anyhow::ensure!(
            child.parent_id.as_ref() == Some(parent_id),
            "mixed decomposition parents"
        );
        if let Some(prior) = existing.iter().find(|prior| prior.id == child.id) {
            anyhow::ensure!(
                prior.parent_id == child.parent_id && prior.root_id == child.root_id,
                "child {} already belongs to another hierarchy",
                child.id
            );
        }
    }
    for prior in existing
        .iter()
        .filter(|orb| orb.parent_id.as_ref() == Some(parent_id))
    {
        anyhow::ensure!(
            result.children.iter().any(|child| child.id == prior.id),
            "decomposition would orphan existing child {}; reconcile obsolete work explicitly",
            prior.id
        );
    }
    for edge in &existing_edges {
        let internal = result.children.iter().any(|child| child.id == edge.from)
            && result.children.iter().any(|child| child.id == edge.to);
        if internal && edge.edge_type.is_blocking() {
            anyhow::ensure!(
                result.edges.iter().any(|planned| same_edge(edge, planned)),
                "decomposition conflicts with existing ordering {} -> {}; reconcile explicitly",
                edge.from,
                edge.to
            );
        }
    }
    persist_plan_identity(result, store, parent_id)?;
    for child in &result.children {
        // Existing records are authoritative, including operator edits and
        // terminal results. Replay must never append a fresh Pending version.
        if !existing.iter().any(|prior| prior.id == child.id) {
            store
                .append(child)
                .map_err(|e| anyhow::anyhow!("failed to append child orb: {e}"))?;
        }
    }
    for edge in &result.edges {
        if !existing_edges.iter().any(|prior| same_edge(prior, edge)) {
            dep_store
                .add_edge(edge.clone())
                .map_err(|e| anyhow::anyhow!("failed to add dep edge: {e}"))?;
        }
    }

    Ok(())
}

fn same_edge(left: &DepEdge, right: &DepEdge) -> bool {
    left.from == right.from && left.to == right.to && left.edge_type == right.edge_type
}

/// Pin the accepted plan before materialization. A torn file blocks recovery
/// rather than silently treating a different plan as the original generation.
fn persist_plan_identity(
    result: &DecomposeResult,
    store: &OrbStore,
    parent: &OrbId,
) -> anyhow::Result<()> {
    use sha2::{Digest, Sha256};
    use std::io::Write;

    let base = store
        .path()
        .parent()
        .ok_or_else(|| anyhow::anyhow!("orb store has no parent"))?;
    let directory = base.join("decomposition-plans");
    std::fs::create_dir_all(&directory)?;
    std::fs::File::open(base)?.sync_all()?;
    let path = directory.join(format!(
        "{:x}.json",
        Sha256::digest(parent.to_string().as_bytes())
    ));
    let plan = serde_json::json!({
        "children": result.children.iter().map(|child| serde_json::json!({
            "id": child.id, "parent_id": child.parent_id, "root_id": child.root_id,
            "title": child.title, "description": child.description,
            "preferred_model": child.preferred_model,
        })).collect::<Vec<_>>(),
        "edges": result.edges.iter().map(|edge| serde_json::json!({
            "from": edge.from, "to": edge.to, "type": edge.edge_type,
        })).collect::<Vec<_>>(),
    });
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            serde_json::to_writer(&mut file, &plan)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            std::fs::File::open(&directory)?.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let prior: serde_json::Value = serde_json::from_reader(std::fs::File::open(&path)?)?;
            anyhow::ensure!(prior == plan,
                "decomposition plan changed for {parent}; explicit generation reconciliation required");
        }
        Err(error) => return Err(error.into()),
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
    orb.set_phase(OrbPhase::Decomposing).is_ok()
}

/// Transitions an orb from Decomposing to Refining.
///
/// Returns `false` if the orb is not in the Decomposing phase.
pub fn finish_decomposing(orb: &mut Orb) -> bool {
    if orb.phase != Some(OrbPhase::Decomposing) {
        return false;
    }
    orb.set_phase(OrbPhase::Refining).is_ok()
}

// ── Worker-dispatch prompt builder (task 60) ─────────────────────

/// A single subtask the worker proposes during decomposition.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct DecomposedSubtask {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub depends_on: Option<Vec<String>>,
    pub title: String,
    pub description: String,
    /// Execution order group. Subtasks with the same order may run
    /// in parallel; smaller numbers run first.
    #[serde(default = "default_order")]
    pub order: u32,
    /// Optional approved catalog key selected by the coordinator for this
    /// child. This is stored on the child as `preferred_model`.
    #[serde(default)]
    pub model_option: Option<String>,
    /// Brief explanation of why the selected catalog option fits the work.
    /// It guides coordinator output without becoming persisted execution data.
    #[serde(default)]
    pub model_reason: Option<String>,
}

fn default_order() -> u32 {
    1
}

/// Plan parsed from a decompose worker's response.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct DecompositionPlan {
    pub subtasks: Vec<DecomposedSubtask>,
    /// Whether the parent needs a worker pass after all children complete.
    #[serde(default)]
    pub has_parent_final_work: bool,
}

/// Returns `(system, user)` prompts for the decomposition worker.
/// System prompt locks the output to strict JSON with a `subtasks`
/// array of objects.
#[must_use]
pub fn build_prompt(orb: &Orb, models: &crate::config::ModelConfig) -> (String, String) {
    let mut system = "You are a task decomposer. Break the work below into a small set of \
ordered subtasks. Respond with exactly one JSON object — no surrounding prose, \
no code fences — in this shape:\n\
  {\"subtasks\": [\n\
    {\"title\": \"<short subtask title>\", \"description\": \"<what to do>\", \"order\": 1, \"model_option\": \"<optional approved key>\", \"model_reason\": \"<brief reason>\"},\n\
    ...\n\
  ], \"has_parent_final_work\": false}\n\
Every subtask must include a unique nonempty `id` and an explicit `depends_on` \
array of local IDs, including [] for independent work. These fields override \
legacy `order`. Describe the concrete artifact consumed from each prerequisite \
in Inputs and keep titles scoped to the child boundary. Aim for 2-6 subtasks. Avoid trivial \
one-line subtasks; each should be a meaningful unit of work. Set \
`has_parent_final_work` to true only when the parent must perform its own \
synthesis or verification after the children finish.\
\
Scope each child for one focused worker. The child `description` must begin \
with the actual concrete task, not with planning metadata. Add the following \
sections only when they provide useful information; omit any section that is \
not needed rather than filling it with boilerplate:\
- Scope: the exact behavior or files this child owns.\
- Inputs: existing files, interfaces, or completed sibling work it may rely on.\
- Outputs: the files, symbols, or observable behavior it must produce.\
- Deferred work: work intentionally left to later children or the parent; use \
  this only when the boundary would otherwise be ambiguous.\
- Verification: how this child can tell its own work is correct, including the \
  narrowest useful command or inspection.\
- Expected intermediate failures: tests or checks that may still fail because \
  later work is not complete; include this only when applicable, explain why, \
  and do not present those checks as passing.\
\
Do not give a child ownership of the entire parent feature merely because it \
needs the parent requirements for context. Keep shared context brief, make \
dependencies explicit through ordering, and avoid duplicating work across \
siblings. A child may leave an intentionally unreferenced or incomplete \
intermediate artifact when a later child is explicitly responsible for wiring \
or integration."
        .to_string();
    if !models.coordinator_model_choice || models.options.is_empty() {
        system = system.replace(
            ", \"model_option\": \"<optional approved key>\", \"model_reason\": \"<brief reason>\"",
            "",
        );
    }
    if models.coordinator_model_choice && !models.options.is_empty() {
        system.push_str("\n\nWhen a child benefits from a different approved model, set `model_option` to exactly one catalog key and give a short `model_reason`. Omit both fields when the normal role default is appropriate. Approved options:\n");
        for (key, option) in &models.options {
            let description = option
                .description
                .as_deref()
                .unwrap_or("No description provided");
            let _ = writeln!(system, "- {key} — {description}");
        }
        system.push_str("Do not use raw provider/model strings or invent option names.");
    }
    let mut user = format!(
        "Title: {}\n\nDescription:\n{}\n",
        orb.title, orb.description
    );
    if let Some(ref design) = orb.design {
        let _ = write!(user, "\nDesign:\n{design}\n");
    }
    if let Some(ref ac) = orb.acceptance_criteria {
        let _ = write!(user, "\nAcceptance criteria:\n{ac}\n");
    }
    (system, user)
}

/// Ensures coordinator-selected model options are keys in the configured
/// catalog. A decomposition response is untrusted, so raw model selectors are
/// intentionally not accepted here even though manual orb overrides retain
/// legacy raw-selector compatibility.
///
/// # Errors
///
/// Returns an error naming the child and unknown catalog key.
pub fn validate_model_options(
    plan: &DecompositionPlan,
    models: &crate::config::ModelConfig,
) -> anyhow::Result<()> {
    for subtask in &plan.subtasks {
        if let Some(key) = subtask.model_option.as_deref() {
            anyhow::ensure!(
                models.options.contains_key(key),
                "subtask `{}` selected unknown model option `{key}`",
                subtask.title
            );
        }
    }
    Ok(())
}

/// Persists a validated coordinator choice as the child's model preference.
/// The stored value remains the catalog key so later configuration resolution
/// can map it to the current provider/model details.
pub fn apply_model_option(child: &mut Orb, subtask: &DecomposedSubtask) {
    child.preferred_model.clone_from(&subtask.model_option);
}

/// Checks materialization requirements before accepting structured output.
pub(crate) fn validate_subtasks(plan: &DecompositionPlan) -> anyhow::Result<()> {
    anyhow::ensure!(
        !plan.subtasks.is_empty(),
        "decomposition plan has no subtasks"
    );
    for (index, subtask) in plan.subtasks.iter().enumerate() {
        anyhow::ensure!(
            !subtask.title.trim().is_empty() && !subtask.description.trim().is_empty(),
            "decomposition subtask {} has an empty title or description",
            index + 1
        );
    }
    Ok(())
}

/// Historical order-only plans remain readable. Mixed graph/legacy data is invalid.
pub(crate) fn validate_graph(plan: &DecompositionPlan) -> anyhow::Result<()> {
    if plan
        .subtasks
        .iter()
        .all(|task| task.id.is_none() && task.depends_on.is_none())
    {
        return Ok(());
    }
    let mut ids = std::collections::HashSet::new();
    for task in &plan.subtasks {
        let id = task.id.as_deref().unwrap_or("");
        anyhow::ensure!(!id.trim().is_empty(), "graph task requires a nonempty id");
        anyhow::ensure!(ids.insert(id), "duplicate graph task id: {id}");
        anyhow::ensure!(
            task.depends_on.is_some(),
            "task {id} must declare depends_on"
        );
    }
    for task in &plan.subtasks {
        for dependency in task.depends_on.iter().flatten() {
            anyhow::ensure!(
                ids.contains(dependency.as_str()),
                "unknown dependency: {dependency}"
            );
            anyhow::ensure!(
                task.id.as_ref() != Some(dependency),
                "self dependency: {dependency}"
            );
        }
    }
    let mut completed = std::collections::HashSet::new();
    loop {
        let previous = completed.len();
        for task in &plan.subtasks {
            if task
                .depends_on
                .iter()
                .flatten()
                .all(|id| completed.contains(id.as_str()))
            {
                if let Some(id) = task.id.as_deref() {
                    completed.insert(id);
                }
            }
        }
        if completed.len() == ids.len() {
            return Ok(());
        }
        anyhow::ensure!(
            completed.len() > previous,
            "dependency graph contains a cycle"
        );
    }
}

/// Converts an accepted worker plan into the durable child graph.  This is
/// deliberately separate from parsing: a response is not a decomposition
/// until the caller has persisted this result through its configured stores.
///
/// # Errors
///
/// Returns an error when the parent is not currently decomposing or the plan
/// contains no actionable subtasks.
pub fn materialize_plan(parent: &Orb, plan: &DecompositionPlan) -> anyhow::Result<DecomposeResult> {
    anyhow::ensure!(
        parent.phase == Some(OrbPhase::Decomposing),
        "parent orb must be in Decomposing phase, got {:?}",
        parent.phase
    );
    validate_subtasks(plan)?;
    validate_graph(plan)?;

    let root_id = parent.root_id.clone().unwrap_or_else(|| parent.id.clone());
    let mut children = Vec::with_capacity(plan.subtasks.len());
    let mut edges = Vec::new();
    let mut children_by_order: std::collections::BTreeMap<u32, Vec<OrbId>> =
        std::collections::BTreeMap::new();

    for (index, subtask) in plan.subtasks.iter().enumerate() {
        let child_id = parent
            .id
            .child(u32::try_from(index + 1).unwrap_or(u32::MAX));
        let mut child = Orb::new(&subtask.title, &subtask.description).with_type(OrbType::Task);
        child.id = child_id.clone();
        child.parent_id = Some(parent.id.clone());
        child.root_id = Some(root_id.clone());
        apply_model_option(&mut child, subtask);

        edges.push(DepEdge::new(
            parent.id.clone(),
            child_id.clone(),
            EdgeType::Parent,
        ));
        edges.push(DepEdge::new(
            child_id.clone(),
            parent.id.clone(),
            EdgeType::Child,
        ));
        children_by_order
            .entry(subtask.order)
            .or_default()
            .push(child_id);
        children.push(child);
    }
    if plan.subtasks.iter().any(|task| task.id.is_some()) {
        let ids: std::collections::HashMap<_, _> = plan
            .subtasks
            .iter()
            .zip(&children)
            .filter_map(|(task, child)| task.id.as_deref().map(|id| (id, &child.id)))
            .collect();
        for (task, child) in plan.subtasks.iter().zip(&children) {
            for dependency in task.depends_on.iter().flatten() {
                if let Some(prerequisite) = ids.get(dependency.as_str()) {
                    edges.push(DepEdge::new(
                        child.id.clone(),
                        (*prerequisite).clone(),
                        EdgeType::DependsOn,
                    ));
                }
            }
            tracing::info!(child = %child.id, local_id = ?task.id, depends_on = ?task.depends_on, "accepted decomposition graph node");
        }
        return Ok(DecomposeResult { children, edges });
    }
    // Build complete order groups before adding edges: input order must not
    // determine which prerequisites exist, and every parallel predecessor
    // must finish before the next group is eligible.
    let mut groups = children_by_order.values();
    if let Some(mut previous) = groups.next() {
        for current in groups {
            for child in current {
                for prerequisite in previous {
                    edges.push(DepEdge::new(
                        child.clone(),
                        prerequisite.clone(),
                        EdgeType::DependsOn,
                    ));
                }
            }
            previous = current;
        }
    }
    Ok(DecomposeResult { children, edges })
}

/// Parses the worker's response into a `DecompositionPlan`. Accepts
/// strict JSON or a fenced JSON block.
#[must_use]
pub fn parse_response(text: &str) -> Option<DecompositionPlan> {
    crate::phases::prompt_util::parse_response_json::<DecompositionPlan>(text)
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
    fn replay_preserves_child_progress_and_rejects_surplus_children() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrbStore::new(dir.path().join("orbs.jsonl"));
        let deps = DepStore::new(dir.path().join("deps.jsonl"));
        let mut parent = Orb::new("Parent", "first task\nsecond task").with_type(OrbType::Feature);
        parent.phase = Some(OrbPhase::Decomposing);
        store.append(&parent).unwrap();
        let result = decompose_orb(&parent, &store, &deps).unwrap();
        apply_decomposition(&result, &store, &deps).unwrap();
        let mut child = result.children.first().unwrap().clone();
        child.status = Some(orbs::orb::OrbStatus::Done);
        child.description = "operator-edited description".into();
        child.result = Some("completed evidence".into());
        store.update(&child).unwrap();
        apply_decomposition(&result, &store, &deps).unwrap();
        let replayed = store.load_by_id(&child.id).unwrap().unwrap();
        assert_eq!(replayed.status, child.status);
        assert_eq!(replayed.result, child.result);
        assert_eq!(replayed.description, child.description);
        let reduced = DecomposeResult {
            children: vec![result.children.first().unwrap().clone()],
            edges: vec![],
        };
        assert!(apply_decomposition(&reduced, &store, &deps).is_err());
        assert_eq!(store.load_children(&parent.id).unwrap().len(), 2);
    }

    #[test]
    fn begin_decomposing_from_speccing() {
        let mut orb = feature_orb("Auth", "Implement auth");
        orb.phase = Some(OrbPhase::Speccing); // test setup

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
        orb.phase = Some(OrbPhase::Decomposing); // test setup

        assert!(finish_decomposing(&mut orb));
        assert_eq!(orb.phase, Some(OrbPhase::Refining));
    }

    #[test]
    fn finish_decomposing_from_non_decomposing_fails() {
        let mut orb = feature_orb("Auth", "Implement auth");
        orb.phase = Some(OrbPhase::Speccing); // test setup

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
        orb.phase = Some(OrbPhase::Decomposing); // test setup

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
        orb.phase = Some(OrbPhase::Decomposing); // test setup

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
        orb.phase = Some(OrbPhase::Decomposing); // test setup

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
        orb.phase = Some(OrbPhase::Decomposing); // test setup

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
        orb.phase = Some(OrbPhase::Decomposing); // test setup

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
        orb.phase = Some(OrbPhase::Decomposing); // test setup

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
        orb.phase = Some(OrbPhase::Decomposing); // test setup

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
        orb.phase = Some(OrbPhase::Decomposing); // test setup

        let result = decompose_orb(&orb, &store, &dep_store).unwrap();
        apply_decomposition(&result, &store, &dep_store).unwrap();

        let children = store.load_children(&OrbId::from_raw("orb-feat1")).unwrap();
        assert_eq!(children.len(), 3);
    }

    #[test]
    fn materialized_worker_plan_keeps_parallel_groups_and_model_choices() {
        let (_dir, store, dep_store) = tmp_stores();
        let mut parent = feature_orb("Auth", "Implement auth");
        parent.phase = Some(OrbPhase::Decomposing);
        let plan = DecompositionPlan {
            subtasks: vec![
                DecomposedSubtask {
                    id: None,
                    depends_on: None,
                    title: "schema".into(),
                    description: "Add the schema".into(),
                    order: 1,
                    model_option: Some("fast".into()),
                    model_reason: None,
                },
                DecomposedSubtask {
                    id: None,
                    depends_on: None,
                    title: "api".into(),
                    description: "Add the API".into(),
                    order: 1,
                    model_option: None,
                    model_reason: None,
                },
                DecomposedSubtask {
                    id: None,
                    depends_on: None,
                    title: "tests".into(),
                    description: "Add tests".into(),
                    order: 2,
                    model_option: None,
                    model_reason: None,
                },
            ],
            has_parent_final_work: true,
        };

        let result = materialize_plan(&parent, &plan).unwrap();
        assert_eq!(result.children[0].preferred_model.as_deref(), Some("fast"));
        assert_eq!(result.children.len(), 3);
        assert_eq!(
            result
                .edges
                .iter()
                .filter(|edge| edge.edge_type == EdgeType::DependsOn)
                .count(),
            2
        );
        apply_decomposition(&result, &store, &dep_store).unwrap();
        assert_eq!(store.load_children(&parent.id).unwrap().len(), 3);
    }

    // ── snapshot_decomposition ───────────────────────────────

    #[test]
    fn materialized_order_groups_include_every_predecessor_independent_of_input_order() {
        let mut parent = feature_orb("Feature", "Grouped work");
        parent.phase = Some(OrbPhase::Decomposing);
        let plan = parse_response(
            r#"{"subtasks":[
            {"title":"finish","description":"finish","order":7},
            {"title":"api","description":"api","order":1},
            {"title":"tests","description":"tests","order":3},
            {"title":"schema","description":"schema","order":1},
            {"title":"docs","description":"docs","order":3}
        ]}"#,
        )
        .unwrap();
        let expected = std::collections::BTreeSet::from([
            ("tests", "api"),
            ("tests", "schema"),
            ("docs", "api"),
            ("docs", "schema"),
            ("finish", "tests"),
            ("finish", "docs"),
        ]);
        let mut reversed = plan.clone();
        reversed.subtasks.reverse();
        for candidate in [plan, reversed] {
            let result = materialize_plan(&parent, &candidate).unwrap();
            let titles: std::collections::HashMap<_, _> = result
                .children
                .iter()
                .map(|child| (&child.id, child.title.as_str()))
                .collect();
            let actual: std::collections::BTreeSet<_> = result
                .edges
                .iter()
                .filter(|edge| edge.edge_type == EdgeType::DependsOn)
                .map(|edge| (titles[&edge.from], titles[&edge.to]))
                .collect();
            assert_eq!(actual, expected);
        }
    }

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
        orb.phase = Some(OrbPhase::Speccing); // test setup
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

    // ── build_prompt / parse_response ─────────────────────────

    #[test]
    fn build_prompt_includes_description_and_design() {
        let mut orb = Orb::new("Build auth", "Make it work").with_type(OrbType::Feature);
        orb.design = Some("Use PKCE flow".into());
        let (system, user) = build_prompt(&orb, &crate::config::ModelConfig::default());
        assert!(system.contains("subtasks"));
        assert!(system.contains("order"));
        assert!(system.contains("actual concrete task"));
        assert!(system.contains("only when they provide useful information"));
        assert!(system.contains("Deferred work:"));
        assert!(user.contains("Build auth"));
        assert!(user.contains("Make it work"));
        assert!(user.contains("Use PKCE flow"));
    }

    #[test]
    fn build_prompt_omits_design_when_absent() {
        let orb = Orb::new("X", "Y").with_type(OrbType::Feature);
        let (_system, user) = build_prompt(&orb, &crate::config::ModelConfig::default());
        assert!(!user.contains("\nDesign:"));
    }

    #[test]
    fn parse_response_extracts_subtasks() {
        let text = r#"{"subtasks": [
            {"title": "Step 1", "description": "Do A", "order": 1},
            {"title": "Step 2", "description": "Do B", "order": 2}
        ]}"#;
        let plan = parse_response(text).unwrap();
        assert_eq!(plan.subtasks.len(), 2);
        assert_eq!(plan.subtasks[0].title, "Step 1");
        assert_eq!(plan.subtasks[1].order, 2);
    }

    #[test]
    fn decomposition_model_options_are_catalog_keys() {
        let mut config = crate::config::ModelConfig {
            options: std::collections::BTreeMap::from([(
                "fast".to_string(),
                crate::config::ModelOption {
                    model: "openai/gpt-4.1-mini".to_string(),
                    description: Some("Fast implementation model".to_string()),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let text = r#"{"subtasks":[{"title":"Implement","description":"Add it","model_option":"fast","model_reason":"Small focused change"}]}"#;

        let plan = parse_response(text).unwrap();
        assert_eq!(plan.subtasks[0].model_option.as_deref(), Some("fast"));
        assert_eq!(
            plan.subtasks[0].model_reason.as_deref(),
            Some("Small focused change")
        );
        assert!(!build_prompt(&feature_orb("X", "Y"), &config)
            .0
            .contains("Approved options"));
        assert!(!build_prompt(&feature_orb("X", "Y"), &config)
            .0
            .contains("model_option"));
        config.coordinator_model_choice = true;
        assert!(validate_model_options(&plan, &config).is_ok());
        assert!(build_prompt(&feature_orb("X", "Y"), &config)
            .0
            .contains("fast — Fast implementation model"));

        let mut child = Orb::new("Implement", "Add it");
        apply_model_option(&mut child, &plan.subtasks[0]);
        assert_eq!(child.preferred_model.as_deref(), Some("fast"));
    }

    #[test]
    fn decomposition_rejects_unknown_model_option() {
        let plan = parse_response(
            r#"{"subtasks":[{"title":"Implement","description":"Add it","model_option":"not-approved"}]}"#,
        )
        .unwrap();

        let error =
            validate_model_options(&plan, &crate::config::ModelConfig::default()).unwrap_err();
        assert!(error.to_string().contains("not-approved"));
    }

    #[test]
    fn parse_response_defaults_missing_order_to_1() {
        let text = r#"{"subtasks": [{"title": "T", "description": "D"}]}"#;
        let plan = parse_response(text).unwrap();
        assert_eq!(plan.subtasks[0].order, 1);
    }

    #[test]
    fn parse_response_accepts_fenced_block() {
        let text = "```json\n{\"subtasks\": [{\"title\":\"a\",\"description\":\"b\"}]}\n```";
        let plan = parse_response(text).unwrap();
        assert_eq!(plan.subtasks.len(), 1);
    }

    #[test]
    fn parse_response_accepts_prose_wrapped_plan_and_confidence_line() {
        let text = "Based on the repository inspection, here is the plan:\n\n{\"subtasks\": [{\"title\":\"Add todo model\",\"description\":\"Implement the Todo struct.\",\"order\":1}], \"has_parent_final_work\": true}\n\nCONFIDENCE: 0.95";
        let plan = parse_response(text).unwrap();
        assert_eq!(plan.subtasks.len(), 1);
        assert_eq!(plan.subtasks[0].title, "Add todo model");
        assert!(plan.has_parent_final_work);
    }
}

#[cfg(test)]
mod explicit_graph_tests {
    use super::*;

    #[test]
    fn validates_graph_and_rejects_cycles_and_unknown_references() {
        let mut plan = parse_response(
            r#"{"subtasks":[
            {"id":"core","title":"Core","description":"Produce API","depends_on":[]},
            {"id":"app","title":"App","description":"Consume API","depends_on":["core"]}
        ]}"#,
        )
        .unwrap();
        assert!(validate_graph(&plan).is_ok());
        plan.subtasks[0].depends_on = Some(vec!["app".into()]);
        assert!(validate_graph(&plan).is_err());
        plan.subtasks[0].depends_on = Some(vec!["missing".into()]);
        assert!(validate_graph(&plan).is_err());
        plan.subtasks[0].depends_on = None;
        assert!(validate_graph(&plan).is_err());
    }
}

#![allow(clippy::needless_pass_by_value)]

use std::path::{Path, PathBuf};

use orbs::dep::{DepEdge, EdgeType};
use orbs::dep_store::DepStore;
use orbs::orb::{Orb, OrbPhase, OrbType};
use orbs::orb_store::OrbStore;
use orbs::pipeline::{self, PipelineDir};

use crate::phases::decompose::{apply_decomposition, decompose_orb, snapshot_decomposition};

/// Configuration for the plan pipeline.
#[derive(Debug, Clone, Default)]
pub struct PlanConfig {
    /// If true, persist decomposition and stop in Decomposing before refinement.
    pub shallow: bool,
    /// If set, read the task description from this file.
    pub file: Option<PathBuf>,
}

/// Builds the records for a locally scaffolded plan without opening a store.
///
/// Shared-state callers submit the returned records as one supervisor
/// operation; unlike the historical pipeline implementation this cannot leave
/// a partially-created plan visible after a concurrent mutation or crash.
pub fn build_shared_plan(
    title: &str,
    description: &str,
    shallow: bool,
) -> anyhow::Result<(Orb, Vec<Orb>, Vec<DepEdge>)> {
    let mut epic = Orb::new(title, description).with_type(OrbType::Epic);
    epic.set_phase(OrbPhase::Speccing)
        .map_err(|e| anyhow::anyhow!("speccing transition rejected: {e}"))?;
    epic.set_phase(OrbPhase::Decomposing)
        .map_err(|e| anyhow::anyhow!("decomposing transition rejected: {e}"))?;

    let lines: Vec<&str> = description
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let subtasks: Vec<(String, String)> = if lines.len() > 1 {
        lines
            .iter()
            .enumerate()
            .map(|(index, line)| {
                (
                    format!("{title} - subtask {}", index + 1),
                    (*line).to_owned(),
                )
            })
            .collect()
    } else {
        vec![(format!("{title} - implementation"), description.to_owned())]
    };
    let mut children = Vec::with_capacity(subtasks.len());
    let mut edges = Vec::new();
    for (index, (child_title, child_description)) in subtasks.into_iter().enumerate() {
        let child_id = epic.id.child(u32::try_from(index + 1).unwrap_or(u32::MAX));
        let mut child = Orb::new(&child_title, &child_description).with_type(OrbType::Task);
        child.id = child_id.clone();
        child.parent_id = Some(epic.id.clone());
        child.root_id = Some(epic.id.clone());
        edges.push(DepEdge::new(
            epic.id.clone(),
            child_id.clone(),
            EdgeType::Parent,
        ));
        edges.push(DepEdge::new(
            child_id.clone(),
            epic.id.clone(),
            EdgeType::Child,
        ));
        if index > 0 {
            edges.push(DepEdge::new(
                child_id,
                epic.id.child(u32::try_from(index).unwrap_or(u32::MAX)),
                EdgeType::DependsOn,
            ));
        }
        children.push(child);
    }
    if shallow {
        return Ok((epic, children, edges));
    }

    // A normal plan has only constructed a local child scaffold. Refining
    // is deliberately left queued for a worker; it has not completed.
    epic.set_phase(OrbPhase::Refining)
        .map_err(|e| anyhow::anyhow!("refining transition rejected: {e}"))?;
    Ok((epic, children, edges))
}

/// Persists a plan scaffold into the canonical state store.
///
/// The queue, `orb` commands, and daemon all use these stores. Keeping plan
/// creation here avoids the former split where `plan` wrote only a private
/// pipeline directory that ordinary CLI commands could not inspect.
pub fn persist_plan(
    epic: &Orb,
    children: &[Orb],
    edges: &[DepEdge],
    store: &OrbStore,
    dep_store: &DepStore,
) -> anyhow::Result<()> {
    store
        .append(epic)
        .map_err(|e| anyhow::anyhow!("failed to append plan epic: {e}"))?;
    for child in children {
        store
            .append(child)
            .map_err(|e| anyhow::anyhow!("failed to append plan child: {e}"))?;
    }
    for edge in edges {
        dep_store
            .add_edge(edge.clone())
            .map_err(|e| anyhow::anyhow!("failed to append plan dependency: {e}"))?;
    }
    Ok(())
}

/// Prints the lifecycle and next action for a newly created plan.
pub fn print_plan_summary(
    store: &OrbStore,
    dep_store: &DepStore,
    epic: &Orb,
    state_dir: &Path,
    shared_state: bool,
    shallow: bool,
) {
    print_plan_tree(store, dep_store, epic);
    let mode = if shallow {
        "shallow scaffold (stopped before refinement; planning may resume on execution)"
    } else {
        "normal scaffold (refinement queued)"
    };
    println!("\nPlan created");
    println!("  Epic:       {}", epic.id);
    println!(
        "  Children:   {}",
        store.load_children(&epic.id).map_or(0, |v| v.len())
    );
    println!(
        "  Lifecycle:  {:?} — {mode}",
        epic.phase.unwrap_or(OrbPhase::Pending)
    );
    println!(
        "  State:      {} ({})",
        state_dir.display(),
        if shared_state {
            "shared-state projection"
        } else {
            "local projection"
        }
    );
    println!("  Inspect:    orboros plan --status {}", epic.id);
    println!("  Next:       orboros execute {} --wait", epic.id);
}

/// Prints a compact, inspectable plan lifecycle view.
pub fn print_plan_status(store: &OrbStore, dep_store: &DepStore, id: &str) -> anyhow::Result<()> {
    use orbs::id::OrbId;

    let epic = store
        .load_by_id(&OrbId::from_raw(id))?
        .ok_or_else(|| anyhow::anyhow!("plan orb not found: {id}"))?;
    anyhow::ensure!(
        epic.orb_type.uses_phase(),
        "orb {id} is not a plan-capable epic or feature"
    );
    let all_orbs = store.load_all()?;
    let children = store.load_children(&epic.id)?;
    let ready = dep_store.ready(&all_orbs)?;
    let phase = epic.phase.unwrap_or(OrbPhase::Pending);
    let child_done = children
        .iter()
        .filter(|child| child.effective_status() == orbs::task::TaskStatus::Done)
        .count();
    let child_failed = children
        .iter()
        .filter(|child| child.effective_status() == orbs::task::TaskStatus::Failed)
        .count();
    let child_ready = children
        .iter()
        .filter(|child| ready.contains(&child.id))
        .count();

    println!("Plan:       {} ({})", epic.title, epic.id);
    println!("Phase:      {phase:?}");
    println!(
        "Children:   {} total; {child_done} done; {child_failed} failed; {child_ready} ready",
        children.len()
    );
    println!(
        "Dispatch:   {}",
        if epic.execution.is_some() {
            "execution marker present (worker liveness not confirmed)"
        } else {
            "no execution marker"
        }
    );
    let next = plan_next_action(
        phase,
        epic.execution.is_some(),
        epic.id.as_str(),
        children.len(),
        child_done,
        child_failed,
        child_ready,
    );
    println!("Next action: {next}");
    Ok(())
}

fn plan_next_action(
    phase: OrbPhase,
    execution_present: bool,
    id: &str,
    child_count: usize,
    child_done: usize,
    child_failed: usize,
    child_ready: usize,
) -> String {
    if execution_present {
        return format!(
            "execution marker present; confirm worker activity with orboros orb logs {id} and orboros daemon --status before recovery"
        );
    }
    match phase {
        OrbPhase::Speccing | OrbPhase::Decomposing | OrbPhase::Refining | OrbPhase::Reevaluating => {
            format!("eligible for {} dispatch; orboros execute {id} --wait", phase_name(phase))
        }
        OrbPhase::Review => format!("awaiting review; orboros orb show {id}, then orboros orb review {id} <decision>"),
        OrbPhase::Waiting if child_failed > 0 => {
            format!("blocked by failed child; orboros orb deps {id}, then inspect that child's orb logs and orb reset it")
        }
        OrbPhase::Waiting if child_done == child_count && child_count > 0 => {
            format!("children complete; orboros execute {id} --wait advances parent final work or completion")
        }
        OrbPhase::Waiting if child_ready > 0 => {
            format!("{child_ready} children ready; orboros execute {id} --wait")
        }
        OrbPhase::Waiting => format!("no ready children; orboros orb deps {id} to inspect dependencies and orboros orb show <child-id> to inspect lifecycle"),
        OrbPhase::Done => "complete; no further action required".to_string(),
        OrbPhase::Failed => format!("failed; orboros orb logs {id}, then orboros orb reset {id} after resolving the failure"),
        _ => format!("not currently queue-dispatchable; orboros orb show {id} to inspect lifecycle state"),
    }
}

fn phase_name(phase: OrbPhase) -> &'static str {
    match phase {
        OrbPhase::Speccing => "speccing",
        OrbPhase::Decomposing => "decomposition",
        OrbPhase::Refining => "refinement",
        OrbPhase::Reevaluating => "re-evaluation",
        _ => "worker",
    }
}

/// Parses plan text in the same format accepted by `--file`.
pub fn parse_plan_text(content: &str) -> anyhow::Result<(String, String)> {
    parse_plan_file(content)
}

/// Creates a plan: an epic orb with shallow decomposition into child orbs.
///
/// 1. Creates an epic orb in `Pending` phase
/// 2. Creates a pipeline directory
/// 3. Transitions through Speccing -> Decomposing
/// 4. Runs shallow decomposition (stub: splits description lines into subtasks)
/// 5. Persists children and dep edges to the pipeline store
/// 6. Takes a decomposition snapshot
/// 7. Queues refinement unless shallow; shallow plans remain in Decomposing
/// 8. Returns the epic orb
///
/// # Errors
///
/// Returns an error if store operations or decomposition fails.
pub fn create_plan(
    title: &str,
    description: &str,
    base_dir: &Path,
    config: &PlanConfig,
) -> anyhow::Result<(Orb, PipelineDir)> {
    // Create the epic orb
    let mut epic = Orb::new(title, description).with_type(OrbType::Epic);

    // Create pipeline directory
    let pipeline = pipeline::create_pipeline(base_dir, &epic)?;
    let store = pipeline.orb_store();
    let dep_store = DepStore::new(pipeline.deps_path());

    // Persist the epic to the pipeline store
    store
        .append(&epic)
        .map_err(|e| anyhow::anyhow!("failed to append epic orb: {e}"))?;

    // Transition: Pending -> Speccing -> Decomposing
    epic.set_phase(OrbPhase::Speccing)
        .map_err(|e| anyhow::anyhow!("speccing transition rejected: {e}"))?;
    store
        .update(&epic)
        .map_err(|e| anyhow::anyhow!("failed to update epic phase: {e}"))?;

    epic.set_phase(OrbPhase::Decomposing)
        .map_err(|e| anyhow::anyhow!("decomposing transition rejected: {e}"))?;
    store
        .update(&epic)
        .map_err(|e| anyhow::anyhow!("failed to update epic phase: {e}"))?;

    // Run decomposition
    let result = decompose_orb(&epic, &store, &dep_store)?;
    apply_decomposition(&result, &store, &dep_store)?;

    // Snapshot the decomposition state
    snapshot_decomposition(&pipeline)?;

    // Decomposing is the last reached phase for shallow plans. Waiting requires
    // review, so advancing there would claim work that has not happened.
    if config.shallow {
        return Ok((epic, pipeline));
    }

    epic.set_phase(OrbPhase::Refining)
        .map_err(|e| anyhow::anyhow!("refining transition rejected: {e}"))?;
    store
        .update(&epic)
        .map_err(|e| anyhow::anyhow!("failed to update epic phase: {e}"))?;

    Ok((epic, pipeline))
}

/// Reads a markdown file and creates a plan from it.
///
/// Format: first non-empty line = title, rest = description.
///
/// # Errors
///
/// Returns an error if the file cannot be read or is empty.
pub fn create_plan_from_file(
    path: &Path,
    base_dir: &Path,
    config: &PlanConfig,
) -> anyhow::Result<(Orb, PipelineDir)> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("failed to read plan file {}: {e}", path.display()))?;

    let (title, description) = parse_plan_file(&content)?;
    create_plan(&title, &description, base_dir, config)
}

/// Parses a plan file: first non-empty line = title, rest = description.
///
/// Strips leading `#` from the title line (markdown heading).
fn parse_plan_file(content: &str) -> anyhow::Result<(String, String)> {
    let mut lines = content.lines();

    // Find the first non-empty line as the title
    let title = loop {
        match lines.next() {
            Some(line) => {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    // Strip leading markdown heading markers
                    let title = trimmed.trim_start_matches('#').trim();
                    break title.to_string();
                }
            }
            None => anyhow::bail!("plan file is empty"),
        }
    };

    anyhow::ensure!(
        !title.is_empty(),
        "plan file title is empty after stripping"
    );

    // Rest is description (skip blank lines immediately after title)
    let remaining: Vec<&str> = lines.collect();
    let description = remaining.join("\n").trim().to_string();

    // Allow empty description
    Ok((title, description))
}

/// Copies pipeline orbs into the canonical store.
///
/// Loads all orbs from the pipeline's `orbs.jsonl` and appends them
/// to the canonical `OrbStore`.
///
/// # Errors
///
/// Returns an error if reading or writing fails.
pub fn merge_to_canonical(
    _plan_orb: &Orb,
    pipeline: &PipelineDir,
    canonical_store: &OrbStore,
) -> anyhow::Result<()> {
    let pipeline_store = pipeline.orb_store();
    let orbs = pipeline_store
        .load_all()
        .map_err(|e| anyhow::anyhow!("failed to load pipeline orbs: {e}"))?;

    for orb in &orbs {
        canonical_store
            .append(orb)
            .map_err(|e| anyhow::anyhow!("failed to append orb to canonical store: {e}"))?;
    }

    Ok(())
}

/// Prints a plan tree to stdout.
pub fn print_plan_tree(store: &OrbStore, dep_store: &DepStore, root: &Orb) {
    use orbs::tree::build_full_timeline;

    if let Some(timeline) = build_full_timeline(store, dep_store, &root.id) {
        println!("Plan: {} ({})", root.title, root.id);
        println!(
            "  Type: {:?}  Phase: {:?}",
            root.orb_type,
            root.phase.unwrap_or(OrbPhase::Pending)
        );
        println!(
            "  {} orb(s), depth {}",
            timeline.total_orbs, timeline.max_depth
        );
        println!();
        print_node(&timeline.root, "");
    } else {
        println!("Plan: {} ({})", root.title, root.id);
        println!("  (no children yet)");
    }
}

fn print_node(node: &orbs::tree::OrbNode, prefix: &str) {
    let type_str = node.orb.orb_type.as_hash_str();
    let status = if let Some(phase) = node.orb.phase {
        format!("{phase:?}")
    } else if let Some(status) = node.orb.status {
        format!("{status:?}")
    } else {
        "?".to_string()
    };

    println!("{prefix}{} [{type_str}] ({status})", node.orb.title);

    for child in &node.children {
        let child_prefix = format!("{prefix}  ");
        print_node(child, &child_prefix);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orbs::dep::EdgeType;

    fn tmp_base_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn waiting_plan_guidance_distinguishes_blocked_and_ready_children() {
        let blocked = plan_next_action(OrbPhase::Waiting, false, "orb-test", 2, 0, 0, 0);
        assert!(blocked.contains("no ready children"));
        assert!(blocked.contains("orb deps orb-test"));
        let ready = plan_next_action(OrbPhase::Waiting, false, "orb-test", 2, 0, 0, 1);
        assert!(ready.contains("execute orb-test --wait"));
        let failed = plan_next_action(OrbPhase::Waiting, false, "orb-test", 2, 0, 1, 0);
        assert!(failed.contains("failed child"));
    }

    #[test]
    fn execution_marker_is_not_proof_of_a_live_worker() {
        let next = plan_next_action(OrbPhase::Refining, true, "orb-test", 1, 0, 0, 0);
        assert!(next.contains("confirm worker activity"));
        assert!(!next.contains("eligible"));
    }

    #[test]
    fn shared_normal_plan_queues_refinement() {
        let (epic, children, edges) = build_shared_plan("Feature", "First\nSecond", false).unwrap();

        assert_eq!(epic.phase, Some(OrbPhase::Refining));
        assert_eq!(children.len(), 2);
        assert_eq!(
            edges
                .iter()
                .filter(|edge| edge.edge_type == EdgeType::Parent)
                .count(),
            2
        );
    }

    #[test]
    fn shared_shallow_plan_stops_before_refinement() {
        let (epic, children, edges) = build_shared_plan("Feature", "First\nSecond", true).unwrap();

        assert_eq!(epic.phase, Some(OrbPhase::Decomposing));
        assert_eq!(children.len(), 2);
        assert_eq!(
            edges
                .iter()
                .filter(|edge| edge.edge_type == EdgeType::Parent)
                .count(),
            2
        );
    }

    #[test]
    fn persist_plan_writes_canonical_orbs_and_dependencies() {
        let tmp = tmp_base_dir();
        let store = OrbStore::new(tmp.path().join("orbs.jsonl"));
        let dep_store = DepStore::new(tmp.path().join("deps.jsonl"));
        let (epic, children, edges) = build_shared_plan("Feature", "First\nSecond", false).unwrap();

        persist_plan(&epic, &children, &edges, &store, &dep_store).unwrap();

        assert_eq!(
            store.load_by_id(&epic.id).unwrap().unwrap().phase,
            Some(OrbPhase::Refining)
        );
        assert_eq!(store.load_children(&epic.id).unwrap().len(), 2);
        assert_eq!(dep_store.all_edges().unwrap().len(), edges.len());
        print_plan_status(&store, &dep_store, epic.id.as_str()).unwrap();
    }

    // ── create_plan tests ────────────────────────────────────

    #[test]
    fn create_plan_creates_epic_orb() {
        let tmp = tmp_base_dir();
        let config = PlanConfig::default();

        let (epic, _pipeline) = create_plan(
            "Auth system",
            "Design auth\nImplement login",
            tmp.path(),
            &config,
        )
        .unwrap();

        assert_eq!(epic.orb_type, OrbType::Epic);
        assert_eq!(epic.title, "Auth system");
    }

    #[test]
    fn create_plan_epic_is_in_refining_phase() {
        let tmp = tmp_base_dir();
        let config = PlanConfig::default();

        let (epic, _pipeline) =
            create_plan("Feature X", "Step one\nStep two", tmp.path(), &config).unwrap();

        assert_eq!(epic.phase, Some(OrbPhase::Refining));
    }

    #[test]
    fn create_plan_shallow_persists_decomposition_without_refinement() {
        let tmp = tmp_base_dir();
        let config = PlanConfig {
            shallow: true,
            file: None,
        };

        let (epic, pipeline) =
            create_plan("Feature X", "Step one\nStep two", tmp.path(), &config).unwrap();

        assert_eq!(epic.phase, Some(OrbPhase::Decomposing));
        assert_eq!(
            pipeline
                .orb_store()
                .load_by_id(&epic.id)
                .unwrap()
                .unwrap()
                .phase,
            Some(OrbPhase::Decomposing)
        );
        assert_eq!(
            pipeline.orb_store().load_children(&epic.id).unwrap().len(),
            2
        );
        assert!(pipeline.snapshots_dir().join("decomposition").exists());
        assert!(!pipeline.snapshots_dir().join("refinement-1").exists());
    }

    #[test]
    fn plan_modes_preserve_the_same_decomposition() {
        let mut graphs = Vec::new();
        for shallow in [false, true] {
            let tmp = tmp_base_dir();
            let config = PlanConfig {
                shallow,
                ..Default::default()
            };
            let (epic, pipeline) =
                create_plan("Feature", " First\n\nSecond ", tmp.path(), &config).unwrap();
            let store = pipeline.orb_store();
            let deps = DepStore::new(pipeline.deps_path());
            let snapshot = pipeline.snapshots_dir().join("decomposition");
            let snapshot_store = OrbStore::new(snapshot.join("orbs.jsonl"));
            let snapshot_deps = DepStore::new(snapshot.join("deps.jsonl"));
            assert_eq!(
                snapshot_store.load_by_id(&epic.id).unwrap().unwrap().phase,
                Some(OrbPhase::Decomposing)
            );
            assert_eq!(snapshot_store.load_children(&epic.id).unwrap().len(), 2);
            let edge_signature = |edges: Vec<DepEdge>| {
                let mut signature: Vec<_> = edges
                    .into_iter()
                    .map(|edge| {
                        (
                            edge.from.as_str().replace(epic.id.as_str(), "epic"),
                            edge.to.as_str().replace(epic.id.as_str(), "epic"),
                            edge.edge_type,
                        )
                    })
                    .collect();
                signature.sort_by_key(|(from, to, edge_type)| {
                    (from.clone(), to.clone(), format!("{edge_type:?}"))
                });
                signature
            };
            assert_eq!(
                edge_signature(snapshot_deps.all_edges().unwrap()),
                edge_signature(deps.all_edges().unwrap())
            );
            assert_eq!(epic.description, " First\n\nSecond ");

            let mut children: Vec<_> = store
                .load_children(&epic.id)
                .unwrap()
                .into_iter()
                .map(|child| (child.title, child.description))
                .collect();
            children.sort();
            let edges: Vec<_> = deps
                .all_edges()
                .unwrap()
                .into_iter()
                .map(|edge| {
                    (
                        edge.from.as_str().replace(epic.id.as_str(), "epic"),
                        edge.to.as_str().replace(epic.id.as_str(), "epic"),
                        edge.edge_type,
                    )
                })
                .collect();
            let mut edges = edges;
            edges.sort_by_key(|(from, to, edge_type)| {
                (from.clone(), to.clone(), format!("{edge_type:?}"))
            });
            assert_eq!(edges.len(), 5);
            print_plan_status(&store, &deps, epic.id.as_str()).unwrap();
            print_plan_status(&store, &deps, epic.id.as_str()).unwrap();
            assert_eq!(store.load_all().unwrap().len(), 3);
            assert_eq!(deps.all_edges().unwrap().len(), 5);
            graphs.push((children, edges));
        }
        assert_eq!(graphs.first(), graphs.last());
    }

    #[test]
    fn create_plan_creates_children() {
        let tmp = tmp_base_dir();
        let config = PlanConfig::default();

        let (epic, pipeline) =
            create_plan("My plan", "Task A\nTask B\nTask C", tmp.path(), &config).unwrap();

        let store = pipeline.orb_store();
        let children = store.load_children(&epic.id).unwrap();
        assert_eq!(children.len(), 3);
    }

    #[test]
    fn create_plan_children_have_correct_parent() {
        let tmp = tmp_base_dir();
        let config = PlanConfig::default();

        let (epic, pipeline) =
            create_plan("Parent plan", "Sub A\nSub B", tmp.path(), &config).unwrap();

        let store = pipeline.orb_store();
        let children = store.load_children(&epic.id).unwrap();
        for child in &children {
            assert_eq!(child.parent_id, Some(epic.id.clone()));
        }
    }

    #[test]
    fn create_plan_creates_dep_edges() {
        let tmp = tmp_base_dir();
        let config = PlanConfig::default();

        let (_epic, pipeline) =
            create_plan("Deps plan", "Step 1\nStep 2\nStep 3", tmp.path(), &config).unwrap();

        let dep_store = DepStore::new(pipeline.deps_path());
        let edges = dep_store.all_edges().unwrap();

        // Should have parent/child edges + ordering edges
        let parent_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::Parent)
            .collect();
        assert_eq!(parent_edges.len(), 3);

        let depends_on_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::DependsOn)
            .collect();
        assert_eq!(depends_on_edges.len(), 2);
    }

    #[test]
    fn create_plan_takes_snapshot() {
        let tmp = tmp_base_dir();
        let config = PlanConfig::default();

        let (_epic, pipeline) = create_plan("Snapshot plan", "A\nB", tmp.path(), &config).unwrap();

        let snap_dir = pipeline.snapshots_dir().join("decomposition");
        assert!(snap_dir.exists(), "decomposition snapshot should exist");
    }

    #[test]
    fn create_plan_single_line_description() {
        let tmp = tmp_base_dir();
        let config = PlanConfig::default();

        let (epic, pipeline) =
            create_plan("Single task", "Just one thing", tmp.path(), &config).unwrap();

        let store = pipeline.orb_store();
        let children = store.load_children(&epic.id).unwrap();
        assert_eq!(children.len(), 1);
    }

    // ── parse_plan_file tests ────────────────────────────────

    #[test]
    fn parse_plan_file_basic() {
        let (title, desc) = parse_plan_file("My Plan\nDo thing A\nDo thing B").unwrap();
        assert_eq!(title, "My Plan");
        assert_eq!(desc, "Do thing A\nDo thing B");
    }

    #[test]
    fn parse_plan_file_strips_markdown_heading() {
        let (title, _desc) = parse_plan_file("# My Plan\nDescription here").unwrap();
        assert_eq!(title, "My Plan");
    }

    #[test]
    fn parse_plan_file_strips_multiple_hashes() {
        let (title, _desc) = parse_plan_file("## Sub Plan\nDetails").unwrap();
        assert_eq!(title, "Sub Plan");
    }

    #[test]
    fn parse_plan_file_skips_leading_blank_lines() {
        let (title, desc) = parse_plan_file("\n\n  \nTitle Here\nBody").unwrap();
        assert_eq!(title, "Title Here");
        assert_eq!(desc, "Body");
    }

    #[test]
    fn parse_plan_file_empty_returns_error() {
        let result = parse_plan_file("");
        assert!(result.is_err());
    }

    #[test]
    fn parse_plan_file_only_whitespace_returns_error() {
        let result = parse_plan_file("   \n  \n  ");
        assert!(result.is_err());
    }

    #[test]
    fn parse_plan_file_title_only_no_description() {
        let (title, desc) = parse_plan_file("Just a Title").unwrap();
        assert_eq!(title, "Just a Title");
        assert_eq!(desc, "");
    }

    // ── create_plan_from_file tests ──────────────────────────

    #[test]
    fn create_plan_from_file_reads_markdown() {
        let tmp = tmp_base_dir();
        let config = PlanConfig::default();

        let file_path = tmp.path().join("plan.md");
        std::fs::write(&file_path, "# Auth Feature\nDesign login\nImplement OAuth").unwrap();

        let (epic, pipeline) = create_plan_from_file(&file_path, tmp.path(), &config).unwrap();

        assert_eq!(epic.title, "Auth Feature");
        assert_eq!(epic.orb_type, OrbType::Epic);

        let store = pipeline.orb_store();
        let children = store.load_children(&epic.id).unwrap();
        assert_eq!(children.len(), 2);
    }

    #[test]
    fn create_plan_from_file_missing_file_errors() {
        let tmp = tmp_base_dir();
        let config = PlanConfig::default();

        let result = create_plan_from_file(Path::new("/nonexistent/plan.md"), tmp.path(), &config);
        assert!(result.is_err());
    }

    // ── merge_to_canonical tests ─────────────────────────────

    #[test]
    fn merge_to_canonical_copies_all_orbs() {
        let tmp = tmp_base_dir();
        let config = PlanConfig::default();

        let (epic, pipeline) =
            create_plan("Merge test", "Task A\nTask B", tmp.path(), &config).unwrap();

        let canonical = OrbStore::new(tmp.path().join("canonical_orbs.jsonl"));
        merge_to_canonical(&epic, &pipeline, &canonical).unwrap();

        let canonical_orbs = canonical.load_all().unwrap();
        // Should have the epic + 2 children = 3 orbs
        // But the epic is stored multiple times due to phase transitions;
        // OrbStore deduplicates by ID, so we get the latest state per ID.
        // Epic (latest state) + 2 children = 3
        assert_eq!(canonical_orbs.len(), 3);
    }

    #[test]
    fn merge_to_canonical_preserves_hierarchy() {
        let tmp = tmp_base_dir();
        let config = PlanConfig::default();

        let (epic, pipeline) =
            create_plan("Hierarchy test", "Sub 1\nSub 2", tmp.path(), &config).unwrap();

        let canonical = OrbStore::new(tmp.path().join("canonical_orbs.jsonl"));
        merge_to_canonical(&epic, &pipeline, &canonical).unwrap();

        let children = canonical.load_children(&epic.id).unwrap();
        assert_eq!(children.len(), 2);
        for child in &children {
            assert_eq!(child.parent_id, Some(epic.id.clone()));
        }
    }
}

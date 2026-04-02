#![allow(clippy::needless_pass_by_value)]

use std::path::{Path, PathBuf};

use orbs::dep_store::DepStore;
use orbs::orb::{Orb, OrbPhase, OrbType};
use orbs::orb_store::OrbStore;
use orbs::pipeline::{self, PipelineDir};

use crate::phases::decompose::{apply_decomposition, decompose_orb, snapshot_decomposition};

/// Configuration for the plan pipeline.
#[derive(Debug, Clone, Default)]
pub struct PlanConfig {
    /// If true, only run shallow decomposition (no refinement).
    pub shallow: bool,
    /// If set, read the task description from this file.
    pub file: Option<PathBuf>,
}

/// Creates a plan: an epic orb with shallow decomposition into child orbs.
///
/// 1. Creates an epic orb in `Pending` phase
/// 2. Creates a pipeline directory
/// 3. Transitions through Speccing -> Decomposing
/// 4. Runs shallow decomposition (stub: splits description lines into subtasks)
/// 5. Persists children and dep edges to the pipeline store
/// 6. Takes a decomposition snapshot
/// 7. Returns the epic orb
///
/// # Errors
///
/// Returns an error if store operations or decomposition fails.
pub fn create_plan(
    title: &str,
    description: &str,
    base_dir: &Path,
    _config: &PlanConfig,
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
    epic.set_phase(OrbPhase::Speccing);
    store
        .update(&epic)
        .map_err(|e| anyhow::anyhow!("failed to update epic phase: {e}"))?;

    epic.set_phase(OrbPhase::Decomposing);
    store
        .update(&epic)
        .map_err(|e| anyhow::anyhow!("failed to update epic phase: {e}"))?;

    // Run decomposition
    let result = decompose_orb(&epic, &store, &dep_store)?;
    apply_decomposition(&result, &store, &dep_store)?;

    // Snapshot the decomposition state
    snapshot_decomposition(&pipeline)?;

    // Transition to Refining (or Done if shallow)
    epic.set_phase(OrbPhase::Refining);
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

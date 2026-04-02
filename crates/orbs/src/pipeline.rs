use std::path::{Path, PathBuf};

use crate::dep::DepEdge;
use crate::id::OrbId;
use crate::orb::Orb;
use crate::orb_store::OrbStore;

/// Manages a pipeline directory: `pipelines/<type>-<hash>/`
///
/// Inside each pipeline dir:
/// - `orbs.jsonl` — pipeline-local orb store
/// - `deps.jsonl` — pipeline-local dependency edges
/// - `events.jsonl` — pipeline-local audit events
/// - `snapshots/` — phase snapshots (e.g. `snapshots/decomposition/`)
/// - `history/` — compacted history
#[derive(Debug, Clone)]
pub struct PipelineDir {
    /// Root path to this pipeline directory.
    root: PathBuf,
}

impl PipelineDir {
    /// Returns the root path of this pipeline directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the path to the pipeline's `orbs.jsonl`.
    pub fn orbs_path(&self) -> PathBuf {
        self.root.join("orbs.jsonl")
    }

    /// Returns the path to the pipeline's `deps.jsonl`.
    pub fn deps_path(&self) -> PathBuf {
        self.root.join("deps.jsonl")
    }

    /// Returns the path to the pipeline's `events.jsonl`.
    pub fn events_path(&self) -> PathBuf {
        self.root.join("events.jsonl")
    }

    /// Returns the path to the `snapshots/` subdirectory.
    pub fn snapshots_dir(&self) -> PathBuf {
        self.root.join("snapshots")
    }

    /// Returns the path to the `history/` subdirectory.
    pub fn history_dir(&self) -> PathBuf {
        self.root.join("history")
    }

    /// Returns the path to the `.lock` file used for mutation safety.
    pub fn lock_path(&self) -> PathBuf {
        self.root.join(".lock")
    }

    /// Returns true if the pipeline has an active lock (incomplete mutation).
    pub fn is_locked(&self) -> bool {
        self.lock_path().exists()
    }

    /// Returns the `OrbStore` for this pipeline's local orbs.
    pub fn orb_store(&self) -> OrbStore {
        OrbStore::new(self.orbs_path())
    }
}

/// Generates the directory name for a pipeline: `<type>-<hash_prefix>`.
///
/// Uses the orb's type and the first 8 chars of its ID (after the `orb-` prefix).
fn pipeline_dir_name(orb: &Orb) -> String {
    let type_str = orb.orb_type.as_hash_str();
    let id_str = orb.id.as_str();
    // Strip the "orb-" prefix if present, use first 8 chars of the remainder
    let hash_part = id_str.strip_prefix("orb-").unwrap_or(id_str);
    let hash_prefix = if hash_part.len() > 8 {
        &hash_part[..8]
    } else {
        hash_part
    };
    format!("{type_str}-{hash_prefix}")
}

/// Creates a pipeline directory structure for the given orb.
///
/// Directory layout:
/// ```text
/// <base_dir>/pipelines/<type>-<hash>/
///   orbs.jsonl
///   deps.jsonl
///   events.jsonl
///   snapshots/
///   history/
/// ```
///
/// # Errors
///
/// Returns an IO error if directory creation or file writing fails.
pub fn create_pipeline(base_dir: &Path, orb: &Orb) -> std::io::Result<PipelineDir> {
    let dir_name = pipeline_dir_name(orb);
    let pipeline_root = base_dir.join("pipelines").join(dir_name);

    std::fs::create_dir_all(&pipeline_root)?;
    std::fs::create_dir_all(pipeline_root.join("snapshots"))?;
    std::fs::create_dir_all(pipeline_root.join("history"))?;

    // Create empty JSONL files so they exist from the start
    for filename in &["orbs.jsonl", "deps.jsonl", "events.jsonl"] {
        let path = pipeline_root.join(filename);
        if !path.exists() {
            std::fs::write(&path, "")?;
        }
    }

    Ok(PipelineDir {
        root: pipeline_root,
    })
}

/// Copies the current pipeline state to a named snapshot subdirectory.
///
/// Creates `snapshots/<phase_name>/` with copies of `orbs.jsonl`, `deps.jsonl`,
/// and `events.jsonl`. If a snapshot with a conflicting name exists, appends a
/// numeric suffix (e.g. `refinement-1`, `refinement-2`).
///
/// # Errors
///
/// Returns an IO error if file operations fail.
pub fn snapshot(pipeline: &PipelineDir, phase_name: &str) -> std::io::Result<PathBuf> {
    let snap_base = pipeline.snapshots_dir();
    let snapshot_dir = find_available_snapshot_dir(&snap_base, phase_name)?;

    std::fs::create_dir_all(&snapshot_dir)?;

    for filename in &["orbs.jsonl", "deps.jsonl", "events.jsonl"] {
        let src = pipeline.root().join(filename);
        let dst = snapshot_dir.join(filename);
        if src.exists() {
            std::fs::copy(&src, &dst)?;
        }
    }

    Ok(snapshot_dir)
}

/// Finds an available snapshot directory name, appending a numeric suffix on conflict.
fn find_available_snapshot_dir(snapshots_dir: &Path, phase_name: &str) -> std::io::Result<PathBuf> {
    let candidate = snapshots_dir.join(phase_name);
    if !candidate.exists() {
        return Ok(candidate);
    }

    // Try numeric suffixes
    for i in 1..=999 {
        let suffixed = snapshots_dir.join(format!("{phase_name}-{i}"));
        if !suffixed.exists() {
            return Ok(suffixed);
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("too many snapshots for phase {phase_name}"),
    ))
}

/// Compacts the pipeline's JSONL files: keeps only the latest state per orb
/// in `orbs.jsonl`, moves old entries to `history/`.
///
/// For deps and events, the current files are appended to history and then
/// the originals are rewritten with only active (non-removed) edges.
///
/// # Errors
///
/// Returns an IO error if file operations fail.
pub fn compact(pipeline: &PipelineDir) -> std::io::Result<()> {
    // Acquire lock
    write_lock(pipeline)?;

    let result = compact_inner(pipeline);

    // Release lock regardless of outcome
    remove_lock(pipeline)?;

    result
}

fn compact_inner(pipeline: &PipelineDir) -> std::io::Result<()> {
    let history_dir = pipeline.history_dir();
    std::fs::create_dir_all(&history_dir)?;

    let timestamp = chrono::Utc::now().format("%Y%m%d%H%M%S").to_string();

    // -- Compact orbs.jsonl --
    compact_orbs(pipeline, &history_dir, &timestamp)?;

    // -- Compact deps.jsonl --
    compact_deps(pipeline, &history_dir, &timestamp)?;

    Ok(())
}

/// Compacts orbs: archive current file to history, rewrite with latest-per-id only.
fn compact_orbs(
    pipeline: &PipelineDir,
    history_dir: &Path,
    timestamp: &str,
) -> std::io::Result<()> {
    let orbs_path = pipeline.orbs_path();
    if !orbs_path.exists() {
        return Ok(());
    }

    // Archive current state
    let archive_path = history_dir.join(format!("orbs-{timestamp}.jsonl"));
    std::fs::copy(&orbs_path, &archive_path)?;

    // Load deduplicated orbs (latest per ID) and rewrite
    let store = OrbStore::new(&orbs_path);
    let orbs = store.load_all_including_tombstoned()?;

    // Rewrite with only latest state
    let mut content = String::new();
    for orb in &orbs {
        let line = serde_json::to_string(orb)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        content.push_str(&line);
        content.push('\n');
    }
    std::fs::write(&orbs_path, content)?;

    Ok(())
}

/// Compacts deps: archive current file to history, rewrite with only active edges.
fn compact_deps(
    pipeline: &PipelineDir,
    history_dir: &Path,
    timestamp: &str,
) -> std::io::Result<()> {
    use std::io::BufRead;

    let deps_path = pipeline.deps_path();
    if !deps_path.exists() {
        return Ok(());
    }

    // Archive current state
    let archive_path = history_dir.join(format!("deps-{timestamp}.jsonl"));
    std::fs::copy(&deps_path, &archive_path)?;

    // Read all edges, keep only non-removed, deduplicate by (from, to, edge_type)
    let file = std::fs::File::open(&deps_path)?;
    let reader = std::io::BufReader::new(file);
    let mut edges: std::collections::HashMap<String, DepEdge> = std::collections::HashMap::new();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let edge: DepEdge = serde_json::from_str(&line)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let key = format!(
            "{}|{}|{:?}",
            edge.from.as_str(),
            edge.to.as_str(),
            edge.edge_type
        );
        edges.insert(key, edge);
    }

    // Rewrite with only active edges
    let mut content = String::new();
    for edge in edges.values() {
        if !edge.is_removed() {
            let line = serde_json::to_string(edge)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            content.push_str(&line);
            content.push('\n');
        }
    }
    std::fs::write(&deps_path, content)?;

    Ok(())
}

/// Writes the `.lock` file to indicate a mutation is in progress.
fn write_lock(pipeline: &PipelineDir) -> std::io::Result<()> {
    let lock_content = chrono::Utc::now().to_rfc3339();
    std::fs::write(pipeline.lock_path(), lock_content)
}

/// Removes the `.lock` file.
fn remove_lock(pipeline: &PipelineDir) -> std::io::Result<()> {
    let lock_path = pipeline.lock_path();
    if lock_path.exists() {
        std::fs::remove_file(lock_path)?;
    }
    Ok(())
}

/// Resolves the appropriate `OrbStore` for a given orb ID.
///
/// Checks the pipeline directory first (if a pipeline dir exists containing the ID);
/// falls back to the canonical store at `<base_dir>/orbs.jsonl`.
///
/// # Errors
///
/// Returns an IO error if reading fails.
pub fn resolve_store(base_dir: &Path, orb_id: &OrbId) -> std::io::Result<OrbStore> {
    let pipelines_dir = base_dir.join("pipelines");

    if pipelines_dir.exists() {
        // Scan pipeline directories for one containing this orb
        for entry in std::fs::read_dir(&pipelines_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let pipeline_orbs = entry.path().join("orbs.jsonl");
            if pipeline_orbs.exists() {
                let store = OrbStore::new(&pipeline_orbs);
                if let Some(_orb) = store.load_by_id(orb_id)? {
                    return Ok(store);
                }
            }
        }
    }

    // Fall back to canonical store
    Ok(OrbStore::new(base_dir.join("orbs.jsonl")))
}

/// Recovers a pipeline from an interrupted state.
///
/// If the `.lock` file exists, it indicates an incomplete mutation.
/// Recovery restores from the latest snapshot (by directory modification time).
///
/// Returns `true` if recovery was performed, `false` if no recovery was needed.
///
/// # Errors
///
/// Returns an IO error if file operations fail.
pub fn recover_pipeline(pipeline: &PipelineDir) -> std::io::Result<bool> {
    if !pipeline.is_locked() {
        return Ok(false);
    }

    // Find the latest snapshot
    let snapshots_dir = pipeline.snapshots_dir();
    if !snapshots_dir.exists() {
        // No snapshots to recover from — just remove the stale lock
        remove_lock(pipeline)?;
        return Ok(true);
    }

    let latest = find_latest_snapshot(&snapshots_dir)?;

    if let Some(snapshot_dir) = latest {
        // Restore files from snapshot
        for filename in &["orbs.jsonl", "deps.jsonl", "events.jsonl"] {
            let src = snapshot_dir.join(filename);
            let dst = pipeline.root().join(filename);
            if src.exists() {
                std::fs::copy(&src, &dst)?;
            }
        }
    }

    // Remove the stale lock
    remove_lock(pipeline)?;

    Ok(true)
}

/// Finds the latest snapshot directory by filesystem modification time.
fn find_latest_snapshot(snapshots_dir: &Path) -> std::io::Result<Option<PathBuf>> {
    let mut latest: Option<(PathBuf, std::time::SystemTime)> = None;

    for entry in std::fs::read_dir(snapshots_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let metadata = entry.metadata()?;
        let modified = metadata.modified()?;

        if let Some((_, best_time)) = &latest {
            if modified > *best_time {
                latest = Some((entry.path(), modified));
            }
        } else {
            latest = Some((entry.path(), modified));
        }
    }

    Ok(latest.map(|(path, _)| path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dep::EdgeType;
    use crate::orb::{OrbStatus, OrbType};

    // ── create_pipeline tests ───────────────────────────────────────

    #[test]
    fn create_pipeline_creates_directory_structure() {
        let tmp = tempfile::tempdir().unwrap();
        let orb = Orb::new("Test feature", "A test feature").with_type(OrbType::Feature);

        let pipeline = create_pipeline(tmp.path(), &orb).unwrap();

        assert!(pipeline.root().exists());
        assert!(pipeline.orbs_path().exists());
        assert!(pipeline.deps_path().exists());
        assert!(pipeline.events_path().exists());
        assert!(pipeline.snapshots_dir().exists());
        assert!(pipeline.history_dir().exists());
    }

    #[test]
    fn create_pipeline_dir_name_format() {
        let tmp = tempfile::tempdir().unwrap();
        let orb = Orb::new("My epic", "An epic thing").with_type(OrbType::Epic);

        let pipeline = create_pipeline(tmp.path(), &orb).unwrap();

        let dir_name = pipeline
            .root()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            dir_name.starts_with("epic-"),
            "Expected dir name starting with 'epic-', got: {dir_name}"
        );
    }

    #[test]
    fn create_pipeline_task_type() {
        let tmp = tempfile::tempdir().unwrap();
        let orb = Orb::new("Fix bug", "Fix the thing");

        let pipeline = create_pipeline(tmp.path(), &orb).unwrap();

        let dir_name = pipeline
            .root()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            dir_name.starts_with("task-"),
            "Expected dir name starting with 'task-', got: {dir_name}"
        );
    }

    #[test]
    fn create_pipeline_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let orb = Orb::new("Idempotent", "Should work twice");

        let p1 = create_pipeline(tmp.path(), &orb).unwrap();
        let p2 = create_pipeline(tmp.path(), &orb).unwrap();

        assert_eq!(p1.root(), p2.root());
        assert!(p1.orbs_path().exists());
    }

    // ── snapshot tests ──────────────────────────────────────────────

    #[test]
    fn snapshot_copies_current_state() {
        let tmp = tempfile::tempdir().unwrap();
        let orb = Orb::new("Snapshot test", "Test snapshotting");
        let pipeline = create_pipeline(tmp.path(), &orb).unwrap();

        // Write some data to the pipeline
        let store = pipeline.orb_store();
        store.append(&orb).unwrap();

        let snap_dir = snapshot(&pipeline, "decomposition").unwrap();

        assert!(snap_dir.exists());
        assert!(snap_dir.join("orbs.jsonl").exists());
        assert!(snap_dir.join("deps.jsonl").exists());
        assert!(snap_dir.join("events.jsonl").exists());

        // Verify the snapshot has the same content
        let snap_store = OrbStore::new(snap_dir.join("orbs.jsonl"));
        let snap_orbs = snap_store.load_all().unwrap();
        assert_eq!(snap_orbs.len(), 1);
        assert_eq!(snap_orbs[0].id, orb.id);
    }

    #[test]
    fn snapshot_handles_duplicate_names() {
        let tmp = tempfile::tempdir().unwrap();
        let orb = Orb::new("Dupe snap", "Test duplicate snapshots");
        let pipeline = create_pipeline(tmp.path(), &orb).unwrap();

        let snap1 = snapshot(&pipeline, "refinement").unwrap();
        let snap2 = snapshot(&pipeline, "refinement").unwrap();
        let snap3 = snapshot(&pipeline, "refinement").unwrap();

        assert!(snap1.ends_with("refinement"));
        assert!(snap2.ends_with("refinement-1"));
        assert!(snap3.ends_with("refinement-2"));
    }

    // ── compact tests ───────────────────────────────────────────────

    #[test]
    fn compact_deduplicates_orbs() {
        let tmp = tempfile::tempdir().unwrap();
        let mut orb = Orb::new("Compact me", "Test compaction");
        let pipeline = create_pipeline(tmp.path(), &orb).unwrap();

        let store = pipeline.orb_store();

        // Write multiple versions
        store.append(&orb).unwrap();
        orb.set_status(OrbStatus::Active);
        store.append(&orb).unwrap();
        orb.set_status(OrbStatus::Done);
        orb.result = Some("done".into());
        store.append(&orb).unwrap();

        // Before compaction: 3 lines
        let raw_before = std::fs::read_to_string(pipeline.orbs_path()).unwrap();
        assert_eq!(raw_before.lines().count(), 3);

        compact(&pipeline).unwrap();

        // After compaction: 1 line (latest state only)
        let raw_after = std::fs::read_to_string(pipeline.orbs_path()).unwrap();
        let non_empty: Vec<&str> = raw_after.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(non_empty.len(), 1);

        // The remaining orb should have the final state
        let orbs = store.load_all().unwrap();
        assert_eq!(orbs.len(), 1);
        assert_eq!(orbs[0].result.as_deref(), Some("done"));
    }

    #[test]
    fn compact_archives_to_history() {
        let tmp = tempfile::tempdir().unwrap();
        let orb = Orb::new("Archive me", "Test archiving");
        let pipeline = create_pipeline(tmp.path(), &orb).unwrap();

        let store = pipeline.orb_store();
        store.append(&orb).unwrap();

        compact(&pipeline).unwrap();

        // History directory should have an archived file
        let history_entries: Vec<_> = std::fs::read_dir(pipeline.history_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            !history_entries.is_empty(),
            "Expected history entries after compaction"
        );

        // At least one orbs-*.jsonl file
        let has_orbs_archive = history_entries
            .iter()
            .any(|e| e.file_name().to_str().unwrap_or("").starts_with("orbs-"));
        assert!(has_orbs_archive, "Expected orbs archive in history");
    }

    #[test]
    fn compact_removes_dead_deps() {
        let tmp = tempfile::tempdir().unwrap();
        let orb = Orb::new("Dep compact", "Test dep compaction");
        let pipeline = create_pipeline(tmp.path(), &orb).unwrap();

        // Write active and removed edges
        let edge_active = DepEdge::new(
            OrbId::from_raw("orb-a"),
            OrbId::from_raw("orb-b"),
            EdgeType::Blocks,
        );
        let mut edge_removed = DepEdge::new(
            OrbId::from_raw("orb-c"),
            OrbId::from_raw("orb-d"),
            EdgeType::DependsOn,
        );
        edge_removed.remove();

        // Write both edges
        let deps_path = pipeline.deps_path();
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&deps_path)
                .unwrap();
            writeln!(file, "{}", serde_json::to_string(&edge_active).unwrap()).unwrap();
            writeln!(file, "{}", serde_json::to_string(&edge_removed).unwrap()).unwrap();
        }

        compact(&pipeline).unwrap();

        // After compaction: only the active edge remains
        let raw = std::fs::read_to_string(&deps_path).unwrap();
        let non_empty: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(non_empty.len(), 1);

        let remaining: DepEdge = serde_json::from_str(non_empty[0]).unwrap();
        assert_eq!(remaining.from, OrbId::from_raw("orb-a"));
        assert!(!remaining.is_removed());
    }

    #[test]
    fn compact_removes_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let orb = Orb::new("Lock test", "Test lock removal");
        let pipeline = create_pipeline(tmp.path(), &orb).unwrap();

        let store = pipeline.orb_store();
        store.append(&orb).unwrap();

        compact(&pipeline).unwrap();

        assert!(
            !pipeline.is_locked(),
            "Lock should be removed after compaction"
        );
    }

    // ── store routing tests ─────────────────────────────────────────

    #[test]
    fn resolve_store_finds_orb_in_pipeline() {
        let tmp = tempfile::tempdir().unwrap();
        let orb = Orb::new("Pipeline orb", "Lives in pipeline");
        let pipeline = create_pipeline(tmp.path(), &orb).unwrap();

        // Add orb to pipeline store
        let pstore = pipeline.orb_store();
        pstore.append(&orb).unwrap();

        // resolve_store should find it in the pipeline
        let resolved = resolve_store(tmp.path(), &orb.id).unwrap();
        let found = resolved.load_by_id(&orb.id).unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().title, "Pipeline orb");
    }

    #[test]
    fn resolve_store_falls_back_to_canonical() {
        let tmp = tempfile::tempdir().unwrap();
        let orb = Orb::new("Canonical orb", "Lives in canonical store");

        // Add orb to canonical store (not in any pipeline)
        let canonical = OrbStore::new(tmp.path().join("orbs.jsonl"));
        canonical.append(&orb).unwrap();

        let resolved = resolve_store(tmp.path(), &orb.id).unwrap();
        let found = resolved.load_by_id(&orb.id).unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().title, "Canonical orb");
    }

    #[test]
    fn resolve_store_returns_canonical_when_no_pipelines() {
        let tmp = tempfile::tempdir().unwrap();
        let id = OrbId::from_raw("orb-nonexistent");

        let resolved = resolve_store(tmp.path(), &id).unwrap();
        // Should get the canonical store (even if empty)
        assert_eq!(resolved.path(), tmp.path().join("orbs.jsonl"));
    }

    // ── interruption recovery tests ─────────────────────────────────

    #[test]
    fn recover_noop_when_no_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let orb = Orb::new("No lock", "No recovery needed");
        let pipeline = create_pipeline(tmp.path(), &orb).unwrap();

        let recovered = recover_pipeline(&pipeline).unwrap();
        assert!(!recovered);
    }

    #[test]
    fn recover_restores_from_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let orb = Orb::new("Recover me", "Test recovery");
        let pipeline = create_pipeline(tmp.path(), &orb).unwrap();

        // Add orb and take a snapshot
        let store = pipeline.orb_store();
        store.append(&orb).unwrap();
        snapshot(&pipeline, "pre-crash").unwrap();

        // Simulate a crash: corrupt the orbs file and leave a lock
        std::fs::write(pipeline.orbs_path(), "CORRUPTED\n").unwrap();
        std::fs::write(pipeline.lock_path(), "2025-01-01T00:00:00Z").unwrap();
        assert!(pipeline.is_locked());

        // Recovery should restore from the snapshot
        let recovered = recover_pipeline(&pipeline).unwrap();
        assert!(recovered);
        assert!(!pipeline.is_locked());

        // Orbs should be restored from the snapshot
        let restored = store.load_all().unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].id, orb.id);
    }

    #[test]
    fn recover_removes_stale_lock_without_snapshots() {
        let tmp = tempfile::tempdir().unwrap();
        let orb = Orb::new("Stale lock", "Lock but no snapshots");
        let pipeline = create_pipeline(tmp.path(), &orb).unwrap();

        // Create a lock without any snapshots
        std::fs::write(pipeline.lock_path(), "2025-01-01T00:00:00Z").unwrap();
        assert!(pipeline.is_locked());

        let recovered = recover_pipeline(&pipeline).unwrap();
        assert!(recovered);
        assert!(!pipeline.is_locked());
    }

    #[test]
    fn recover_uses_latest_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let mut orb = Orb::new("Multi snap", "Multiple snapshots");
        let pipeline = create_pipeline(tmp.path(), &orb).unwrap();

        let store = pipeline.orb_store();

        // First state + snapshot
        store.append(&orb).unwrap();
        snapshot(&pipeline, "early").unwrap();

        // Second state + snapshot (this should be "latest")
        orb.set_status(OrbStatus::Active);
        orb.result = Some("active state".into());
        // Rewrite the pipeline orbs file entirely for the new state
        std::fs::write(pipeline.orbs_path(), "").unwrap();
        store.append(&orb).unwrap();

        // Small delay to ensure filesystem modification time differs
        std::thread::sleep(std::time::Duration::from_millis(50));
        snapshot(&pipeline, "later").unwrap();

        // Simulate crash
        std::fs::write(pipeline.orbs_path(), "CORRUPTED\n").unwrap();
        std::fs::write(pipeline.lock_path(), "lock").unwrap();

        let recovered = recover_pipeline(&pipeline).unwrap();
        assert!(recovered);

        let restored = store.load_all().unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].result.as_deref(), Some("active state"));
    }

    // ── lock mechanism tests ────────────────────────────────────────

    #[test]
    fn lock_file_during_compact() {
        let tmp = tempfile::tempdir().unwrap();
        let orb = Orb::new("Lock during compact", "Verify lock behavior");
        let pipeline = create_pipeline(tmp.path(), &orb).unwrap();

        let store = pipeline.orb_store();
        store.append(&orb).unwrap();

        // Before compact: no lock
        assert!(!pipeline.is_locked());

        // After compact: lock should be cleaned up
        compact(&pipeline).unwrap();
        assert!(!pipeline.is_locked());
    }

    // ── edge cases ──────────────────────────────────────────────────

    #[test]
    fn compact_empty_pipeline() {
        let tmp = tempfile::tempdir().unwrap();
        let orb = Orb::new("Empty pipeline", "Nothing to compact");
        let pipeline = create_pipeline(tmp.path(), &orb).unwrap();

        // Compact with empty files should not error
        compact(&pipeline).unwrap();
    }

    #[test]
    fn pipeline_dir_paths_are_correct() {
        let tmp = tempfile::tempdir().unwrap();
        let orb = Orb::new("Paths test", "Check all paths");
        let pipeline = create_pipeline(tmp.path(), &orb).unwrap();

        assert_eq!(pipeline.orbs_path(), pipeline.root().join("orbs.jsonl"));
        assert_eq!(pipeline.deps_path(), pipeline.root().join("deps.jsonl"));
        assert_eq!(pipeline.events_path(), pipeline.root().join("events.jsonl"));
        assert_eq!(pipeline.snapshots_dir(), pipeline.root().join("snapshots"));
        assert_eq!(pipeline.history_dir(), pipeline.root().join("history"));
        assert_eq!(pipeline.lock_path(), pipeline.root().join(".lock"));
    }

    #[test]
    fn custom_orb_type_pipeline_name() {
        let tmp = tempfile::tempdir().unwrap();
        let orb =
            Orb::new("Research", "Research task").with_type(OrbType::Custom("research".into()));
        let pipeline = create_pipeline(tmp.path(), &orb).unwrap();

        let dir_name = pipeline
            .root()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            dir_name.starts_with("research-"),
            "Expected dir name starting with 'research-', got: {dir_name}"
        );
    }
}

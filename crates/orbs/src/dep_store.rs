use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use crate::dep::{DepEdge, EdgeType};
use crate::id::OrbId;
use crate::orb::Orb;
use crate::task::TaskStatus;

/// Error type for dependency operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepError {
    /// Adding this edge would create a cycle in the dependency graph.
    CycleDetected {
        /// The path forming the cycle, from source back to source.
        cycle: Vec<OrbId>,
    },
    /// An IO error occurred.
    Io(String),
}

impl std::fmt::Display for DepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CycleDetected { cycle } => {
                write!(f, "cycle detected: ")?;
                for (i, id) in cycle.iter().enumerate() {
                    if i > 0 {
                        write!(f, " -> ")?;
                    }
                    write!(f, "{id}")?;
                }
                Ok(())
            }
            Self::Io(msg) => write!(f, "io error: {msg}"),
        }
    }
}

impl std::error::Error for DepError {}

impl From<std::io::Error> for DepError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

/// Append-only JSONL store for dependency edges.
///
/// Each mutation (add or remove) is appended as a full JSON line.
/// Reading replays the log and deduplicates by (from, to, `edge_type`),
/// keeping the latest version.
#[derive(Clone)]
pub struct DepStore {
    path: PathBuf,
}

impl DepStore {
    /// Opens or creates a JSONL dep store at the given path.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Returns the path to the store file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends a raw edge to the store file.
    fn append_raw(&self, edge: &DepEdge) -> Result<(), DepError> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let mut line = serde_json::to_string(edge)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        file.write_all(line.as_bytes())?;
        Ok(())
    }

    /// Adds a dependency edge, checking for cycles on blocking edge types.
    ///
    /// Returns `DepError::CycleDetected` if the edge would create a cycle
    /// in the blocking dependency graph (`Blocks`/`DependsOn` edges).
    ///
    /// # Errors
    ///
    /// Returns `DepError::CycleDetected` on cycle, or `DepError::Io` on write failure.
    #[allow(clippy::needless_pass_by_value)]
    pub fn add_edge(&self, edge: DepEdge) -> Result<(), DepError> {
        if edge.edge_type.is_blocking() {
            let existing = self.all_edges()?;
            detect_cycle_with_new_edge(&existing, &edge)?;
        }
        self.append_raw(&edge)
    }

    /// Removes an edge by marking it as removed (soft delete).
    ///
    /// Finds the matching active edge and appends a tombstoned version.
    ///
    /// # Errors
    ///
    /// Returns `DepError::Io` on read/write failure.
    pub fn remove_edge(
        &self,
        from: &OrbId,
        to: &OrbId,
        edge_type: EdgeType,
    ) -> Result<bool, DepError> {
        let edges = self.all_edges()?;
        let found = edges
            .iter()
            .find(|e| e.from == *from && e.to == *to && e.edge_type == edge_type);

        if let Some(edge) = found {
            let mut removed = edge.clone();
            removed.remove();
            self.append_raw(&removed)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Returns all active (non-removed) edges originating from the given orb.
    ///
    /// # Errors
    ///
    /// Returns `DepError::Io` on read failure.
    pub fn edges_from(&self, id: &OrbId) -> Result<Vec<DepEdge>, DepError> {
        Ok(self
            .all_edges()?
            .into_iter()
            .filter(|e| e.from == *id)
            .collect())
    }

    /// Returns all active (non-removed) edges targeting the given orb.
    ///
    /// # Errors
    ///
    /// Returns `DepError::Io` on read failure.
    pub fn edges_to(&self, id: &OrbId) -> Result<Vec<DepEdge>, DepError> {
        Ok(self
            .all_edges()?
            .into_iter()
            .filter(|e| e.to == *id)
            .collect())
    }

    /// Returns all active (non-removed) edges.
    ///
    /// # Errors
    ///
    /// Returns `DepError::Io` on read failure.
    pub fn all_edges(&self) -> Result<Vec<DepEdge>, DepError> {
        use std::io::BufRead;

        if !self.path.exists() {
            return Ok(vec![]);
        }

        let file = std::fs::File::open(&self.path)?;
        let reader = std::io::BufReader::new(file);

        // Deduplicate by (from, to, edge_type), keeping latest version.
        let mut edges: HashMap<(String, String, EdgeType), DepEdge> = HashMap::new();

        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let edge: DepEdge = serde_json::from_str(&line)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            let key = (
                edge.from.as_str().to_string(),
                edge.to.as_str().to_string(),
                edge.edge_type,
            );
            edges.insert(key, edge);
        }

        // Filter out removed edges
        Ok(edges.into_values().filter(|e| !e.is_removed()).collect())
    }

    /// Returns orbs in pipeline order (topological sort respecting blocking deps).
    ///
    /// Orbs with no blocking dependencies come first. If multiple orbs are
    /// ready at the same level, they are sorted by effective priority (ascending,
    /// i.e. priority 1/Critical first).
    ///
    /// # Errors
    ///
    /// Returns `DepError::Io` on read failure.
    pub fn pipeline(&self, orbs: &[Orb]) -> Result<Vec<OrbId>, DepError> {
        let edges = self.all_edges()?;
        let orb_map: HashMap<&str, &Orb> = orbs.iter().map(|o| (o.id.as_str(), o)).collect();
        let orb_ids: HashSet<&str> = orb_map.keys().copied().collect();

        // Build adjacency for blocking edges only (among provided orbs).
        // For "Blocks": from blocks to => to depends on from => from must come before to.
        // For "DependsOn": from depends on to => to must come before from.
        // We need: predecessors map for topological sort.
        let mut in_degree: HashMap<&str, usize> = HashMap::new();
        let mut successors: HashMap<&str, Vec<&str>> = HashMap::new();

        for id in &orb_ids {
            in_degree.entry(id).or_insert(0);
        }

        for edge in &edges {
            let from_str = edge.from.as_str();
            let to_str = edge.to.as_str();
            if !orb_ids.contains(from_str) || !orb_ids.contains(to_str) {
                continue;
            }
            if !edge.edge_type.is_blocking() {
                continue;
            }

            match edge.edge_type {
                EdgeType::Blocks => {
                    // from blocks to: from must come before to
                    successors.entry(from_str).or_default().push(to_str);
                    *in_degree.entry(to_str).or_insert(0) += 1;
                }
                EdgeType::DependsOn => {
                    // from depends on to: to must come before from
                    successors.entry(to_str).or_default().push(from_str);
                    *in_degree.entry(from_str).or_insert(0) += 1;
                }
                _ => {}
            }
        }

        // Compute effective priorities (with propagation).
        let effective_priorities = compute_effective_priorities(orbs, &edges);

        // Kahn's algorithm with priority-based tie-breaking.
        let mut queue: VecDeque<&str> = VecDeque::new();
        let mut ready: Vec<&str> = in_degree
            .iter()
            .filter(|(_, &deg)| deg == 0)
            .map(|(&id, _)| id)
            .collect();
        ready.sort_by_key(|id| effective_priorities.get(*id).copied().unwrap_or(3));
        for id in ready {
            queue.push_back(id);
        }

        let mut result = Vec::new();
        let mut visited = HashSet::new();

        while let Some(id) = queue.pop_front() {
            if !visited.insert(id) {
                continue;
            }
            result.push(OrbId::from_raw(id));

            let mut next_ready = Vec::new();
            if let Some(succs) = successors.get(id) {
                for &succ in succs {
                    if let Some(deg) = in_degree.get_mut(succ) {
                        *deg = deg.saturating_sub(1);
                        if *deg == 0 && !visited.contains(succ) {
                            next_ready.push(succ);
                        }
                    }
                }
            }
            next_ready.sort_by_key(|id| effective_priorities.get(*id).copied().unwrap_or(3));
            for id in next_ready {
                queue.push_back(id);
            }
        }

        // Add any orbs not in the graph (no edges) that weren't visited.
        for id in &orb_ids {
            if !visited.contains(*id) {
                result.push(OrbId::from_raw(*id));
            }
        }

        Ok(result)
    }

    /// Returns orbs whose blocking dependencies are all done.
    ///
    /// An orb is "ready" if:
    /// - It is not done/cancelled/failed itself
    /// - All orbs that block it (via `Blocks` or `DependsOn` edges) have effective status Done
    ///
    /// # Errors
    ///
    /// Returns `DepError::Io` on read failure.
    pub fn ready(&self, orbs: &[Orb]) -> Result<Vec<OrbId>, DepError> {
        let edges = self.all_edges()?;
        let orb_map: HashMap<&str, &Orb> = orbs.iter().map(|o| (o.id.as_str(), o)).collect();

        // Build set of blockers for each orb.
        let mut blockers: HashMap<&str, Vec<&str>> = HashMap::new();

        for edge in &edges {
            if !edge.edge_type.is_blocking() {
                continue;
            }
            let from_str = edge.from.as_str();
            let to_str = edge.to.as_str();

            match edge.edge_type {
                EdgeType::Blocks => {
                    // from blocks to => to is blocked by from
                    if orb_map.contains_key(to_str) {
                        blockers.entry(to_str).or_default().push(from_str);
                    }
                }
                EdgeType::DependsOn => {
                    // from depends on to => from is blocked by to
                    if orb_map.contains_key(from_str) {
                        blockers.entry(from_str).or_default().push(to_str);
                    }
                }
                _ => {}
            }
        }

        let mut ready = Vec::new();
        for orb in orbs {
            let status = orb.effective_status();
            if matches!(
                status,
                TaskStatus::Done | TaskStatus::Failed | TaskStatus::Cancelled
            ) {
                continue;
            }

            let all_blockers_done = blockers.get(orb.id.as_str()).is_none_or(|deps| {
                deps.iter().all(|dep_id| {
                    orb_map
                        .get(dep_id)
                        .is_none_or(|o| o.effective_status() == TaskStatus::Done)
                })
            });

            if all_blockers_done {
                ready.push(orb.id.clone());
            }
        }

        Ok(ready)
    }

    /// Returns orbs that are blocked by at least one incomplete dependency.
    ///
    /// # Errors
    ///
    /// Returns `DepError::Io` on read failure.
    pub fn waiting(&self, orbs: &[Orb]) -> Result<Vec<OrbId>, DepError> {
        let edges = self.all_edges()?;
        let orb_map: HashMap<&str, &Orb> = orbs.iter().map(|o| (o.id.as_str(), o)).collect();

        let mut blockers: HashMap<&str, Vec<&str>> = HashMap::new();

        for edge in &edges {
            if !edge.edge_type.is_blocking() {
                continue;
            }
            let from_str = edge.from.as_str();
            let to_str = edge.to.as_str();

            match edge.edge_type {
                EdgeType::Blocks => {
                    if orb_map.contains_key(to_str) {
                        blockers.entry(to_str).or_default().push(from_str);
                    }
                }
                EdgeType::DependsOn => {
                    if orb_map.contains_key(from_str) {
                        blockers.entry(from_str).or_default().push(to_str);
                    }
                }
                _ => {}
            }
        }

        let mut waiting = Vec::new();
        for orb in orbs {
            let status = orb.effective_status();
            if matches!(
                status,
                TaskStatus::Done | TaskStatus::Failed | TaskStatus::Cancelled
            ) {
                continue;
            }

            let has_incomplete_blocker = blockers.get(orb.id.as_str()).is_some_and(|deps| {
                deps.iter().any(|dep_id| {
                    orb_map
                        .get(dep_id)
                        .is_some_and(|o| o.effective_status() != TaskStatus::Done)
                })
            });

            if has_incomplete_blocker {
                waiting.push(orb.id.clone());
            }
        }

        Ok(waiting)
    }

    /// Computes effective priorities for all orbs, propagating highest
    /// priority from transitive dependents.
    ///
    /// If orb A (priority 1) depends on orb B (priority 3), then B's
    /// effective priority becomes 1.
    ///
    /// # Errors
    ///
    /// Returns `DepError::Io` on read failure.
    pub fn effective_priorities(&self, orbs: &[Orb]) -> Result<HashMap<String, u8>, DepError> {
        let edges = self.all_edges()?;
        Ok(compute_effective_priorities(orbs, &edges))
    }
}

/// Detects if adding `new_edge` would create a cycle in the blocking dep graph.
///
/// Only considers blocking edges (`Blocks`, `DependsOn`).
fn detect_cycle_with_new_edge(existing: &[DepEdge], new_edge: &DepEdge) -> Result<(), DepError> {
    // Build adjacency for the directed blocking graph.
    // Blocks: from -> to (from must come before to)
    // DependsOn: to -> from (to must come before from)
    // We check if adding the new edge creates a path from target back to source.
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();

    for edge in existing {
        if !edge.edge_type.is_blocking() {
            continue;
        }
        match edge.edge_type {
            EdgeType::Blocks => {
                adj.entry(edge.from.as_str())
                    .or_default()
                    .push(edge.to.as_str());
            }
            EdgeType::DependsOn => {
                adj.entry(edge.to.as_str())
                    .or_default()
                    .push(edge.from.as_str());
            }
            _ => {}
        }
    }

    // Add the new edge.
    let (new_src, new_dst) = match new_edge.edge_type {
        EdgeType::Blocks => (new_edge.from.as_str(), new_edge.to.as_str()),
        EdgeType::DependsOn => (new_edge.to.as_str(), new_edge.from.as_str()),
        _ => return Ok(()),
    };

    adj.entry(new_src).or_default().push(new_dst);

    // BFS from new_dst to see if we can reach new_src (that would mean a cycle).
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    let mut parent: HashMap<&str, &str> = HashMap::new();
    queue.push_back(new_dst);
    visited.insert(new_dst);

    while let Some(current) = queue.pop_front() {
        if current == new_src {
            // Reconstruct cycle path.
            let mut cycle = vec![OrbId::from_raw(current)];
            let mut node = current;
            while let Some(&prev) = parent.get(node) {
                cycle.push(OrbId::from_raw(prev));
                node = prev;
                if node == current {
                    break;
                }
            }
            cycle.push(OrbId::from_raw(new_src));
            cycle.reverse();
            return Err(DepError::CycleDetected { cycle });
        }
        if let Some(neighbors) = adj.get(current) {
            for &next in neighbors {
                if visited.insert(next) {
                    parent.insert(next, current);
                    queue.push_back(next);
                }
            }
        }
    }

    Ok(())
}

/// Computes effective priorities by propagating highest priority
/// (lowest number) from transitive dependents upstream through blocking edges.
fn compute_effective_priorities(orbs: &[Orb], edges: &[DepEdge]) -> HashMap<String, u8> {
    let orb_map: HashMap<&str, &Orb> = orbs.iter().map(|o| (o.id.as_str(), o)).collect();

    // Start with each orb's own priority.
    let mut priorities: HashMap<String, u8> = orbs
        .iter()
        .map(|o| (o.id.as_str().to_string(), o.priority))
        .collect();

    // Build "reverse blocking" adjacency:
    // If A blocks B, then B's priority should propagate to A.
    // If A depends_on B, then A's priority should propagate to B.
    // We want: for each orb, which orbs depend on it (directly)?
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();

    for edge in edges {
        if !edge.edge_type.is_blocking() {
            continue;
        }
        match edge.edge_type {
            EdgeType::Blocks => {
                // from blocks to: to depends on from, so to is a dependent of from
                dependents
                    .entry(edge.from.as_str())
                    .or_default()
                    .push(edge.to.as_str());
            }
            EdgeType::DependsOn => {
                // from depends on to: from is a dependent of to
                dependents
                    .entry(edge.to.as_str())
                    .or_default()
                    .push(edge.from.as_str());
            }
            _ => {}
        }
    }

    // Iterative propagation: repeat until stable.
    // For each orb, effective_priority = min(own_priority, min(effective_priority of all transitive dependents)).
    // We do this with BFS from each orb through its dependents.
    let all_ids: Vec<&str> = orb_map.keys().copied().collect();

    for &id in &all_ids {
        // BFS through dependents to find highest priority (lowest number).
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(id);
        visited.insert(id);

        let mut min_priority = orb_map.get(id).map_or(3, |o| o.priority);

        while let Some(current) = queue.pop_front() {
            if let Some(deps) = dependents.get(current) {
                for &dep in deps {
                    if visited.insert(dep) {
                        if let Some(&p) = priorities.get(dep) {
                            min_priority = min_priority.min(p);
                        }
                        queue.push_back(dep);
                    }
                }
            }
        }

        priorities
            .entry(id.to_string())
            .and_modify(|p| *p = (*p).min(min_priority));
    }

    priorities
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orb::{Orb, OrbStatus};

    fn make_orb(id: &str, priority: u8) -> Orb {
        let mut orb = Orb::new(format!("Orb {id}"), format!("Description for {id}"));
        orb.id = OrbId::from_raw(id);
        orb.priority = priority.clamp(1, 5);
        orb
    }

    fn make_done_orb(id: &str) -> Orb {
        let mut orb = make_orb(id, 3);
        orb.set_status(OrbStatus::Done);
        orb
    }

    // ── Edge CRUD ────────────────────────────────────────────

    #[test]
    fn add_and_load_edges() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepStore::new(dir.path().join("deps.jsonl"));

        let e1 = DepEdge::new(
            OrbId::from_raw("orb-a"),
            OrbId::from_raw("orb-b"),
            EdgeType::Blocks,
        );
        let e2 = DepEdge::new(
            OrbId::from_raw("orb-b"),
            OrbId::from_raw("orb-c"),
            EdgeType::DependsOn,
        );

        store.add_edge(e1).unwrap();
        store.add_edge(e2).unwrap();

        let all = store.all_edges().unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn edges_from_filters_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepStore::new(dir.path().join("deps.jsonl"));

        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-a"),
                OrbId::from_raw("orb-b"),
                EdgeType::Blocks,
            ))
            .unwrap();
        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-a"),
                OrbId::from_raw("orb-c"),
                EdgeType::Related,
            ))
            .unwrap();
        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-b"),
                OrbId::from_raw("orb-c"),
                EdgeType::Follows,
            ))
            .unwrap();

        let from_a = store.edges_from(&OrbId::from_raw("orb-a")).unwrap();
        assert_eq!(from_a.len(), 2);

        let from_b = store.edges_from(&OrbId::from_raw("orb-b")).unwrap();
        assert_eq!(from_b.len(), 1);
    }

    #[test]
    fn edges_to_filters_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepStore::new(dir.path().join("deps.jsonl"));

        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-a"),
                OrbId::from_raw("orb-c"),
                EdgeType::Blocks,
            ))
            .unwrap();
        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-b"),
                OrbId::from_raw("orb-c"),
                EdgeType::DependsOn,
            ))
            .unwrap();

        let to_c = store.edges_to(&OrbId::from_raw("orb-c")).unwrap();
        assert_eq!(to_c.len(), 2);

        let to_a = store.edges_to(&OrbId::from_raw("orb-a")).unwrap();
        assert_eq!(to_a.len(), 0);
    }

    #[test]
    fn remove_edge_soft_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepStore::new(dir.path().join("deps.jsonl"));

        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-a"),
                OrbId::from_raw("orb-b"),
                EdgeType::Blocks,
            ))
            .unwrap();
        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-a"),
                OrbId::from_raw("orb-c"),
                EdgeType::Related,
            ))
            .unwrap();

        let removed = store
            .remove_edge(
                &OrbId::from_raw("orb-a"),
                &OrbId::from_raw("orb-b"),
                EdgeType::Blocks,
            )
            .unwrap();
        assert!(removed);

        let all = store.all_edges().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].to, OrbId::from_raw("orb-c"));
    }

    #[test]
    fn remove_nonexistent_edge_returns_false() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepStore::new(dir.path().join("deps.jsonl"));

        let removed = store
            .remove_edge(
                &OrbId::from_raw("orb-x"),
                &OrbId::from_raw("orb-y"),
                EdgeType::Blocks,
            )
            .unwrap();
        assert!(!removed);
    }

    #[test]
    fn empty_store_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepStore::new(dir.path().join("nonexistent.jsonl"));
        assert!(store.all_edges().unwrap().is_empty());
    }

    // ── Cycle detection ──────────────────────────────────────

    #[test]
    fn direct_cycle_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepStore::new(dir.path().join("deps.jsonl"));

        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-a"),
                OrbId::from_raw("orb-b"),
                EdgeType::Blocks,
            ))
            .unwrap();

        // orb-b blocks orb-a would create: a->b->a
        let result = store.add_edge(DepEdge::new(
            OrbId::from_raw("orb-b"),
            OrbId::from_raw("orb-a"),
            EdgeType::Blocks,
        ));
        assert!(result.is_err());
        if let Err(DepError::CycleDetected { cycle }) = result {
            assert!(cycle.len() >= 2);
        }
    }

    #[test]
    fn transitive_cycle_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepStore::new(dir.path().join("deps.jsonl"));

        // a blocks b, b blocks c
        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-a"),
                OrbId::from_raw("orb-b"),
                EdgeType::Blocks,
            ))
            .unwrap();
        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-b"),
                OrbId::from_raw("orb-c"),
                EdgeType::Blocks,
            ))
            .unwrap();

        // c blocks a would create: a->b->c->a
        let result = store.add_edge(DepEdge::new(
            OrbId::from_raw("orb-c"),
            OrbId::from_raw("orb-a"),
            EdgeType::Blocks,
        ));
        assert!(result.is_err());
    }

    #[test]
    fn cycle_detection_with_depends_on() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepStore::new(dir.path().join("deps.jsonl"));

        // a depends_on b (b must come before a)
        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-a"),
                OrbId::from_raw("orb-b"),
                EdgeType::DependsOn,
            ))
            .unwrap();

        // b depends_on a would create cycle
        let result = store.add_edge(DepEdge::new(
            OrbId::from_raw("orb-b"),
            OrbId::from_raw("orb-a"),
            EdgeType::DependsOn,
        ));
        assert!(result.is_err());
    }

    #[test]
    fn mixed_blocks_depends_on_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepStore::new(dir.path().join("deps.jsonl"));

        // a blocks b (a must come before b)
        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-a"),
                OrbId::from_raw("orb-b"),
                EdgeType::Blocks,
            ))
            .unwrap();

        // a depends_on b would mean b must come before a, creating cycle
        let result = store.add_edge(DepEdge::new(
            OrbId::from_raw("orb-a"),
            OrbId::from_raw("orb-b"),
            EdgeType::DependsOn,
        ));
        assert!(result.is_err());
    }

    #[test]
    fn non_blocking_edges_skip_cycle_check() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepStore::new(dir.path().join("deps.jsonl"));

        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-a"),
                OrbId::from_raw("orb-b"),
                EdgeType::Related,
            ))
            .unwrap();

        // Related back is fine — no cycle check for non-blocking.
        let result = store.add_edge(DepEdge::new(
            OrbId::from_raw("orb-b"),
            OrbId::from_raw("orb-a"),
            EdgeType::Related,
        ));
        assert!(result.is_ok());
    }

    #[test]
    fn self_loop_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepStore::new(dir.path().join("deps.jsonl"));

        let result = store.add_edge(DepEdge::new(
            OrbId::from_raw("orb-a"),
            OrbId::from_raw("orb-a"),
            EdgeType::Blocks,
        ));
        assert!(result.is_err());
    }

    // ── Ready / Waiting / Pipeline ───────────────────────────

    #[test]
    fn ready_with_no_deps() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepStore::new(dir.path().join("deps.jsonl"));

        let orbs = vec![make_orb("orb-a", 3), make_orb("orb-b", 2)];
        let ready = store.ready(&orbs).unwrap();
        assert_eq!(ready.len(), 2);
    }

    #[test]
    fn ready_excludes_blocked_orbs() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepStore::new(dir.path().join("deps.jsonl"));

        // a blocks b
        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-a"),
                OrbId::from_raw("orb-b"),
                EdgeType::Blocks,
            ))
            .unwrap();

        let orbs = vec![make_orb("orb-a", 3), make_orb("orb-b", 2)];
        let ready = store.ready(&orbs).unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0], OrbId::from_raw("orb-a"));
    }

    #[test]
    fn ready_includes_orb_when_blocker_done() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepStore::new(dir.path().join("deps.jsonl"));

        // a blocks b
        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-a"),
                OrbId::from_raw("orb-b"),
                EdgeType::Blocks,
            ))
            .unwrap();

        let orbs = vec![make_done_orb("orb-a"), make_orb("orb-b", 2)];
        let ready = store.ready(&orbs).unwrap();
        // a is done (excluded from ready), b is now unblocked
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0], OrbId::from_raw("orb-b"));
    }

    #[test]
    fn ready_with_depends_on() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepStore::new(dir.path().join("deps.jsonl"));

        // b depends_on a
        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-b"),
                OrbId::from_raw("orb-a"),
                EdgeType::DependsOn,
            ))
            .unwrap();

        let orbs = vec![make_orb("orb-a", 3), make_orb("orb-b", 2)];
        let ready = store.ready(&orbs).unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0], OrbId::from_raw("orb-a"));
    }

    #[test]
    fn waiting_returns_blocked_orbs() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepStore::new(dir.path().join("deps.jsonl"));

        // a blocks b, a blocks c
        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-a"),
                OrbId::from_raw("orb-b"),
                EdgeType::Blocks,
            ))
            .unwrap();
        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-a"),
                OrbId::from_raw("orb-c"),
                EdgeType::Blocks,
            ))
            .unwrap();

        let orbs = vec![
            make_orb("orb-a", 3),
            make_orb("orb-b", 2),
            make_orb("orb-c", 1),
        ];
        let waiting = store.waiting(&orbs).unwrap();
        assert_eq!(waiting.len(), 2);
        let waiting_ids: HashSet<_> = waiting.iter().map(|id| id.as_str().to_string()).collect();
        assert!(waiting_ids.contains("orb-b"));
        assert!(waiting_ids.contains("orb-c"));
    }

    #[test]
    fn waiting_empty_when_all_blockers_done() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepStore::new(dir.path().join("deps.jsonl"));

        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-a"),
                OrbId::from_raw("orb-b"),
                EdgeType::Blocks,
            ))
            .unwrap();

        let orbs = vec![make_done_orb("orb-a"), make_orb("orb-b", 2)];
        let waiting = store.waiting(&orbs).unwrap();
        assert!(waiting.is_empty());
    }

    #[test]
    fn pipeline_respects_blocking_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepStore::new(dir.path().join("deps.jsonl"));

        // a blocks b, b blocks c
        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-a"),
                OrbId::from_raw("orb-b"),
                EdgeType::Blocks,
            ))
            .unwrap();
        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-b"),
                OrbId::from_raw("orb-c"),
                EdgeType::Blocks,
            ))
            .unwrap();

        let orbs = vec![
            make_orb("orb-c", 3),
            make_orb("orb-a", 3),
            make_orb("orb-b", 3),
        ];
        let pipeline = store.pipeline(&orbs).unwrap();
        let positions: HashMap<String, usize> = pipeline
            .iter()
            .enumerate()
            .map(|(i, id)| (id.as_str().to_string(), i))
            .collect();

        assert!(positions["orb-a"] < positions["orb-b"]);
        assert!(positions["orb-b"] < positions["orb-c"]);
    }

    #[test]
    fn pipeline_with_depends_on() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepStore::new(dir.path().join("deps.jsonl"));

        // c depends_on b, b depends_on a
        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-c"),
                OrbId::from_raw("orb-b"),
                EdgeType::DependsOn,
            ))
            .unwrap();
        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-b"),
                OrbId::from_raw("orb-a"),
                EdgeType::DependsOn,
            ))
            .unwrap();

        let orbs = vec![
            make_orb("orb-c", 3),
            make_orb("orb-a", 3),
            make_orb("orb-b", 3),
        ];
        let pipeline = store.pipeline(&orbs).unwrap();
        let positions: HashMap<String, usize> = pipeline
            .iter()
            .enumerate()
            .map(|(i, id)| (id.as_str().to_string(), i))
            .collect();

        assert!(positions["orb-a"] < positions["orb-b"]);
        assert!(positions["orb-b"] < positions["orb-c"]);
    }

    #[test]
    fn pipeline_sorts_by_priority_at_same_level() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepStore::new(dir.path().join("deps.jsonl"));

        // No deps — all at same level, sorted by priority.
        let orbs = vec![
            make_orb("orb-low", 4),
            make_orb("orb-high", 1),
            make_orb("orb-med", 3),
        ];
        let pipeline = store.pipeline(&orbs).unwrap();
        // High priority (1) should come first.
        assert_eq!(pipeline[0], OrbId::from_raw("orb-high"));
    }

    // ── Priority propagation ─────────────────────────────────

    #[test]
    fn priority_propagation_through_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepStore::new(dir.path().join("deps.jsonl"));

        // a blocks b. b has priority 1 (critical), a has priority 4 (low).
        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-a"),
                OrbId::from_raw("orb-b"),
                EdgeType::Blocks,
            ))
            .unwrap();

        let orbs = vec![make_orb("orb-a", 4), make_orb("orb-b", 1)];
        let priorities = store.effective_priorities(&orbs).unwrap();

        // a's effective priority should be elevated to 1 because b (priority 1) depends on it.
        assert_eq!(priorities["orb-a"], 1);
        assert_eq!(priorities["orb-b"], 1);
    }

    #[test]
    fn priority_propagation_through_depends_on() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepStore::new(dir.path().join("deps.jsonl"));

        // b depends_on a. b has priority 1, a has priority 4.
        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-b"),
                OrbId::from_raw("orb-a"),
                EdgeType::DependsOn,
            ))
            .unwrap();

        let orbs = vec![make_orb("orb-a", 4), make_orb("orb-b", 1)];
        let priorities = store.effective_priorities(&orbs).unwrap();

        assert_eq!(priorities["orb-a"], 1);
        assert_eq!(priorities["orb-b"], 1);
    }

    #[test]
    fn priority_propagation_transitive() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepStore::new(dir.path().join("deps.jsonl"));

        // a blocks b, b blocks c. c has priority 1.
        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-a"),
                OrbId::from_raw("orb-b"),
                EdgeType::Blocks,
            ))
            .unwrap();
        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-b"),
                OrbId::from_raw("orb-c"),
                EdgeType::Blocks,
            ))
            .unwrap();

        let orbs = vec![
            make_orb("orb-a", 5),
            make_orb("orb-b", 4),
            make_orb("orb-c", 1),
        ];
        let priorities = store.effective_priorities(&orbs).unwrap();

        assert_eq!(priorities["orb-a"], 1);
        assert_eq!(priorities["orb-b"], 1);
        assert_eq!(priorities["orb-c"], 1);
    }

    #[test]
    fn priority_no_propagation_for_non_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepStore::new(dir.path().join("deps.jsonl"));

        // a related to b — should not propagate priority.
        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-a"),
                OrbId::from_raw("orb-b"),
                EdgeType::Related,
            ))
            .unwrap();

        let orbs = vec![make_orb("orb-a", 4), make_orb("orb-b", 1)];
        let priorities = store.effective_priorities(&orbs).unwrap();

        assert_eq!(priorities["orb-a"], 4);
        assert_eq!(priorities["orb-b"], 1);
    }

    // ── Serde round-trip for DepError ────────────────────────

    #[test]
    fn dep_error_display() {
        let err = DepError::CycleDetected {
            cycle: vec![
                OrbId::from_raw("orb-a"),
                OrbId::from_raw("orb-b"),
                OrbId::from_raw("orb-a"),
            ],
        };
        let msg = err.to_string();
        assert!(msg.contains("cycle detected"));
        assert!(msg.contains("orb-a"));
        assert!(msg.contains("orb-b"));
    }

    #[test]
    fn deduplication_on_re_add_after_remove() {
        let dir = tempfile::tempdir().unwrap();
        let store = DepStore::new(dir.path().join("deps.jsonl"));

        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-a"),
                OrbId::from_raw("orb-b"),
                EdgeType::Blocks,
            ))
            .unwrap();

        store
            .remove_edge(
                &OrbId::from_raw("orb-a"),
                &OrbId::from_raw("orb-b"),
                EdgeType::Blocks,
            )
            .unwrap();

        assert!(store.all_edges().unwrap().is_empty());

        // Re-add the same edge.
        store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-a"),
                OrbId::from_raw("orb-b"),
                EdgeType::Blocks,
            ))
            .unwrap();

        assert_eq!(store.all_edges().unwrap().len(), 1);
    }
}

use crate::dep::DepEdge;
use crate::dep_store::DepStore;
use crate::id::OrbId;
use crate::orb::Orb;
use crate::orb_store::OrbStore;

/// A node in an orb tree, containing an orb and its children.
#[derive(Debug, Clone)]
pub struct OrbNode {
    pub orb: Orb,
    pub children: Vec<OrbNode>,
    pub depth: usize,
}

/// A full timeline combining tree structure with dependency edges.
#[derive(Debug, Clone)]
pub struct FullTimeline {
    pub root: OrbNode,
    pub dep_edges: Vec<DepEdge>,
    pub total_orbs: usize,
    pub max_depth: usize,
}

/// Builds a tree of `OrbNode`s from `parent_id` relationships.
///
/// Returns `None` if `root_id` is not found in the store.
///
/// # Errors
///
/// Returns `None` if the store cannot be read or the root orb is missing.
pub fn build_orb_tree(store: &OrbStore, root_id: &OrbId) -> Option<OrbNode> {
    let all_orbs = store.load_all().ok()?;

    let root_orb = all_orbs.iter().find(|o| o.id == *root_id)?.clone();

    Some(build_node(root_orb, &all_orbs, 0))
}

/// Recursively builds a node and its children.
fn build_node(orb: Orb, all_orbs: &[Orb], depth: usize) -> OrbNode {
    let children: Vec<OrbNode> = all_orbs
        .iter()
        .filter(|o| o.parent_id.as_ref() == Some(&orb.id))
        .cloned()
        .map(|child| build_node(child, all_orbs, depth + 1))
        .collect();

    OrbNode {
        orb,
        children,
        depth,
    }
}

/// Builds a full timeline combining tree structure with dependency edges.
///
/// Returns `None` if the root orb is not found.
pub fn build_full_timeline(
    store: &OrbStore,
    dep_store: &DepStore,
    root_id: &OrbId,
) -> Option<FullTimeline> {
    let root = build_orb_tree(store, root_id)?;

    // Collect all orb IDs in the tree
    let all_orbs = flatten(&root);
    let orb_ids: std::collections::HashSet<&str> = all_orbs.iter().map(|o| o.id.as_str()).collect();

    // Filter dep edges to only those involving orbs in this tree
    let all_edges = dep_store.all_edges().ok()?;
    let dep_edges: Vec<DepEdge> = all_edges
        .into_iter()
        .filter(|e| orb_ids.contains(e.from.as_str()) || orb_ids.contains(e.to.as_str()))
        .collect();

    let total_orbs = all_orbs.len();
    let max_depth = depth(&root);

    Some(FullTimeline {
        root,
        dep_edges,
        total_orbs,
        max_depth,
    })
}

/// Returns all leaf nodes (orbs with no children).
pub fn leaves(node: &OrbNode) -> Vec<&Orb> {
    if node.children.is_empty() {
        return vec![&node.orb];
    }
    node.children.iter().flat_map(leaves).collect()
}

/// Returns the maximum depth of the tree (0-indexed from root).
pub fn depth(node: &OrbNode) -> usize {
    if node.children.is_empty() {
        return 0;
    }
    node.children.iter().map(depth).max().unwrap_or(0) + 1
}

/// Returns all orbs in pre-order traversal (root first, then children left to right).
pub fn flatten(node: &OrbNode) -> Vec<&Orb> {
    let mut result = vec![&node.orb];
    for child in &node.children {
        result.extend(flatten(child));
    }
    result
}

/// Returns the longest chain of blocking dependencies from root to a leaf.
///
/// Walks the tree following only edges that have blocking dep relationships
/// (`Blocks` or `DependsOn`) in the dep store. Returns the longest such path.
///
/// # Errors
///
/// Returns an empty vec if dep edges cannot be loaded.
pub fn critical_path<'a>(node: &'a OrbNode, dep_store: &DepStore) -> Vec<&'a Orb> {
    let all_edges = dep_store.all_edges().unwrap_or_default();

    // Build a set of blocking edges for quick lookup: (from, to)
    let blocking: std::collections::HashSet<(String, String)> = all_edges
        .iter()
        .filter(|e| e.edge_type.is_blocking())
        .map(|e| (e.from.as_str().to_string(), e.to.as_str().to_string()))
        .collect();

    find_critical_path(node, &blocking)
}

/// Recursively finds the longest blocking path.
fn find_critical_path<'a>(
    node: &'a OrbNode,
    blocking: &std::collections::HashSet<(String, String)>,
) -> Vec<&'a Orb> {
    if node.children.is_empty() {
        return vec![&node.orb];
    }

    // Find children connected by blocking edges (in either direction)
    let blocking_children: Vec<&OrbNode> = node
        .children
        .iter()
        .filter(|child| {
            let from_to = (
                node.orb.id.as_str().to_string(),
                child.orb.id.as_str().to_string(),
            );
            let to_from = (
                child.orb.id.as_str().to_string(),
                node.orb.id.as_str().to_string(),
            );
            blocking.contains(&from_to) || blocking.contains(&to_from)
        })
        .collect();

    // If no blocking children, just return root
    if blocking_children.is_empty() {
        return vec![&node.orb];
    }

    // Find the longest path among blocking children
    let mut longest: Vec<&Orb> = vec![];
    for child in blocking_children {
        let child_path = find_critical_path(child, blocking);
        if child_path.len() > longest.len() {
            longest = child_path;
        }
    }

    let mut path = vec![&node.orb];
    path.extend(longest);
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dep::{DepEdge, EdgeType};
    use crate::orb::Orb;

    // ── Helper to create a deterministic orb with a known ID ──
    fn make_orb(id: &str, title: &str) -> Orb {
        let mut orb = Orb::new(title, "test description");
        orb.id = OrbId::from_raw(id);
        orb
    }

    fn make_child_orb(id: &str, title: &str, parent_id: &str) -> Orb {
        let mut orb = make_orb(id, title);
        orb.parent_id = Some(OrbId::from_raw(parent_id));
        orb.root_id = Some(OrbId::from_raw(parent_id));
        orb
    }

    // ── Tree building tests ──

    #[test]
    fn build_tree_single_root() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrbStore::new(dir.path().join("orbs.jsonl"));

        let root = make_orb("orb-root", "Root");
        store.append(&root).unwrap();

        let tree = build_orb_tree(&store, &OrbId::from_raw("orb-root")).unwrap();
        assert_eq!(tree.orb.id, OrbId::from_raw("orb-root"));
        assert!(tree.children.is_empty());
        assert_eq!(tree.depth, 0);
    }

    #[test]
    fn build_tree_nested_children() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrbStore::new(dir.path().join("orbs.jsonl"));

        let root = make_orb("orb-root", "Root");
        let child1 = make_child_orb("orb-c1", "Child 1", "orb-root");
        let child2 = make_child_orb("orb-c2", "Child 2", "orb-root");
        let grandchild = make_child_orb("orb-gc1", "Grandchild", "orb-c1");

        store.append(&root).unwrap();
        store.append(&child1).unwrap();
        store.append(&child2).unwrap();
        store.append(&grandchild).unwrap();

        let tree = build_orb_tree(&store, &OrbId::from_raw("orb-root")).unwrap();
        assert_eq!(tree.children.len(), 2);
        assert_eq!(tree.depth, 0);

        // Find child1 and verify it has a grandchild
        let c1 = tree
            .children
            .iter()
            .find(|c| c.orb.id == OrbId::from_raw("orb-c1"))
            .unwrap();
        assert_eq!(c1.children.len(), 1);
        assert_eq!(c1.depth, 1);
        assert_eq!(c1.children[0].depth, 2);
        assert_eq!(c1.children[0].orb.title, "Grandchild");

        // Child2 has no children
        let c2 = tree
            .children
            .iter()
            .find(|c| c.orb.id == OrbId::from_raw("orb-c2"))
            .unwrap();
        assert!(c2.children.is_empty());
    }

    #[test]
    fn build_tree_missing_root_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrbStore::new(dir.path().join("orbs.jsonl"));

        let orb = make_orb("orb-other", "Other");
        store.append(&orb).unwrap();

        let result = build_orb_tree(&store, &OrbId::from_raw("orb-missing"));
        assert!(result.is_none());
    }

    #[test]
    fn build_tree_empty_store_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrbStore::new(dir.path().join("nonexistent.jsonl"));

        let result = build_orb_tree(&store, &OrbId::from_raw("orb-any"));
        assert!(result.is_none());
    }

    // ── Query helper tests ──

    #[test]
    fn leaves_single_node() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrbStore::new(dir.path().join("orbs.jsonl"));

        let root = make_orb("orb-root", "Root");
        store.append(&root).unwrap();

        let tree = build_orb_tree(&store, &OrbId::from_raw("orb-root")).unwrap();
        let leaf_orbs = leaves(&tree);
        assert_eq!(leaf_orbs.len(), 1);
        assert_eq!(leaf_orbs[0].id, OrbId::from_raw("orb-root"));
    }

    #[test]
    fn leaves_returns_only_leaf_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrbStore::new(dir.path().join("orbs.jsonl"));

        let root = make_orb("orb-root", "Root");
        let child1 = make_child_orb("orb-c1", "Child 1", "orb-root");
        let child2 = make_child_orb("orb-c2", "Child 2", "orb-root");
        let grandchild = make_child_orb("orb-gc1", "Grandchild", "orb-c1");

        store.append(&root).unwrap();
        store.append(&child1).unwrap();
        store.append(&child2).unwrap();
        store.append(&grandchild).unwrap();

        let tree = build_orb_tree(&store, &OrbId::from_raw("orb-root")).unwrap();
        let leaf_orbs = leaves(&tree);

        // Leaves should be child2 and grandchild (not root or child1)
        assert_eq!(leaf_orbs.len(), 2);
        let leaf_ids: Vec<&str> = leaf_orbs.iter().map(|o| o.id.as_str()).collect();
        assert!(leaf_ids.contains(&"orb-c2"));
        assert!(leaf_ids.contains(&"orb-gc1"));
    }

    #[test]
    fn depth_single_node_is_zero() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrbStore::new(dir.path().join("orbs.jsonl"));

        let root = make_orb("orb-root", "Root");
        store.append(&root).unwrap();

        let tree = build_orb_tree(&store, &OrbId::from_raw("orb-root")).unwrap();
        assert_eq!(depth(&tree), 0);
    }

    #[test]
    fn depth_nested_tree() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrbStore::new(dir.path().join("orbs.jsonl"));

        let root = make_orb("orb-root", "Root");
        let child = make_child_orb("orb-c1", "Child", "orb-root");
        let grandchild = make_child_orb("orb-gc1", "Grandchild", "orb-c1");

        store.append(&root).unwrap();
        store.append(&child).unwrap();
        store.append(&grandchild).unwrap();

        let tree = build_orb_tree(&store, &OrbId::from_raw("orb-root")).unwrap();
        assert_eq!(depth(&tree), 2);
    }

    #[test]
    fn flatten_preorder() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrbStore::new(dir.path().join("orbs.jsonl"));

        let root = make_orb("orb-root", "Root");
        let child1 = make_child_orb("orb-c1", "Child 1", "orb-root");
        let child2 = make_child_orb("orb-c2", "Child 2", "orb-root");
        let grandchild = make_child_orb("orb-gc1", "Grandchild", "orb-c1");

        store.append(&root).unwrap();
        store.append(&child1).unwrap();
        store.append(&child2).unwrap();
        store.append(&grandchild).unwrap();

        let tree = build_orb_tree(&store, &OrbId::from_raw("orb-root")).unwrap();
        let all = flatten(&tree);

        assert_eq!(all.len(), 4);
        // Root is first in pre-order
        assert_eq!(all[0].id, OrbId::from_raw("orb-root"));
    }

    #[test]
    fn flatten_single_node() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrbStore::new(dir.path().join("orbs.jsonl"));

        let root = make_orb("orb-root", "Root");
        store.append(&root).unwrap();

        let tree = build_orb_tree(&store, &OrbId::from_raw("orb-root")).unwrap();
        let all = flatten(&tree);
        assert_eq!(all.len(), 1);
    }

    // ── Critical path tests ──

    #[test]
    fn critical_path_no_blocking_deps() {
        let dir = tempfile::tempdir().unwrap();
        let orb_store = OrbStore::new(dir.path().join("orbs.jsonl"));
        let dep_store = DepStore::new(dir.path().join("deps.jsonl"));

        let root = make_orb("orb-root", "Root");
        let child = make_child_orb("orb-c1", "Child", "orb-root");

        orb_store.append(&root).unwrap();
        orb_store.append(&child).unwrap();

        let tree = build_orb_tree(&orb_store, &OrbId::from_raw("orb-root")).unwrap();
        let path = critical_path(&tree, &dep_store);

        // No blocking deps means just the root
        assert_eq!(path.len(), 1);
        assert_eq!(path[0].id, OrbId::from_raw("orb-root"));
    }

    #[test]
    fn critical_path_with_blocking_deps() {
        let dir = tempfile::tempdir().unwrap();
        let orb_store = OrbStore::new(dir.path().join("orbs.jsonl"));
        let dep_store = DepStore::new(dir.path().join("deps.jsonl"));

        let root = make_orb("orb-root", "Root");
        let child1 = make_child_orb("orb-c1", "Child 1", "orb-root");
        let child2 = make_child_orb("orb-c2", "Child 2", "orb-root");
        let grandchild = make_child_orb("orb-gc1", "Grandchild", "orb-c1");

        orb_store.append(&root).unwrap();
        orb_store.append(&child1).unwrap();
        orb_store.append(&child2).unwrap();
        orb_store.append(&grandchild).unwrap();

        // root blocks child1, child1 blocks grandchild (longest chain = 3)
        // root blocks child2 (shorter chain = 2)
        dep_store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-root"),
                OrbId::from_raw("orb-c1"),
                EdgeType::Blocks,
            ))
            .unwrap();
        dep_store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-c1"),
                OrbId::from_raw("orb-gc1"),
                EdgeType::Blocks,
            ))
            .unwrap();
        dep_store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-root"),
                OrbId::from_raw("orb-c2"),
                EdgeType::Blocks,
            ))
            .unwrap();

        let tree = build_orb_tree(&orb_store, &OrbId::from_raw("orb-root")).unwrap();
        let path = critical_path(&tree, &dep_store);

        // Should be root -> child1 -> grandchild (length 3)
        assert_eq!(path.len(), 3);
        assert_eq!(path[0].id, OrbId::from_raw("orb-root"));
        assert_eq!(path[1].id, OrbId::from_raw("orb-c1"));
        assert_eq!(path[2].id, OrbId::from_raw("orb-gc1"));
    }

    #[test]
    fn critical_path_depends_on_edge() {
        let dir = tempfile::tempdir().unwrap();
        let orb_store = OrbStore::new(dir.path().join("orbs.jsonl"));
        let dep_store = DepStore::new(dir.path().join("deps.jsonl"));

        let root = make_orb("orb-root", "Root");
        let child = make_child_orb("orb-c1", "Child", "orb-root");

        orb_store.append(&root).unwrap();
        orb_store.append(&child).unwrap();

        // DependsOn is also blocking
        dep_store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-c1"),
                OrbId::from_raw("orb-root"),
                EdgeType::DependsOn,
            ))
            .unwrap();

        let tree = build_orb_tree(&orb_store, &OrbId::from_raw("orb-root")).unwrap();
        let path = critical_path(&tree, &dep_store);

        assert_eq!(path.len(), 2);
        assert_eq!(path[0].id, OrbId::from_raw("orb-root"));
        assert_eq!(path[1].id, OrbId::from_raw("orb-c1"));
    }

    // ── Full timeline tests ──

    #[test]
    fn full_timeline_basic() {
        let dir = tempfile::tempdir().unwrap();
        let orb_store = OrbStore::new(dir.path().join("orbs.jsonl"));
        let dep_store = DepStore::new(dir.path().join("deps.jsonl"));

        let root = make_orb("orb-root", "Root");
        let child = make_child_orb("orb-c1", "Child", "orb-root");

        orb_store.append(&root).unwrap();
        orb_store.append(&child).unwrap();

        dep_store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-root"),
                OrbId::from_raw("orb-c1"),
                EdgeType::Blocks,
            ))
            .unwrap();

        let timeline =
            build_full_timeline(&orb_store, &dep_store, &OrbId::from_raw("orb-root")).unwrap();

        assert_eq!(timeline.total_orbs, 2);
        assert_eq!(timeline.max_depth, 1);
        assert_eq!(timeline.dep_edges.len(), 1);
        assert_eq!(timeline.root.orb.id, OrbId::from_raw("orb-root"));
    }

    #[test]
    fn full_timeline_missing_root_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let orb_store = OrbStore::new(dir.path().join("orbs.jsonl"));
        let dep_store = DepStore::new(dir.path().join("deps.jsonl"));

        let result = build_full_timeline(&orb_store, &dep_store, &OrbId::from_raw("orb-missing"));
        assert!(result.is_none());
    }

    #[test]
    fn full_timeline_excludes_unrelated_edges() {
        let dir = tempfile::tempdir().unwrap();
        let orb_store = OrbStore::new(dir.path().join("orbs.jsonl"));
        let dep_store = DepStore::new(dir.path().join("deps.jsonl"));

        let root = make_orb("orb-root", "Root");
        let child = make_child_orb("orb-c1", "Child", "orb-root");
        let unrelated = make_orb("orb-other", "Other");

        orb_store.append(&root).unwrap();
        orb_store.append(&child).unwrap();
        orb_store.append(&unrelated).unwrap();

        // Edge within the tree
        dep_store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-root"),
                OrbId::from_raw("orb-c1"),
                EdgeType::Blocks,
            ))
            .unwrap();

        // Edge outside the tree (between unrelated orbs)
        dep_store
            .add_edge(DepEdge::new(
                OrbId::from_raw("orb-other"),
                OrbId::from_raw("orb-unrelated2"),
                EdgeType::Related,
            ))
            .unwrap();

        let timeline =
            build_full_timeline(&orb_store, &dep_store, &OrbId::from_raw("orb-root")).unwrap();

        // Should only include the edge within the tree
        assert_eq!(timeline.dep_edges.len(), 1);
        assert_eq!(timeline.dep_edges[0].from, OrbId::from_raw("orb-root"));
    }
}

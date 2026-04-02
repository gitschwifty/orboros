# orbs — Core Library

The `orbs` crate provides the data model, persistence, and query layer for orboros. It has no async dependencies and no knowledge of workers or IPC.

## Orb Basics

```rust
use orbs::orb::{Orb, OrbType, OrbStatus, OrbPhase};

// Create a task (default type)
let orb = Orb::new("Fix login bug", "Users can't log in with SSO");

// Create an epic (uses phase lifecycle)
let epic = Orb::new("User management", "Full CRUD for users")
    .with_type(OrbType::Epic)
    .with_priority(2);

// Create a child orb
let child = Orb::new("Add user creation endpoint", "POST /users")
    .with_parent(epic.id.clone(), None);
// child.parent_id == Some(epic.id)
// child.root_id == Some(epic.id)  (inherited)
```

### Lifecycle

Tasks use `status`, epics/features use `phase`:

```rust
// Task lifecycle
let mut task = Orb::new("Fix bug", "details");
assert_eq!(task.status, Some(OrbStatus::Pending));
task.set_status(OrbStatus::Active);
task.set_status(OrbStatus::Done);
assert!(task.closed_at.is_some());

// Epic lifecycle
let mut epic = Orb::new("Big feature", "details").with_type(OrbType::Epic);
assert_eq!(epic.phase, Some(OrbPhase::Pending));
epic.set_phase(OrbPhase::Speccing);
epic.set_phase(OrbPhase::Decomposing);
// ...continues through Refining, Review, Waiting, Executing, Done

// Deferral (only from inactive states)
let mut orb = Orb::new("Low priority", "later");
assert!(orb.defer()); // Pending -> Deferred
orb.undefer();         // Deferred -> Pending

// Soft delete
let mut orb = Orb::new("Duplicate", "oops");
orb.tombstone(Some("duplicate of orb-abc".into()));
assert!(orb.is_tombstoned());
```

### Content Hashing

Used for change detection (e.g. refinement termination):

```rust
let mut orb = Orb::new("Task", "description");
orb.update_content_hash();
let hash1 = orb.content_hash.clone();

orb.description = "changed description".into();
orb.update_content_hash();
assert_ne!(orb.content_hash, hash1); // content changed

orb.updated_at = chrono::Utc::now();  // metadata-only change
orb.update_content_hash();
// hash unchanged — content_hash ignores timestamps
```

### IDs

Content-addressed, hierarchical:

```rust
use orbs::id::OrbId;
use std::collections::HashSet;

// Generate from seed fields
let id = OrbId::generate("title", "desc", "user", 1234567890, &HashSet::new());
// e.g. "orb-k4f"

// Child IDs
let child_id = id.child(1);  // "orb-k4f.1"
let nested = child_id.child(3);  // "orb-k4f.1.3"

assert!(child_id.is_child());
assert_eq!(child_id.parent_id(), Some(id.clone()));
```

## Persistence

All stores use append-only JSONL. Latest entry per ID wins on read.

### OrbStore

```rust
use orbs::orb::Orb;
use orbs::orb_store::OrbStore;

let store = OrbStore::new("/path/to/orbs.jsonl");

// Write
let orb = Orb::new("Task", "description");
store.append(&orb)?;

// Update (appends new version)
let mut orb = orb;
orb.set_status(orbs::orb::OrbStatus::Done);
store.update(&orb)?;

// Read (deduplicates by ID, excludes tombstoned)
let all = store.load_all()?;
let by_id = store.load_by_id(&orb.id)?;
let pending = store.load_by_status(orbs::task::TaskStatus::Pending)?;
let epics = store.load_by_type(&orbs::orb::OrbType::Epic)?;
let children = store.load_children(&parent_id)?;

// Include tombstoned
let everything = store.load_all_including_tombstoned()?;

// For ID generation collision checking
let existing = store.existing_ids()?;
```

### DepStore

```rust
use orbs::dep::{DepEdge, EdgeType};
use orbs::dep_store::DepStore;
use orbs::id::OrbId;

let store = DepStore::new("/path/to/deps.jsonl");
let a = OrbId::from_raw("orb-a");
let b = OrbId::from_raw("orb-b");

// Add edge (rejects cycles on blocking edges)
store.add_edge(&a, &b, EdgeType::Blocks)?;

// Query
let from_a = store.edges_from(&a)?;    // edges where a is source
let to_b = store.edges_to(&b)?;        // edges where b is target
let all = store.all_edges()?;

// Remove (soft-delete)
store.remove_edge(&a, &b, EdgeType::Blocks)?;

// Scheduling queries (need an OrbStore for status lookups)
let orb_store = OrbStore::new("/path/to/orbs.jsonl");
let pipeline = store.pipeline(&orb_store)?;  // topological sort
let ready = store.ready(&orb_store)?;        // unblocked orbs
let waiting = store.waiting(&orb_store)?;    // blocked orbs

// Priority propagation
let priorities = store.effective_priorities(&orb_store)?;
// HashMap<OrbId, u8> — highest priority of transitive dependents
```

### AuditStore

```rust
use orbs::audit::{AuditEvent, EventType, Comment};
use orbs::audit_store::AuditStore;

let store = AuditStore::new("/path/to/events.jsonl");

// Log an event
store.log_event(AuditEvent::new(
    orb_id.clone(),
    EventType::StatusChanged,
    "system",
    Some("pending -> active".into()),
))?;

// Add a comment
store.add_comment(Comment::new(
    orb_id.clone(),
    "reviewer",
    "Looks good, but needs tests",
))?;

// Query
let events = store.events_for_orb(&orb_id)?;
let comments = store.comments_for_orb(&orb_id)?;
let all = store.all_events()?;
```

## Tree Reconstruction

Build orb hierarchies from parent_id relationships:

```rust
use orbs::tree::{build_orb_tree, build_full_timeline, leaves, depth, flatten, critical_path};

// Build tree from a root orb
if let Some(tree) = build_orb_tree(&orb_store, &root_id) {
    println!("Depth: {}", depth(&tree));
    println!("Leaves: {}", leaves(&tree).len());

    // All orbs in pre-order
    for orb in flatten(&tree) {
        println!("  {} — {}", orb.id, orb.title);
    }

    // Longest blocking chain
    let crit = critical_path(&tree, &dep_store);
    println!("Critical path: {} orbs", crit.len());
}

// Full timeline with dep edges
let timeline = build_full_timeline(&orb_store, &dep_store, &root_id);
println!("Total orbs: {}, Max depth: {}", timeline.total_orbs, timeline.max_depth);
```

## Pipeline Directories

Isolated work directories per pipeline:

```rust
use orbs::pipeline::{create_pipeline, snapshot, compact, resolve_store, recover_pipeline};

let base_dir = Path::new("/path/to/project/.orbs");

// Create pipeline directory structure
let pipeline = create_pipeline(base_dir, &epic_orb)?;
// Creates: pipelines/epic-k4f/orbs.jsonl, deps.jsonl, events.jsonl, snapshots/, history/

// Pipeline has its own stores
let pipe_store = pipeline.orb_store();
let pipe_deps = pipeline.dep_store();

// Snapshot current state
snapshot(&pipeline, "decomposition")?;
// Copies to: snapshots/decomposition/

// Multiple snapshots auto-number
snapshot(&pipeline, "refinement")?;   // snapshots/refinement/
snapshot(&pipeline, "refinement")?;   // snapshots/refinement-1/

// Compact (deduplicate, archive old entries)
compact(&pipeline)?;

// Store routing — finds the right store for an orb
let store = resolve_store(base_dir, &orb_id)?;
// Checks pipeline dirs first, falls back to canonical store

// Recovery after interruption
recover_pipeline(&pipeline)?;
// Restores from latest snapshot if .lock file is stale
```

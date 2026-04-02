# Orboros

Multi-agent orchestrator and ticketing system for software development. Decomposes complex work into orbs (tracked work items), manages dependencies and lifecycles, and executes through worker processes with cancellation, budgets, retries, and tracing.

## Workspace

Two crates:

- **`orbs`** — Core library: Orb schema, stores, dependency graph, audit log, tree reconstruction, pipeline management
- **`orboros`** — CLI binary: orchestration, phases, queue loop, daemon, configuration

## Quick Start

```bash
# Initialize a project
cargo run -- init

# Create an orb (work item)
cargo run -- orb create "Add authentication" --type task --priority 2

# Plan an epic (decompose into subtasks)
cargo run -- plan "Build user management system"

# Run a single task via worker
cargo run -- run "What is the capital of France?"

# Full orchestration: decompose + route + execute
cargo run -- orchestrate "Refactor the authentication module"

# Start the daemon (background processing)
cargo run -- daemon
```

## CLI Commands

### Task Execution

| Command | Description |
|---------|-------------|
| `run <task>` | Execute a single task directly via worker |
| `decompose <task>` | Break into subtasks, print plan |
| `orchestrate <task>` | Decompose + execute all subtasks |
| `plan <description>` | Create an epic with shallow decomposition |
| `plan --file <path>` | Plan from a markdown file |

### Orb Management

| Command | Description |
|---------|-------------|
| `orb create <title>` | Create a new orb (`--type`, `--priority`) |
| `orb show <id>` | Show full orb details |
| `orb list` | List orbs (`--type`, `--status` filters) |
| `orb update <id>` | Update fields (`--title`, `--priority`, `--status`) |
| `orb delete <id>` | Soft-delete (tombstone) an orb |
| `orb dep add <from> <to>` | Add dependency edge (`--type blocks`) |
| `orb dep rm <from> <to>` | Remove dependency edge |
| `orb deps <id>` | List dependencies for an orb |
| `orb review <id> <decision>` | Apply review decision (approve/reject/revise) |

### Project & System

| Command | Description |
|---------|-------------|
| `init` | Initialize `.orbs/` in current directory |
| `daemon` | Start background queue loop |
| `daemon --stop` | Stop running daemon |
| `daemon --status` | Check daemon status |
| `tasks [-s status]` | List legacy tasks |
| `status <id>` | Show legacy task details |
| `review` | List tasks awaiting review |

### Global Options

| Flag | Env Var | Default | Description |
|------|---------|---------|-------------|
| `--state-dir` | — | `~/.orboros/default` | Project state directory |
| `--worker-binary` | `HEDDLE_BINARY` | — | Path to heddle-headless binary |
| `--model` | — | `openrouter/free` | Default model |

## Orb Schema

An **orb** is a tracked work item with content-addressed IDs, dual lifecycle, and rich metadata.

### Identity

- **Content-hash IDs**: SHA-256 of seed fields, base36 encoded (e.g. `orb-k4f`)
- **Hierarchical children**: `orb-k4f.1`, `orb-k4f.2` (monotonic counter)
- **Content hash**: Separate hash of mutable fields for change detection

### Types

`epic` | `feature` | `task` | `bug` | `chore` | `docs` | `Custom(String)`

### Lifecycle

**Tasks, bugs, chores, docs** use `status`:
```
draft → pending → active → [review] → done | failed
any → cancelled | tombstone
pending → deferred (reversible)
```

**Epics, features** use `phase`:
```
draft → pending → speccing → decomposing → refining → [review] → waiting → executing → [review] → done | failed
any → cancelled | tombstone
pending | waiting → deferred (reversible)
waiting → reevaluating → executing (when deps change)
```

### Priority

| Level | Name |
|-------|------|
| 1 | Critical |
| 2 | High |
| 3 | Medium (default) |
| 4 | Low |
| 5 | Backlog |

## Dependency Graph

Seven edge types: `Blocks`, `DependsOn`, `Parent`, `Child`, `Related`, `Duplicates`, `Follows`.

- Blocking edges (`Blocks`, `DependsOn`) enforce execution ordering
- Cycle detection (BFS) on blocking edges prevents deadlocks
- `pipeline()` returns topological sort with priority tie-breaking
- `ready()` / `waiting()` queries drive the queue loop
- Effective priority propagates upstream through blocking deps

## Pipeline Phases

The phase pipeline for epics/features:

1. **Speccing** — Detect or generate design + acceptance criteria
2. **Decomposition** — Break into child orbs with hierarchical IDs and dep edges
3. **Refinement** — Iterative passes until content hash stabilizes (or max rounds)
4. **Review** — Human-in-the-loop checkpoint (configurable per project)
5. **Re-evaluation** — Check upstream deps before execution; escalate on failures

## Configuration

Layered config with TOML:

```
~/.orboros/config.toml          # Global defaults
.orbs/config.toml               # Project overrides
CLI flags                       # Per-invocation overrides
```

```toml
# Example .orbs/config.toml
default_model = "anthropic/claude-sonnet-4-20250514"
max_concurrency = 4

[review]
requires_approval_by_default = false
review_on_completion = true

[notifications]
enabled = true
desktop = true
```

Projects are registered in `~/.orboros/projects.toml` automatically on `orboros init`.

## Model Routing

```toml
# routing.toml in state directory
default_model = "openrouter/auto"

[[rules]]
worker_type = "research"
model = "google/gemini-2.0-flash-001"

[[rules]]
worker_type = "edit"
model = "anthropic/claude-sonnet-4-20250514"

[profiles.edit]
allowed_tools = ["read", "write", "glob", "grep"]
```

## Architecture

```
crates/
  orbs/                   # Core library
    src/
      id.rs               # Content-hash ID generation (SHA-256 base36)
      orb.rs              # Orb struct, types, lifecycle enums
      orb_store.rs        # JSONL persistence with tombstone filtering
      store.rs            # Legacy TaskStore
      task.rs             # Legacy Task struct
      trace.rs            # Trace types, TerminationReason
      dep.rs              # Dependency edge schema
      dep_store.rs        # Dep persistence, cycle detection, topological sort
      audit.rs            # Audit events + comments
      audit_store.rs      # JSONL audit persistence
      tree.rs             # Tree reconstruction + query helpers
      pipeline.rs         # Pipeline directory lifecycle + snapshots

  orboros/                # CLI binary
    src/
      main.rs             # CLI entry point (clap)
      lib.rs              # Module exports
      config.rs           # Layered config loading + project registry
      runner.rs           # Single-task execution
      orchestrator.rs     # Multi-subtask orchestration
      trace.rs            # Timeline builder (bridges orbs types)
      queue_loop.rs       # Tick-based daemon loop
      daemon.rs           # PID management, signal handling, log rotation
      plan.rs             # Plan pipeline + file parsing
      notify.rs           # Terminal + desktop notifications
      slop.rs             # Post-completion quality checks
      orb_cmd.rs          # Orb CRUD CLI implementations
      coordinator/        # LLM-powered decomposition + aggregation
      ipc/                # JSON-line protocol with heddle workers
      routing/            # Model selection + tool profiles
      worker/             # Process lifecycle, pool, budget, FSM
      phases/             # Pipeline phase implementations
        speccing.rs
        decompose.rs
        refinement.rs
        review.rs
        re_evaluation.rs
```

## Development

```bash
cargo test                # 540 tests
cargo clippy --workspace  # lint (pedantic)
cargo fmt --all           # format
```

Tests use mock worker scripts (`test-fixtures/mock-worker*.sh`) for fast unit tests. Set `HEDDLE_BINARY` for integration tests against a real heddle instance.

## Project Layout

```
~/repos/orboros/
  worktree.sh       # Creates worktrees with shared file symlinks
  main/             # Main branch worktree (this code)
```

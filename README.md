<p align="center">
  <img src="assets/orboros-icon.png" alt="Orboros" width="128">
</p>

# Orboros

Multi-agent orchestrator and ticketing system for software development. Decomposes complex work into orbs (tracked work items), manages dependencies and lifecycles, and executes through worker processes with cancellation, budgets, retries, and tracing.

## Workspace

Two crates:

- **`orbs`** — Core library: Orb schema, stores, dependency graph, audit log, tree reconstruction, pipeline management
- **`orboros`** — CLI binary: orchestration, phases, queue loop, daemon, configuration

## Quick Start

```bash
# Initialize a project
orboros init

# Create an orb (work item)
orboros orb create "Add authentication" --type task --priority 2

# Run a single task orb in the foreground
orboros run "What is the capital of France?"

# Plan an epic (decompose into subtasks)
orboros plan "Build user management system"

# Drive an existing orb through the foreground queue path
orboros execute orb-k4f --wait

# Legacy full orchestration: decompose + route + execute
orboros legacy orchestrate "Refactor the authentication module"

# Start the daemon (background processing)
orboros daemon
```

## CLI Commands

### Task Execution

| Command | Description |
|---------|-------------|
| `run <task>` | Create a task orb and run queue/dispatch in the foreground |
| `execute <orb-id> --wait` | Drive an existing orb with the foreground queue |
| `plan <description>` | Create an epic with shallow decomposition |
| `plan --file <path>` | Plan from a markdown file |
| `legacy run <task>` | Execute a legacy `tasks.jsonl` task directly via worker |
| `legacy decompose <task>` | Break a legacy task into subtasks, print plan |
| `legacy orchestrate <task>` | Legacy TaskStore decompose + execute flow |

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
| `legacy tasks [-s status]` | List legacy tasks |
| `legacy status <id>` | Show legacy task details |
| `legacy review` | List legacy tasks awaiting review |

### Global Options

| Flag | Env Var | Default | Description |
|------|---------|---------|-------------|
| `--state-dir` | — | nearest ancestor `.orbs`, then `~/.orboros/default` | Project state directory |
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

[models.default]
worker = "balanced"
coordinator = "planner"
reviewer = "fast"
bench = "balanced"

[models.options.balanced]
model = "anthropic/claude-sonnet-5"
description = "Default strong coding and planning model."
provider = "anthropic"
router = "openrouter"
reasoning = "medium"
effort = "medium"

[models.options.fast]
model = "anthropic/claude-haiku-4.5"
description = "Cheap fast model for review, grading, and simple tasks."
provider = "anthropic"
router = "openrouter"
reasoning = "low"
effort = "low"

[models.options.planner]
model = "openai/gpt-5.6-terra"
description = "Higher-effort OpenAI planning and decomposition model."
provider = "openai"
router = "openrouter"
reasoning = "high"
effort = "high"

[models.options.kimi]
model = "moonshotai/kimi-k3"
description = "Moonshot Kimi K3 candidate for coding and agentic bench runs."
provider = "moonshotai"
router = "openrouter"
reasoning = "max"
effort = "max"

[models.options.qwen]
model = "qwen/qwen3.6-plus"
description = "Qwen 3.6 candidate for cost/performance bench runs."
provider = "qwen"
router = "openrouter"
reasoning = "high"
effort = "high"

[models.workers]
research = "fast"
edit = "balanced"
review = "balanced"
test = "fast"

[models.coordinators]
decompose = "planner"
aggregate = "fast"

[models.phases]
speccing = "planner"
refining = "balanced"

[models.bench]
default = "balanced"
grader = "fast"

[tool_profiles.edit]
allowed_tools = ["read_file", "write_file", "edit_file", "glob", "grep", "bash"]

[tool_profiles.research]
allowed_tools = ["read_file", "glob", "grep", "web_fetch", "write_file"]

[review]
requires_approval_by_default = false
review_on_completion = true

[notification]
enabled = true
desktop_enabled = true

[prompts.workers.edit]
system = "You are an implementation worker. Make focused, tested code changes."

[prompts.workers.review]
system_file = "prompts/review.md" # resolves from .orbs/prompts/review.md

[prompts.coordinators.decompose]
system_file = "prompts/decompose.md"

[prompts.coordinators.aggregate]
system = "You synthesize completed subtask results into the final answer."

[prompts.phases.speccing]
system_file = "prompts/speccing.md"

[prompts.phases.refining]
system = "You are refining an Orboros plan. Return only the requested JSON shape."
```

Projects are registered in `~/.orboros/projects.toml` automatically on `orboros init`.

Model catalog entries are optional; configs with only `default_model` still work.
Role mappings may reference a named catalog option or a raw `provider/model`
string. Model strings are sent to Heddle exactly as configured; no
`openrouter/` prefix is added or removed. Selectors default to
`router = "openrouter"` for credential/routing metadata while OpenRouter is the
primary inference path, and catalog options can opt out by setting a different
`router`.

Prompt overrides fall back to role-specific built-in prompts when omitted.
Worker keys include subtask roles like `research`, `edit`, `review`, `test`,
and `plan`, plus the orb execution worker `execute`. Coordinator keys include
`decompose` and `aggregate`. Phase keys include `speccing`, `decomposing`,
`refining`, and `reevaluating`. Legacy `[prompts.workers.decompose]` and
`[prompts.workers.aggregate]` entries still work as a compatibility fallback.

For one-shot worker-spawning commands, CLI prompt overrides take precedence over
global/project config:

```bash
orboros decompose "Plan the refactor" --system-prompt-file prompts/decompose-v2.md
orboros orchestrate "Fix auth flow" --system-prompt "You are a strict implementation worker."
```

Queue dispatch also appends dynamic Orboros task context to worker user prompts:
the current orb, parent/root summaries, sibling/child awareness, and upstream
dependency results/status. Heddle remains responsible for project context such
as `AGENTS.md`.

## Model Routing And Tool Profiles

Model selection now resolves through the `[models]` catalog first. Worker
roles use `[models.workers]`; phase dispatch uses `[models.phases]`; reviewer
and benchmark roles use their dedicated mappings. Selectors may be catalog keys
or raw `provider/model` strings.

Tool profiles live in the main config under `[tool_profiles.<worker_type>]`.
They use concrete Heddle tool names and override the built-in capability set
for that worker role. Built-ins are `coordinator`/`review` (read-only), `test`
(read-only plus `bash`), `research` (read/web/write artifact), `edit`, and the
benchmark roles `bench_t1` (none) and `bench_t2` (edit capability). A `default`
profile applies when no exact worker-type profile exists. `tools: []` is an
explicit no-tool allowlist, not a request for Heddle defaults.

`routing.toml` is legacy and remains only as a fallback reader for old tool
profile files.

```toml
# preferred: .orbs/config.toml
[tool_profiles.edit]
allowed_tools = ["read_file", "write_file", "edit_file", "glob", "grep", "bash"]
```

## Architecture

```
src/
  main.rs                 # CLI entry point (clap)
  lib.rs                  # Module exports
  config.rs               # Layered config loading + project registry
  runner.rs               # Single-task execution
  orchestrator.rs         # Multi-subtask orchestration
  trace.rs                # Timeline builder
  queue_loop.rs           # Tick-based daemon loop
  daemon.rs               # PID management, signal handling, log rotation
  plan.rs                 # Plan pipeline + file parsing
  notify.rs               # Terminal + desktop notifications
  slop.rs                 # Post-completion quality checks
  orb_cmd.rs              # Orb CRUD CLI implementations
  bench/                  # Benchmark corpus harness
  coordinator/            # LLM-powered decomposition + aggregation
  ipc/                    # JSON-line protocol with heddle workers
  routing/                # Legacy tool profile compatibility
  worker/                 # Process lifecycle, pool, budget, FSM
  phases/                 # Pipeline phase implementations
    speccing.rs
    decompose.rs
    refinement.rs
    review.rs
    re_evaluation.rs
```

## Development

```bash
just ci                  # fmt-check + clippy + tests
just test                # cargo test
just clippy              # cargo clippy --all-targets -- -D warnings
just fmt                 # cargo fmt
just bench-init ../orboros-bench # create private sibling bench corpus dirs
just bench-list ../orboros-bench # list benchmark cases
just bench-run t1 ../orboros-bench
just bench-run-model kimi t1 kimi-smoke ../orboros-bench
orboros bench --bench-root ../orboros-bench --bench-config ../orboros-bench/config.toml run --tier t1
orboros bench --bench-root ../orboros-bench details <run-id>
```

Bench runs can set overall caps in config; individual cases can override these:

```toml
[bench]
timeout_s = 600
max_iterations = 16
```

`timeout_s` is a whole-case wall-clock cap. `max_iterations` is passed to
Heddle as the per-worker agent/tool-loop budget for each dispatched orb; it is
not the number of Orboros queue ticks or generated child tasks.

Benchmark results default to `<bench-root>/results`; use
`--bench-results-dir <dir>` or `ORBOROS_BENCH_RESULTS_DIR` to inspect or write a
different store, including older runs under `~/.orboros/default/bench`. Each new
run writes under `<bench-root>/results/YYYY-MM-DD/<run-id>/`.

Bench runs load normal Orboros config first, then overlay
`<bench-root>/config.toml` when it exists. Use `--bench-config <path>` to point
at a different file; explicit paths must exist. Benchmark config is applied
before CLI flags such as `--worker-binary`, `bench run --model`, and
`bench run --variant`. Run metadata records the Orboros git commit and the
benchmark corpus git commit when each root is a git worktree.

Tests use mock worker scripts (`test-fixtures/mock-worker*.sh`) for fast unit tests. Set `HEDDLE_BINARY` for integration tests against a real heddle instance.

## Project Layout

```
~/repos/orboros/
  worktree.sh       # Creates worktrees with shared file symlinks
  main/             # Main branch worktree (this code)
```

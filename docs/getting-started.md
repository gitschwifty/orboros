# Getting Started with Orboros

## Prerequisites

- **Rust** stable toolchain
- **Heddle** — the `heddle-headless` binary (for worker execution)
- **API key** — configured in heddle (OpenRouter, Anthropic, etc.)

## Setup

### 1. Build

```bash
cd main/
cargo build
```

### 2. Initialize a project

```bash
# In your project directory
cargo run -- init
```

This creates:
- `.orbs/config.toml` — project config with defaults
- `.orbs/orbs.jsonl` — empty orb store
- Registers the project in `~/.orboros/projects.toml`

### 3. Worker binary

Orboros spawns heddle-headless as worker processes. Configure the path:

```bash
# Via environment variable (recommended)
export HEDDLE_BINARY=/path/to/heddle-headless

# Or per-command
cargo run -- --worker-binary /path/to/heddle-headless run "Hello"
```

### 4. Configuration (optional)

Edit `.orbs/config.toml` in your project:

```toml
default_model = "anthropic/claude-sonnet-4-20250514"
max_concurrency = 4

[review]
requires_approval_by_default = false

[notifications]
enabled = true
desktop = true
```

Global defaults go in `~/.orboros/config.toml`. Project config overrides global.

## Basic Usage

### Create and manage orbs

Orbs are tracked work items — tasks, bugs, epics, features, etc.

```bash
# Create a task
cargo run -- orb create "Fix login bug" --type task --priority 2

# Create an epic
cargo run -- orb create "User management system" --type epic

# List all orbs
cargo run -- orb list

# Filter by type or status
cargo run -- orb list --type task --status pending

# Show details
cargo run -- orb show orb-k4f

# Update
cargo run -- orb update orb-k4f --priority 1 --status active

# Soft delete
cargo run -- orb delete orb-k4f --reason "duplicate"
```

### Manage dependencies

```bash
# orb-b blocks orb-a (a can't start until b is done)
cargo run -- orb dep add orb-b orb-a --type blocks

# orb-a depends on orb-c
cargo run -- orb dep add orb-a orb-c --type depends_on

# View deps
cargo run -- orb deps orb-a

# Remove
cargo run -- orb dep rm orb-b orb-a --type blocks
```

Edge types: `blocks`, `depends_on`, `parent`, `child`, `related`, `duplicates`, `follows`.

### Plan an epic

Break a high-level goal into subtasks:

```bash
# From inline description
cargo run -- plan "Build a REST API for user management"

# From a markdown file (first line = title, rest = description)
cargo run -- plan --file spec.md
```

This creates an epic orb, decomposes it into child tasks with dependency edges, and prints the plan tree.

### Execute tasks

```bash
# Run a single task directly
cargo run -- run "What is the capital of France?"

# Full orchestration: decompose + route + execute
cargo run -- orchestrate "Refactor the authentication module"
```

### Review orbs

```bash
# Apply a review decision
cargo run -- orb review orb-k4f approve
cargo run -- orb review orb-k4f reject
cargo run -- orb review orb-k4f revise
```

### Daemon mode

The daemon runs a background loop that processes the pipeline:

```bash
# Start
cargo run -- daemon

# Check status
cargo run -- daemon --status

# Stop
cargo run -- daemon --stop
```

The daemon loop:
1. Detects pending epics/features → starts pipeline (speccing)
2. Finds ready (unblocked) orbs → marks for execution
3. Checks root completion → marks parent done when all children done
4. Finds waiting orbs → triggers re-evaluation

## Model Routing

Place `routing.toml` in your state directory to route subtask types to different models:

```toml
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

See `examples/routing.toml` for a complete example.

## What Happens During Orchestration

1. **Coordinator** decomposes the task into typed subtasks with ordering
2. **Router** maps each subtask type to a model and tool profile
3. **Workers** execute in parallel (within order groups), sequential across groups
4. **Budget/timeout** enforcement cancels remaining work if limits hit
5. **Aggregator** synthesizes results into a unified response
6. State persisted to JSONL — check back anytime with `orb list` / `orb show`

## Project Structure

```
your-project/
  .orbs/
    config.toml     # Project config
    orbs.jsonl      # Orb store
    deps.jsonl      # Dependency edges
    events.jsonl    # Audit log
    pipelines/      # Per-pipeline working directories
      epic-k4f/
        orbs.jsonl
        deps.jsonl
        snapshots/
        history/
```

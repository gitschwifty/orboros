# CLI Reference

## Global Options

```
orboros [OPTIONS] <COMMAND>
```

| Option | Env Var | Default | Description |
|--------|---------|---------|-------------|
| `--state-dir <PATH>` | — | `~/.orboros/default` | Project state directory |
| `--worker-binary <PATH>` | `HEDDLE_BINARY` | — | Path to heddle-headless binary |
| `--model <MODEL>` | — | `openrouter/free` | Default model for workers |

## Commands

### `init`

Initialize a new project in the current directory.

```bash
orboros init
```

Creates `.orbs/` with `config.toml` and `orbs.jsonl`. Registers the project in `~/.orboros/projects.toml`.

---

### `run <TASK>`

Execute a single task directly via a worker.

```bash
orboros run "Explain how JWT works" --priority 2
orboros run "Fix the bug" --queue  # queue only, don't execute
```

| Option | Default | Description |
|--------|---------|-------------|
| `--priority, -p <N>` | 3 | Priority 1-5 |
| `--queue` | false | Queue without executing |

---

### `decompose <TASK>`

Break a task into subtasks using the coordinator LLM. Prints the plan without executing.

```bash
orboros decompose "Add error handling to the REST API"
```

---

### `orchestrate <TASK>`

Full orchestration: decompose into subtasks, route to models, execute, aggregate results.

```bash
orboros orchestrate "Refactor the authentication module" --priority 2
```

| Option | Default | Description |
|--------|---------|-------------|
| `--priority, -p <N>` | 3 | Priority for subtasks |

---

### `plan`

Create an epic with shallow decomposition into subtasks.

```bash
# Inline description
orboros plan "Build user management system"

# From markdown file (first line = title, rest = description)
orboros plan --file spec.md

# Shallow only (no refinement)
orboros plan "API redesign" --shallow
```

| Option | Description |
|--------|-------------|
| `--file <PATH>` | Read task from markdown file |
| `--shallow` | Shallow decomposition only |

---

### `orb`

Orb management subcommands.

#### `orb create <TITLE>`

```bash
orboros orb create "Fix login bug"
orboros orb create "User management" --type epic --priority 1
orboros orb create "Update docs" --type docs -d "Refresh API docs"
```

| Option | Default | Description |
|--------|---------|-------------|
| `--type, -t <TYPE>` | task | Orb type: task, epic, feature, bug, chore, docs |
| `--priority, -p <N>` | 3 | Priority 1-5 |
| `--description, -d <TEXT>` | (title) | Description |

#### `orb show <ID>`

Print full details of an orb.

```bash
orboros orb show orb-k4f
```

#### `orb list`

List orbs with optional filters.

```bash
orboros orb list
orboros orb list --type epic
orboros orb list --status pending
orboros orb list --type task --status active
```

| Option | Description |
|--------|-------------|
| `--type, -t <TYPE>` | Filter by type |
| `--status, -s <STATUS>` | Filter by status: draft, pending, active, review, done, failed, cancelled, deferred |

#### `orb update <ID>`

Update fields on an existing orb.

```bash
orboros orb update orb-k4f --title "New title"
orboros orb update orb-k4f --priority 1 --status active
```

| Option | Description |
|--------|-------------|
| `--title <TEXT>` | New title |
| `--description <TEXT>` | New description |
| `--priority, -p <N>` | New priority 1-5 |
| `--status, -s <STATUS>` | New status |

#### `orb delete <ID>`

Soft-delete (tombstone) an orb. Tombstoned orbs are excluded from queries.

```bash
orboros orb delete orb-k4f
orboros orb delete orb-k4f --reason "duplicate of orb-abc"
```

#### `orb dep add <FROM> <TO>`

Add a dependency edge between two orbs.

```bash
# orb-b blocks orb-a
orboros orb dep add orb-b orb-a --type blocks

# orb-a depends on orb-c
orboros orb dep add orb-a orb-c --type depends_on
```

| Option | Default | Description |
|--------|---------|-------------|
| `--type, -t <EDGE>` | blocks | Edge type: blocks, depends_on, parent, child, related, duplicates, follows |

Blocking edges (`blocks`, `depends_on`) are validated for cycles.

#### `orb dep rm <FROM> <TO>`

Remove a dependency edge.

```bash
orboros orb dep rm orb-b orb-a --type blocks
```

#### `orb deps <ID>`

List all dependency edges involving an orb.

```bash
orboros orb deps orb-k4f
```

#### `orb review <ID> <DECISION>`

Apply a review decision.

```bash
orboros orb review orb-k4f approve   # advance to next phase
orboros orb review orb-k4f reject    # mark as failed
orboros orb review orb-k4f revise    # send back for changes
```

---

### `daemon`

Run or manage the background queue loop.

```bash
# Start daemon
orboros daemon

# Check status
orboros daemon --status

# Stop running daemon
orboros daemon --stop
```

| Option | Default | Description |
|--------|---------|-------------|
| `--stop` | false | Stop running daemon |
| `--status` | false | Show daemon status |
| `--pid-file <PATH>` | `~/.orboros/orboros.pid` | PID file location |
| `--log-file <PATH>` | — | Log file path |
| `--tick-interval <MS>` | 1000 | Queue loop tick interval |

---

### `tasks`

List legacy tasks (from `tasks.jsonl`).

```bash
orboros tasks
orboros tasks --status done
```

### `status <ID>`

Show details for a legacy task by UUID.

### `review`

List legacy tasks awaiting review.

## Edge Types

| Type | Blocking | Direction | Meaning |
|------|----------|-----------|---------|
| `blocks` | Yes | A blocks B → B waits for A | A must complete before B starts |
| `depends_on` | Yes | A depends_on B → A waits for B | A needs B's output |
| `parent` | No | A is parent of B | Hierarchy |
| `child` | No | A is child of B | Hierarchy |
| `related` | No | Informational link | No scheduling effect |
| `duplicates` | No | A duplicates B | No scheduling effect |
| `follows` | No | A follows B | Suggested ordering (not enforced) |

## Orb Types

| Type | Lifecycle | Description |
|------|-----------|-------------|
| `task` | status | Concrete work item |
| `bug` | status | Defect to fix |
| `chore` | status | Maintenance work |
| `docs` | status | Documentation |
| `epic` | phase | Large initiative, decomposes into features/tasks |
| `feature` | phase | Feature, decomposes into tasks |

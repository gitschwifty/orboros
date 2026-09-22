# CLI Reference

## Global Options

```
orboros [OPTIONS] <COMMAND>
```

| Option | Env Var | Default | Description |
|--------|---------|---------|-------------|
| `--state-dir <PATH>` | — | nearest ancestor `.orbs`, then `~/.orboros/default` | Project state directory |
| `--worker-binary <PATH>` | `HEDDLE_BINARY` | — | Path to heddle-headless binary |
| `--model <MODEL>` | — | configured role default | Explicit model override for workers |

## Commands

### `init`

Initialize a new project in the current directory.

```bash
orboros init
```

Creates `.orbs/orbs.jsonl` and `.orboros/config.toml`. Registers the project in `~/.orboros/projects.toml`.

When `--state-dir` is omitted, Orboros searches upward from the current
directory for a `.orbs` directory, stopping at home. If none is found, it falls
back to `~/.orboros/default`.

---

### `config`

Manage the versioned layered configuration without implicit startup rewrites.

```bash
orboros config init                 # annotated project template
orboros config init --global        # ~/.orboros/config.toml
orboros config init --minimal       # inherit global defaults
orboros config upgrade              # preview schema changes, field examples, and imports
orboros config upgrade --apply      # write the previewed changes
orboros config show                 # effective worker/chat settings
```

---

### `run <TASK>`

Create a task orb, run the normal queue transition plus worker dispatch path in
the foreground, print the persisted result, and exit.

```bash
orboros run "Explain how JWT works" --priority 2
orboros run "Fix the bug" --queue  # create the orb only
```

| Option | Default | Description |
|--------|---------|-------------|
| `--priority, -p <N>` | 3 | Priority 1-5 |
| `--queue` | false | Create the orb without foreground execution |
| `--max-ticks <N>` | 20 | Maximum foreground queue cycles |
| `--interval-ms <MS>` | 100 | Delay between foreground queue cycles |

---

### `execute <ORB_ID>`

Drive the normal orb queue/dispatch path for an existing orb. Without `--wait`,
this performs one foreground queue cycle. With `--wait`, it loops until the
target orb reaches a terminal state, the queue becomes idle, or `--max-ticks`
is reached.

```bash
orboros execute orb-k4f
orboros execute orb-k4f --wait --max-ticks 50
```

| Option | Default | Description |
|--------|---------|-------------|
| `--wait` | false | Continue until the target orb is terminal |
| `--max-ticks <N>` | 20 | Maximum foreground queue cycles |
| `--interval-ms <MS>` | 100 | Delay between foreground queue cycles |

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

`plan` creates an epic plus a local, line-based child scaffold in the selected
state store. A normal plan leaves refinement queued; `--shallow` stops after
persisting the scaffold, leaving the epic in `Decomposing` without entering
`Refining`, `Review`, or `Waiting`. Later queue/daemon execution can
resume planning from that phase. Neither mode runs a worker while creating
the plan. The completion summary names the
state source and gives the next command. Inspect an existing plan with:

```bash
orboros plan --status orb-abc
```

| Option | Description |
|--------|-------------|
| `--file <PATH>` | Read task from markdown file |
| `--shallow` | Shallow decomposition only |

---

### `orb`

Orb management subcommands.

Mutating `orb` commands acquire a short-lived exclusive lease when using a
standalone local state directory. If another CLI mutation is already in
progress, retry after it finishes. For shared-state projects, mutations always
go through the running supervisor daemon; Orboros does not fall back to direct
JSONL writes when that daemon is unavailable or draining.

Read-only `orb` commands print whether their result came from the standalone
local projection or the shared-state projection.

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

#### `orb recover-decomposition <ID>`

Materialize the valid decomposition response already saved on an epic or
feature. This is intended for parent orbs that reached Refining before runtime
decomposition materialization was available. It does not call a worker or
change the parent phase.

```bash
orboros orb recover-decomposition orb-k4f
```

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

Run or manage the background supervisor. By default it processes every project
registered by `orboros init`; use an explicit `--state-dir` for legacy
single-project operation.

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
| `--project <NAME>` | — | Supervise one registered project |

`orboros daemon --status` lists each registered queue with its availability,
state mode, project root, state directory, and effective local concurrency
limit. Supervisor log events carry the same project/root/state attribution.

### `telemetry`

Project telemetry is stored under the registered project's user-local
`~/.orboros/projects/<project-key>/telemetry/` directory, never in the
repository worktree. The compact summary uses exact microdollar accounting;
operator output renders USD.

```bash
orboros telemetry show --project dockyard
orboros telemetry rebuild --project dockyard
```

`rebuild` recreates the durable telemetry projection from the project's
existing `executions.jsonl` evidence. It is appropriate after introducing the
feature to an existing project or if a summary must be recovered.

### `chat` and `sessions`

Start an interactive worker-backed conversation with `chat`. It requires the
normal worker and credential preflight; the transcript is stored under the
selected state directory's `sessions/` directory. Link it to an existing orb
when the conversation is part of that work item.

```bash
orboros chat --link-orb orb-k4f
orboros sessions list --status idle
orboros sessions show session-abc12345
```

Use `sessions show` to replay durable transcript events; it does not start a
worker. Use `chat --chat-model <MODEL>` to override the normal chat model for
one session.

### `hooks`

Hooks are configured separately in `~/.orboros/hooks.toml` and
`.orboros/hooks.toml`, never in general configuration. Validate and inspect
them before allowing a hook to run against live work:

```bash
orboros hooks check
orboros hooks list
orboros hooks run notify --orb orb-k4f --dry-run
orboros hooks log --orb orb-k4f
```

`--dry-run` records what would run but does not spawn the hook command. Hook
invocations are durable evidence; use `hooks log` for recovery or debugging.

### `review-queue`

List parent orbs whose second-opinion verdict is `Revise` and requires an
operator decision:

```bash
orboros review-queue
orboros orb show orb-k4f
orboros orb review orb-k4f revise
```

Inspect the orb before applying a decision. `orb review` is the state-changing
step; `review-queue` is read-only.

### Orb evidence and recovery

```bash
orboros orb logs orb-k4f
orboros orb logs orb-k4f --attempt 2
orboros orb reset orb-k4f --reason "fixed worker setup"
orboros orb rollback-list orb-k4f
orboros orb rollback orb-k4f --count 1
```

`orb logs` lists every durable outer attempt and its Heddle transcript. Reset
only a failed orb; it appends retryable state without deleting evidence.
`orb reset <ID> [--reason <TEXT>]` clears stale outcomes and records the full
failed snapshot, timestamp, operator intent, reason, and target state in
`events.jsonl`. Tasks return to pending; phase orbs return to their evidenced
failed worker phase. Ambiguous phase history is rejected, with no force override.
Existing pipeline copies are synchronized. See
[the phase lifecycle notes](../README.md) for the execution marker rule:
successful nonterminal phase transitions clear the in-flight marker, while
interrupted work keeps it until an explicit reset.
[retrying failed orbs](getting-started.md#retry-a-failed-orb) for limitations.
`rollback-list` shows append-only checkpoints, and `rollback` restores a prior
snapshot while retaining the history that made recovery possible.

### `bench`

Benchmarks keep results under the benchmark root rather than the project log
home. Parent options must come before the benchmark subcommand:

```bash
orboros bench --bench-root ../orboros-bench list
orboros bench --bench-root ../orboros-bench run --tier t1 --jobs 2
orboros bench --bench-root ../orboros-bench list-runs
orboros bench --bench-root ../orboros-bench show <RUN_ID>
orboros bench --bench-root ../orboros-bench report <RUN_ID>
orboros bench --bench-root ../orboros-bench compare <RUN_A> <RUN_B>
orboros bench --bench-root ../orboros-bench archive <RUN_ID>
```

Use `details`, `prompts`, `calibration`, `report-history`, and `storage` for
deeper inspection. `archive` is recoverable: it moves a completed run beneath
the local benchmark archive and preserves its lookup metadata.

---

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

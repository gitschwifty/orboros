# Orboros

Multi-agent orchestrator for software development tasks. Decomposes complex work into subtasks, routes each to an appropriate model, and executes them through worker processes with cancellation, budgets, retries, tool profiles, and end-to-end tracing.

## How It Works

```
User task → Coordinator (LLM) → Subtasks → Router → Workers → Results → Aggregation
```

1. **Coordinator** breaks a high-level task into structured subtasks with types (research, edit, review, test)
2. **Router** maps each subtask's type to a model and tool profile based on TOML config
3. **Workers** (heddle-headless processes) execute each subtask via JSON-line IPC, bounded by a semaphore-based pool
4. **Orchestrator** manages execution order groups (sequential phases, parallel within each), cancellation, budgets, retries, and timeline tracing
5. **Aggregator** synthesizes subtask results into a unified response
6. **Task store** tracks status in append-only JSONL

## Quick Start

```bash
# Build
cargo build

# Set up environment (API key + worker binary)
cp ../examples-env .env  # or create your own
# .env needs:
#   OPENROUTER_API_KEY=sk-or-v1-...
#   HEDDLE_BINARY=/path/to/heddle-headless

# Run a single task
cargo run -- run "What is the capital of France?"

# Decompose without executing
cargo run -- decompose "Add error handling to the REST API"

# Full orchestration: decompose → route → execute all subtasks
cargo run -- orchestrate "Refactor the authentication module"

# List tasks
cargo run -- tasks
cargo run -- tasks --status done

# Check a specific task
cargo run -- status <task-uuid>
```

## CLI Commands

| Command | Description |
|---------|-------------|
| `run <task>` | Execute a single task directly |
| `decompose <task>` | Break into subtasks, print plan |
| `orchestrate <task>` | Decompose + execute all subtasks |
| `tasks [-s status]` | List tasks, optionally filtered |
| `status <id>` | Show details for a specific task |
| `review` | List tasks awaiting human review |

### Global Options

| Flag | Env Var | Default | Description |
|------|---------|---------|-------------|
| `--state-dir` | -- | `~/.orboros/default` | Project state directory |
| `--worker-binary` | `HEDDLE_BINARY` | -- | Path to heddle-headless binary |
| `--model` | -- | `openrouter/free` | Default model |

## Features

### Cancellation & Timeout

- **Cooperative cancellation** via `CancellationToken` (tokio-util) with parent/child hierarchy
- **`CancelSender` handle** sends IPC cancel requests to in-flight workers without `&mut self` aliasing
- **Task-level timeout** fires the cancellation token after a configurable duration
- **Orphan prevention** via `force_stop()` -- graceful shutdown with kill fallback, `kill_on_drop(true)` on all child processes

### Budget Enforcement

- **Token budget tracking** via `BudgetTracker` -- fires the shared cancellation token when cumulative usage exceeds the configured limit
- Budget is an orchestrator concern, not pool-level -- each subtask's usage is recorded after completion

### Retry

- **One-shot retry** for transient failures (Crash/Timeout failure classes)
- Retry is transparent to semaphore management (holds same pool permit)
- No retry on protocol errors, cancellation, or after successful completion

### Tool Profiles

- **Role-based tool restrictions** -- profiles define allowed tools per worker type
- Profiles constrain (ceiling) rather than replace coordinator output
- Denied tools are logged as warnings and captured in `SubtaskOutcome.permission_denials`

### Trace & Observability

- **Harness latency fields** (`model_latency_ms`, `tool_latency_ms`, `total_latency_ms`) flow from IPC through `SendOutcome` → `SubtaskOutcome` → `SubtaskResult`
- **Orchestrator timestamps** (`dispatched_at`, `completed_at`) captured around pool execution
- **`task_id` / `worker_id` correlation** -- set in `WorkerConfig`, sent in IPC `init`, echoed back on events and results
- **`build_timeline()`** reconstructs a `TaskTimeline` from `OrchestrateOutcome` with:
  - Per-subtask `TraceSpan` (wall clock, overhead, latency breakdown, retries)
  - Gap detection: `MissingHarnessLatency`, `MissingTimestamps`, `NegativeOverhead`, `InterGroupGap`, `LatencyMismatch`
  - `TerminationReason` (Completed, PartialFailure, Timeout, BudgetExceeded, Cancelled)

## Model Routing

Place a `routing.toml` in your state directory to route subtask types to different models:

```toml
default_model = "openrouter/auto"

[[rules]]
worker_type = "research"
model = "google/gemini-2.0-flash-001"
reason = "cheap, fast for search tasks"

[[rules]]
worker_type = "edit"
model = "anthropic/claude-sonnet-4-20250514"
reason = "good at code generation"
```

Profiles can restrict tools per worker type:

```toml
[profiles.edit]
allowed_tools = ["read", "write", "glob", "grep"]

[profiles.research]
allowed_tools = ["read", "glob", "grep", "web_search"]
```

See `examples/routing.toml` for a full example.

## Architecture

```
src/
  main.rs           # CLI entry point (clap + tracing)
  lib.rs            # Public API surface
  runner.rs         # Single-task execution: CLI -> worker -> store
  orchestrator.rs   # Multi-subtask orchestration with ordering, cancellation, budgets
  trace.rs          # Timeline reconstruction and gap detection
  coordinator/
    decompose.rs    # LLM-powered task decomposition
    aggregate.rs    # Result aggregation (LLM-powered with fallback)
  ipc/
    types.rs        # Request/Response enums (serde tagged)
    transport.rs    # Read/write JSON lines on child stdin/stdout
    error.rs        # IpcError (thiserror)
  routing/
    rules.rs        # Match worker type -> model (TOML config)
    profile.rs      # Tool profiles -- per-worker-type tool restrictions
  worker/
    process.rs      # Spawn heddle, send/receive, cancel, force_stop
    fsm.rs          # Worker lifecycle state machine (Idle/Initializing/Ready/Running/Draining/Stopped)
    pool.rs         # Semaphore-based concurrency limiter, retry logic
    budget.rs       # Token budget enforcement
  state/
    task.rs         # Task struct + status enum (Pending/Active/Done/Failed/Cancelled)
    store.rs        # Mutex-protected JSONL append + read
```

### Worker Lifecycle (FSM)

```
Idle -> Initializing -> Ready -> Running -> Ready (on success)
                                         -> Draining -> Stopped (on failure/cancel)
```

Failure classes (`Crash`, `Timeout`, `Protocol`, `Cancelled`) map to restart policies (`None`, `RetryOnce`).

### IPC Protocol

Workers communicate over JSON lines on stdin/stdout. Protocol v0.2.0:

- **Requests** (orboros -> worker): `init`, `send`, `status`, `shutdown`, `cancel`
- **Responses** (worker -> orboros): `init_ok`, `event`, `result`, `status_ok`, `shutdown_ok`
- **Events** stream between `send` and `result`: `content_delta`, `tool_start`, `tool_end`, `usage`, `error`, `heartbeat`, `permission_request`, `permission_denied`, `plan_complete`, `context_prune`, `context_compact`, `context_handoff`
- **Result fields** include `session_id`, `task_id`, `worker_id` (correlation), `model_latency_ms`, `tool_latency_ms`, `total_latency_ms` (trace)
- **Error envelope**: `{ code, message, retryable, details? }` on Result and InitOk

Golden transcripts in `test-fixtures/ipc/` are the canonical contract. See `compatibility.md` for the full protocol changelog and compatibility policy.

## Development

```bash
cargo test           # 200 tests (197 unit + 3 integration)
cargo clippy         # lint (pedantic)
cargo fmt            # format
```

Tests use mock worker scripts (`test-fixtures/mock-worker*.sh`) for fast unit tests without external dependencies. Set `HEDDLE_BINARY` to run integration tests against a real heddle-headless instance.

## Project Layout

This repo uses a bare repo + worktree layout:

```
~/repos/orboros/
  .bare/            # bare git repo
  worktree.sh       # creates worktrees with shared file symlinks
  main/             # main branch worktree (this code)
```

## Sister Project

**[Heddle](https://github.com/...)** -- TypeScript LLM harness. Orboros spawns heddle-headless instances as workers. The IPC protocol is defined here and synced to heddle via `scripts/sync-ipc.sh`.

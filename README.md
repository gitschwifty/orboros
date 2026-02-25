# Orboros

Multi-agent orchestrator for software development tasks. Decomposes complex work into subtasks, routes each to an appropriate model, and executes them through worker processes.

## How It Works

```
User task → Coordinator (LLM) → Subtasks → Router → Workers → Results
```

1. **Coordinator** breaks a high-level task into structured subtasks with types (research, edit, review, test)
2. **Router** maps each subtask's type to a model based on TOML config
3. **Workers** (heddle-headless processes) execute each subtask via JSON-line IPC
4. **Task store** tracks status in append-only JSONL

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
| `--state-dir` | — | `~/.orboros/default` | Project state directory |
| `--worker-binary` | `HEDDLE_BINARY` | — | Path to heddle-headless binary |
| `--model` | — | `openrouter/free` | Default model |

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

See `examples/routing.toml` for a full example.

## Architecture

```
src/
  main.rs           # CLI entry point (clap + tracing)
  runner.rs         # Task execution: CLI → worker → store
  coordinator/      # LLM-powered task decomposition
  ipc/              # JSON-line protocol (heddle communication)
  routing/          # Model selection from TOML config
  worker/           # Worker process lifecycle (spawn/send/shutdown)
  state/            # JSONL task persistence
```

### IPC Protocol

Workers communicate over JSON lines on stdin/stdout. Protocol v0.1.0:

- **Requests** (orboros → worker): `init`, `send`, `status`, `shutdown`, `cancel`
- **Responses** (worker → orboros): `init_ok`, `event`, `result`, `status_ok`, `shutdown_ok`
- **Events** stream between `send` and `result`: `content_delta`, `tool_start`, `tool_end`, `usage`, `error`

Golden transcripts in `fixtures/ipc/` are the canonical contract.

## Development

```bash
cargo test           # 52 tests
cargo clippy         # lint
cargo fmt            # format
```

Tests use a mock worker script (`test-fixtures/mock-worker.sh`) for fast unit tests without external dependencies.

## Project Layout

This repo uses a bare repo + worktree layout:

```
~/repos/orboros/
  .bare/            # bare git repo
  .env              # API keys (gitignored, symlinked into worktrees)
  private/          # planning docs (gitignored, symlinked into worktrees)
  link.sh           # symlinks shared files into a worktree
  main/             # main branch worktree (this code)
```

## Sister Project

**[Heddle](https://github.com/...)** — TypeScript LLM harness. Orboros spawns heddle-headless instances as workers. The IPC protocol is defined here and synced to heddle via `scripts/sync-ipc.sh`.

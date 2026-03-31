# Getting Started with Orboros

## Prerequisites

- **Heddle** -- the `heddle-headless` binary, built and on your PATH (or at a known location)
- **API key** -- configured in heddle (OpenRouter, Anthropic, etc.)

## Setup

### 1. Worker binary

Orboros needs to know where `heddle-headless` lives. Pass it on every command:

```bash
orboros --worker-binary /path/to/heddle-headless run "Hello world"
```

Or export it in your shell profile so you don't have to repeat it:

```bash
export HEDDLE_BINARY=/path/to/heddle-headless
orboros run "Hello world"
```

> A proper config file (`~/.orboros/config.toml`) is planned for Phase 3. For now, the CLI flag or environment variable is the interface.

### 2. State directory

Orboros stores task state in a project directory. Default: `~/.orboros/default/`. Created automatically on first use.

```bash
# Use the default
orboros tasks

# Point at a project-specific directory
orboros --state-dir ~/projects/my-app tasks
```

Each state directory gets its own `tasks.jsonl` (task history) and can have its own `routing.toml`.

### 3. Model routing (optional)

Without configuration, all workers use the `--model` flag (default: `openrouter/free`).

To route different subtask types to different models, place a `routing.toml` in your state directory:

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

You can also define tool profiles to restrict which tools each worker type can use:

```toml
[profiles.edit]
allowed_tools = ["read", "write", "execute", "glob", "grep"]

[profiles.research]
allowed_tools = ["read", "web_search", "glob", "grep"]
```

See `examples/routing.toml` for a complete example.

## Commands

### Run a single task

```bash
orboros run "What is the capital of France?"
```

Spawns one worker, sends the prompt, prints the result. Good for testing your setup.

### Decompose a task

```bash
orboros decompose "Add error handling to the REST API"
```

Uses a coordinator worker to break the task into structured subtasks. Prints the plan without executing.

### Full orchestration

```bash
orboros orchestrate "Refactor the authentication module"
```

This is the main workflow: decompose into subtasks, route each to a model, execute (parallel within order groups, sequential across groups), aggregate results.

### Task management

```bash
# List all tasks
orboros tasks

# Filter by status
orboros tasks --status done
orboros tasks --status failed

# Show details for a specific task
orboros status <task-uuid>

# List tasks awaiting review
orboros review
```

### Options

| Flag | Env Var | Default | Description |
|------|---------|---------|-------------|
| `--worker-binary` | `HEDDLE_BINARY` | -- | Path to heddle-headless binary |
| `--state-dir` | -- | `~/.orboros/default` | Project state directory |
| `--model` | -- | `openrouter/free` | Default model for workers |

## What to expect

A successful `orchestrate` run will:

1. Create a parent task in the store
2. Decompose it into numbered subtasks with types and order groups
3. Execute order groups sequentially, subtasks within a group in parallel
4. Print status for each subtask as it completes
5. Aggregate all results into a summary
6. Report the final status (completed, partial failure, timeout, etc.)

Task state is persisted to `tasks.jsonl` in the state directory -- you can always check back with `orboros tasks` or `orboros status <id>`.

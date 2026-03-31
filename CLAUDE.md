# Orboros

Rust multi-agent orchestrator — task decomposition, worker coordination, model routing.

## Runtime & Tooling

- **Rust** (stable toolchain). Never use nightly features unless absolutely necessary.
- **Tokio** for async runtime. Use `#[tokio::main]` and `#[tokio::test]`.
- **Cargo** for everything: build, test, lint, format.
  - `cargo build` / `cargo run`
  - `cargo test` — run full suite before committing
  - `cargo clippy` — fix all warnings
  - `cargo fmt` — always format before committing
  - `cargo watch -x check -x test` — dev loop (install via `cargo install cargo-watch`)
- Never use `rm`, `rm -rf`, `shred`, `unlink`, or `find -delete`. Use `trash` instead.

## Development Workflow

### TDD First

Write failing tests before implementation. Define the contract, then build to it.

**In practice:**
1. Write the test with expected behavior (it won't compile or will fail — that's the point)
2. Implement the minimum code to make tests pass
3. Run `cargo test` before committing
4. Test error paths and edge cases, not just happy paths
5. Use `cargo clippy` and `cargo fmt` before every commit

### Test Organization

```rust
// Unit tests: inline at the bottom of each source file
#[cfg(test)]
mod tests {
    use super::*;  // can access private items

    #[test]
    fn parses_valid_config() { ... }

    #[tokio::test]
    async fn worker_sends_init_message() { ... }
}
```

```
tests/                    # integration tests — only test public API
  worker_lifecycle.rs     # realistic end-to-end flows
  common/mod.rs           # shared test helpers
```

- Name tests descriptively: `test_task_transitions_from_pending_to_active`, not `test1`
- Use `#[tokio::test]` for any test involving async code
- Use `tempfile::tempdir()` for filesystem isolation in tests
- Use `assert_cmd` + `predicates` for CLI integration tests (run binary, assert stdout/stderr)
- Never use `#[should_panic]` when `assert!(result.is_err())` works
- Use `..Default::default()` in tests to only specify fields relevant to the test

### Running Tests

```bash
cargo test                           # full suite
cargo test ipc                       # tests matching "ipc"
cargo test --test worker_lifecycle   # specific integration test
cargo nextest run                    # faster parallel runner (cargo install cargo-nextest)
```

## Error Handling

Two crates, clear split:

- **`thiserror`** — for module-level error types. Typed, matchable enums. Use in `src/ipc/`, `src/worker/`, etc.
- **`anyhow`** — for application glue. Use in `main.rs` and CLI entry points. Add `.context("doing X")` liberally.

```rust
// In a module (e.g., src/ipc/error.rs)
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("failed to parse message: {0}")]
    ParseError(#[from] serde_json::Error),

    #[error("worker process exited with code {0}")]
    WorkerExited(i32),

    #[error("init timeout after {0:?}")]
    InitTimeout(Duration),
}

// In main.rs
fn main() -> anyhow::Result<()> {
    let worker = Worker::spawn(config)
        .context("failed to spawn worker")?;
    // ...
}
```

**Rules:**
- Never use `unwrap()` or `expect()` outside of tests
- Never use `Box<dyn Error>` — pick `thiserror` or `anyhow`
- Always preserve error context — never silently discard the underlying cause
- Use `?` operator everywhere. Avoid manual `match` on `Result` when `?` or `map_err` suffices

## Code Style

- `cargo fmt` enforces formatting. Don't fight it.
- Keep functions small. If a function is hard to name, it's doing too much.
- Use enums for state machines (task status, worker state, message types). Make invalid states unrepresentable.
- Prefer `Result<T, E>` over panics. Reserve `unwrap()` for tests only.
- Use `pub(crate)` to expose internals between modules without making them public API.
- Prefer iterators over indexing (`vec[i]` can panic; iterators can't).
- Don't overuse `.clone()` — it often hides design problems. Reach for references first.
- Use the newtype pattern for type safety on IDs and similar primitives:
  ```rust
  struct TaskId(Uuid);
  struct WorkerId(Uuid);
  // Can't accidentally pass a TaskId where WorkerId is expected
  ```
- Implement `From<X> for Y` for standard conversions rather than custom `to_y()` methods.
- Parse, don't validate: transform unvalidated input into a validated type at the boundary, then pass the validated type throughout.

## Serde Patterns

Serde is central — it handles IPC messages, config files, and state persistence.

- Always derive both `Serialize` and `Deserialize` if the type crosses any boundary
- Use `#[serde(rename_all = "snake_case")]` on enums for JSON output
- Use `#[serde(tag = "type")]` for internally-tagged enums (the IPC message protocol uses this)
- Use `#[serde(default)]` on optional config fields
- Use `#[serde(deny_unknown_fields)]` on strict config types to catch typos
- Keep JSON field names `snake_case` (matches Rust conventions and heddle's protocol)

## IPC Compatibility

- Protocol rules live in `compatibility.md` and `PROTOCOL_VERSION`.
- Always send `protocol_version` in `Init` when supported; always return it in `InitOk`.
- Golden transcripts are the contract; update fixtures on any schema change.
- IPC fixtures live in `fixtures/ipc/` (canonical in Orboros) and are synced into Heddle via `scripts/sync-ipc.sh`.
- Pre-commit hooks enforce protocol version alignment and IPC sync.

## Async Patterns

- **Never block in async context.** No `std::fs`, no synchronous networking, no heavy CPU work in async tasks. Use `tokio::task::spawn_blocking()` to offload.
- **Don't hold locks across `.await`.** Use `tokio::sync::Mutex` if you must, or restructure to drop the lock before awaiting.
- **`kill_on_drop(true)`** on child processes. `tokio::process::Child` does NOT kill the child on drop by default. Always set this for worker processes.
- **Always `.await` on `child.wait()`** eventually, or use `kill_on_drop`. Dropped children become zombies.
- **`select!` cancellation:** When a branch loses the race, its future is dropped. Design futures to handle cancellation safely.
- **Spawned tasks must be `'static + Send`.** Clone or `Arc` what you need before `tokio::spawn`.

## Logging

Use `tracing` (not `log`). Async-aware, structured, span-based.

```rust
use tracing::{info, warn, error, instrument};

#[instrument(skip(provider))]  // auto-generates a span with function args
async fn send_to_worker(worker_id: &WorkerId, message: &str, provider: &Provider) {
    info!(worker = %worker_id, "sending message");
    // ...
}
```

- Use structured fields: `info!(task_id = %id, status = ?status, "task updated")`, not `format!()`
- Control log levels at runtime via `RUST_LOG=orboros=debug,tokio=warn`
- Set up in main.rs:
  ```rust
  tracing_subscriber::fmt()
      .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
      .with_target(false)  // cleaner CLI output
      .init();
  ```

## Clippy Configuration

In `src/main.rs` and `src/lib.rs`:

```rust
#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]  // too aggressive for module re-exports
#![allow(clippy::must_use_candidate)]       // noisy on builder methods
```

Key lints this enables:
- `clippy::unwrap_used` — forces proper error handling
- `clippy::needless_pass_by_value` — prefer borrowing
- `clippy::semicolon_if_nothing_returned` — explicit about return values
- `clippy::match_same_arms` — catch redundant match arms

## Project Structure

```
src/
  main.rs           # thin: parse args, set up tracing + tokio, call lib
  lib.rs            # re-exports, public API surface
  ipc/              # JSON-over-stdio protocol
    mod.rs
    types.rs        # Request/Response enums (serde tagged)
    transport.rs    # Read/write JSON lines on child stdin/stdout
    error.rs        # IpcError (thiserror)
  worker/           # Worker lifecycle
    mod.rs
    process.rs      # Spawn heddle, send/receive messages
    pool.rs         # Manage multiple workers
  coordinator/      # Task decomposition (LLM-powered)
    mod.rs
    decompose.rs    # Break tasks into subtasks
    aggregate.rs    # Combine results
  routing/          # Model selection
    mod.rs
    rules.rs        # Match task type → model
  state/            # Persistence
    mod.rs
    task.rs         # Task struct + status enum
    store.rs        # JSONL append + read
  config/           # TOML config loading
    mod.rs
```

Keep `main.rs` thin — parse args, init tracing, build tokio runtime, delegate to `lib.rs`. This makes everything unit-testable.

## Dependencies

```toml
[dependencies]
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"
anyhow = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
uuid = { version = "1", features = ["v4", "serde"] }
chrono = { version = "0.4", features = ["serde"] }
toml = "0.8"
clap = { version = "4", features = ["derive"] }

[dev-dependencies]
tempfile = "3"
assert_cmd = "2"
predicates = "3"
```

## IPC with Heddle

Workers are heddle processes spawned as subprocesses. JSON lines over stdin/stdout.

- **Orboros sends:** `init`, `send`, `shutdown`, `status`
- **Heddle responds:** `init_ok`, `event`, `result`, `shutdown_ok`, `status_ok`
- One active `send` per worker at a time
- Events stream between `send` and `result`
- Always spawn with `kill_on_drop(true)` and a shutdown timeout

See `compatibility.md` for the full IPC protocol versioning policy and changelog.

## Commit Practices

- Commit tests and implementation together (test proves the impl works).
- Keep commits focused — one logical change per commit.
- Run `cargo test && cargo clippy && cargo fmt --check` before committing.

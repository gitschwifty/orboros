# Worker workdir path contract

Orboros starts each Heddle worker with `WorkerConfig.cwd` as its process
working directory. The workdir-relative-path instruction is currently an
opt-in benchmark experiment, not an assumed production default: it names the
assigned workdir and instructs the worker to recover from a failed path by
checking that directory and retrying relatively.

Absolute paths remain valid only when a task explicitly requires an approved
location outside the repository. Orboros neither rewrites a worker tool path
nor expands its filesystem authority.

## Serial main-worktree completion

The configured project worktree can be the main worktree itself. No container
directory is required. Queue execution requires an explicit worker cwd, an
existing Git HEAD, and serial admission (`max_concurrency = 1`). Concurrent
execution in a shared worktree is rejected until isolated worktrees are supported.
Do not run a second orchestrator or edit the same worktree during an orb.

Before execution, Orboros snapshots HEAD, staged and unstaged diffs, status,
and non-ignored untracked file contents. Pre-existing changed paths are off limits:
even disjoint edits within one such file are ambiguous and require human review.
Unrelated changes may remain staged or unstaged, but must remain identical.
Ownership includes paths changed in either the index or the worktree, even when
an unstaged reversal makes a file's contents match HEAD again.
Ignored files are outside this ownership check; keep runtime evidence ignored.
Unborn repositories and unsupported untracked paths fail before dispatch.

The worker runs relevant validation and creates one focused, non-empty commit
whose subject contains the exact orb ID. Documentation-only changes also require
a commit. Its result must include separate `VALIDATION: <checks and results>` and
`COMMIT: <full SHA>` lines. A true no-op instead requires `NO_CHANGES: <reason>`
and validation evidence; a clean checkout alone is insufficient.

Orboros verifies that HEAD is exactly one non-merge commit above the captured HEAD,
that changed paths do not overlap pre-existing changes, that residual state is
preserved, and that the subject and reported SHA match. The execution ledger stores
`completion_commit` (SHA and subject), or `commit_contract_error`. A claimed
success that violates this contract becomes failed and remains reviewable.
Validation evidence is a worker report, not an independent rerun of the checks.

Execution attempts are not automatically retried or promoted through partial-artifact
recovery: failed, cancelled, or timed-out work stays available for review. Orboros
never creates, resets, or removes commits. If an incomplete worker already moved
HEAD, Orboros records a contract violation requiring manual review. Prompt guidance
cannot prevent a misbehaving worker from running Git before it fails.

Commit creation stays inside Heddle's selected policy, including Git hooks and
signing helpers. A denied commit is a failure, not permission to bypass the policy.
The contract currently applies to queue execution, not standalone low-level worker
dispatches or benchmark dispatches.

## Registered project directories

Registered projects use `ProjectEntry::runnable_path()` (the configured `path`,
falling back to `root_dir`) as Heddle's process cwd. Startup and dynamically
attached projects use the same worker configuration constructor; per-orb
configuration preserves this cwd. Config discovery still uses `config_root()`.
The supervisor's launch directory does not select the worker workspace.

Runnable paths must be absolute, accessible directories. Invalid paths produce
project-specific errors; dynamic dispatch is disabled with a diagnostic rather
than launching a worker with an inherited cwd. A path removed after setup can
still fail when the child process starts.

Orboros queue/state storage remains in its existing `.orbs` directory or shared
state projection. Neither is substituted for a registered worker's cwd. The
single-project daemon resolves registered projects from the selected state
directory; an unregistered legacy `.orbs` directory uses its parent as cwd.
Heddle uses the worker cwd for its own instruction discovery and workspace
policy; Orboros does not change that policy.

## AGENTS.md discovery

Heddle independently discovers `AGENTS.md` from its process cwd through its
ancestors toward the user's home, applying outer instructions before inner ones,
and also considers its home-level instructions. Orboros's launcher directory is
not a context root. For example, when the worker cwd is `~/repos/project/main`,
`~/repos/project/AGENTS.md` is an ancestor instruction file;
`~/repos/project/container/AGENTS.md` is a sibling and is not implicitly loaded.

To apply sibling-container guidance, move it into an appropriate ancestor or
explicitly supply a worker system prompt using existing prompt configuration:

```toml
[prompts.workers.execute]
system_file = "instructions/execute.md"
```

Put the desired guidance in that file (along with the desired execution prompt).
This supplies explicit prompt context; it does not alter Heddle discovery or sandbox
boundaries. Orboros appends the completion contract even with a custom prompt.
The cwd integration test uses a protocol fixture modeling ancestor discovery;
actual Heddle discovery remains an upstream integration responsibility.

## Heddle boundary

Heddle owns repository-tool path validation and file-not-found responses.
To make a failed absolute path actionable without exposing unrelated host
paths, Heddle should, when the failed path is within the assigned repository
scope, return a structured path error containing:

- `code: "path_not_found"`;
- the repository-relative path when it can be derived safely;
- a `retry_relative_to_workdir: true` hint; and
- no parent paths or host paths outside the configured workdir.

Orboros can then persist the structured diagnostic once that IPC field exists.
Until then, the stable Orboros-side protection is explicit workdir-relative
prompt guidance and the existing Heddle sandbox boundary.

## Benchmark experiment

Create a new private prompt set, for example
`<bench-root>/prompts/composable-v1.1-workdir-paths/`, by copying the selected
base composition and adding a versioned workdir-path fragment to the relevant
roles. Run it explicitly:

```sh
orboros bench run --tier 3 --prompt-set composable-v1.1-workdir-paths
```

Without a new prompt-set selection, `composable-v1` is loaded unchanged and
its suite fingerprint remains exactly the base fingerprint. The selected
private set is copied into run artifacts and receives its own manifest/hash.
Compare matching runs with `bench compare` before promoting the guidance to
the default runtime contract.

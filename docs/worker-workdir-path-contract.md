# Worker workdir path contract

Orboros starts each Heddle worker with `WorkerConfig.cwd` as its process
working directory. For normal repository reads, edits, searches, and commands,
workers must use paths relative to that directory. The effective system prompt
names the assigned workdir when one is configured and instructs the worker to
recover from a failed path by checking that directory and retrying relatively.

Absolute paths remain valid only when a task explicitly requires an approved
location outside the repository. Orboros neither rewrites a worker tool path
nor expands its filesystem authority.

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

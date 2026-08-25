# Worker workdir path contract

Orboros starts each Heddle worker with `WorkerConfig.cwd` as its process
working directory. The workdir-relative-path instruction is currently an
opt-in benchmark experiment, not an assumed production default: it names the
assigned workdir and instructs the worker to recover from a failed path by
checking that directory and retrying relatively.

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

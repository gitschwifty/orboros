# Configuration Reference

Orboros uses TOML for its main execution policy. This document is the
reference for every currently supported main-config field.

## Files and precedence

Configuration is merged at the TOML-table level, from lowest to highest
precedence:

1. Built-in compatible defaults.
2. `~/.orboros/config.toml` — user-wide defaults.
3. `<workspace>/.orboros/config.toml` through
   `<project>/.orboros/config.toml` — ancestor-to-child workspace policy.
4. The matching `config.local.toml` files — ignored local worktree overrides.
5. `~/.orboros/projects/<project>/config.toml` — user-local project overrides.
6. `<bench-root>/config.toml`, or the file supplied through `--bench-config` — benchmark-only overlay.
7. `--config <path>` — one explicit final TOML overlay.
8. Explicit CLI options. `--worker-binary` wins over `HEDDLE_BINARY`; either wins over TOML.

Only supplied CLI options override config. In particular, `--model` and
`bench run --jobs` have no implicit CLI default.

`~/.orboros/projects.toml` registers projects. Each entry has a `root_dir`,
the inclusive boundary for workspace-config discovery, and may have a `path`,
the default runnable worktree. If `path` is omitted, `root_dir` is used for
both. This supports a container directory with multiple worktrees without
walking config discovery into unrelated ancestors. Project policy is committed
only when the repository chooses to commit `.orboros/config.toml`;
`.orboros/config.local.toml` is created as an ignored convention for a
machine- or worktree-specific override. Existing `.orbs/config.toml` files are
read as a legacy compatibility layer, but new configuration belongs in
`.orboros/`; `.orbs/` is runtime state only.

Create starter files with:

```bash
orboros config init
orboros config init --global
orboros config init --minimal
```

The normal template is the packaged, complete, reviewable policy; it is also
what `config init --global` installs for a new user-wide configuration.
`--minimal` creates only `config_version = 1`, allowing the project to inherit
user-wide and built-in values until it adds an override. Use `orboros config
show` for a small effective-settings summary.

`orboros config upgrade` advances only schema markers; it never fills omitted
policy fields. It also previews
new optional fields introduced by each schema version, with their default TOML
example and explanation, but never writes those examples automatically. This
preserves a project's deliberate inheritance from global configuration. To
regenerate the complete packaged template, use `config init --force` only after
reviewing or backing up the current file.

Never put provider credentials in TOML. For a worker-spawning command,
Orboros resolves credentials in this order:

1. An explicitly supplied process environment variable, such as
   `OPENROUTER_API_KEY`. This remains the recommended CI and launch-wrapper
   mechanism.
2. On macOS, the current user's Keychain item for OpenRouter, using generic
   password service `orboros.openrouter` and account `$USER`.
3. The user-local fallback `~/.orboros/credentials.env`, only when it is a
   regular non-symlink file with owner-only permissions (`chmod 600`).

Automatic `.env` discovery in the current directory or an ancestor is not
used. This avoids accidentally inheriting a credential from a repository or
parent directory.

To use the macOS fallback, add a generic password item in **Keychain Access**
with service/name `orboros.openrouter`, account equal to your macOS username,
and the OpenRouter API key as its password. Orboros reads it only when
`OPENROUTER_API_KEY` is absent. It never prints the value.

For a headless local fallback, create the file explicitly:

```bash
mkdir -p ~/.orboros
chmod 700 ~/.orboros
${EDITOR:-vi} ~/.orboros/credentials.env
chmod 600 ~/.orboros/credentials.env
```

Use one `NAME=VALUE` line per provider, for example
`OPENROUTER_API_KEY=...`. The file is user-local; do not place it in a project
directory or commit it. Linux keyring integration is tracked separately; the
environment and permission-checked file mechanisms remain portable.

## External prompt sets

Set `[prompts].prompt_set` to a directory containing a composable benchmark
prompt set to use its declared roles during normal queue dispatch. Use an
absolute path in a local ignored config so foreground and supervisor queues
both find the same private corpus without committing prompt contents:

```toml
[prompts]
prompt_set = "/absolute/path/to/bench/prompts/composable-v1"
```

Declared prompt-set roles override the corresponding runtime role only;
undeclared roles continue through normal config and built-in fallback. Each
dispatch records a `prompt_set:<name>:<role>:<assembled-sha256>` source plus
the effective prompt hash in its execution metadata.

### Prompt roles and phases

Use these names in a composable set's `composition.toml` under `[roles.<name>]`.
Only listed roles are replaced; the rest continue to use ordinary overrides or
packaged fallback prompts.

| Composable role | Runtime use | Ordinary override key |
|---|---|---|
| `speccing` | Initial feature/epic specification | `[prompts.phases.speccing]` |
| `decompose` | Child-plan generation | `[prompts.phases.decomposing]` |
| `refining` | Structured specification refinement | `[prompts.phases.refining]` |
| `reevaluating` | Dependency/blocker reassessment | `[prompts.phases.reevaluating]` |
| `execute` | Task and parent-final implementation | `[prompts.workers.execute]` |
| `partial_artifact_recovery` | One bounded recovery/verification pass after a failed changed-workspace dispatch | `[prompts.phases.partial_artifact_recovery]` |
| `decomposition_review` | Dedicated reviewer contract for a decomposition plan (reserved until that gate is dispatched) | `[prompts.workers.decomposition_review]` |
| `refinement_review` | Mandatory reviewer after refinement and before child release | `[prompts.workers.refinement_review]` |
| `completion_review` | Dedicated reviewer contract for completed work (reserved until that gate is dispatched) | `[prompts.workers.completion_review]` |

The non-composable prompt surfaces are still configurable individually:

| Runtime role | Ordinary override key |
|---|---|
| Default fallback | `[prompts.default]` |
| Worker roles | `[prompts.workers.research]`, `edit`, `review`, `test`, `plan`, `execute` |
| Coordinator roles | `[prompts.coordinators.decompose]`, `aggregate` |
| Legacy/general review worker | `[prompts.workers.review]` |

The automated refinement-quality reviewer always runs and resolves
`refinement_review` when a prompt set supplies it. To require a human
decision after it accepts, set `[review].requires_approval_by_default = true`
(or set `requires_approval` on an individual orb); otherwise an accepted spec
advances directly to `Waiting`.

For any ordinary override, select one source form:

```toml
[prompts.phases.refining]
system_file = "prompts/refining.md"
```

## Complete example

```toml
config_version = 1
worker_binary = "/path/to/heddle-headless"
default_model = "openrouter/free"
max_concurrency = 4

[models.default]
worker = "balanced"
coordinator = "planner"
phase = "balanced"
reviewer = "fast"
bench = "balanced"
chat = "fast"

[models.options.balanced]
model = "anthropic/claude-sonnet-4"
description = "General implementation model"
provider = "anthropic"
router = "openrouter"
reasoning = "medium"
effort = "medium"

[models.options.planner]
model = "openai/gpt-5"
router = "openrouter"

[models.options.fast]
model = "openai/gpt-4.1-mini"
router = "openrouter"

[models.workers]
execute = "balanced"
edit = "balanced"
research = "fast"
test = "fast"

[models.coordinators]
decompose = "planner"
aggregate = "balanced"

[models.phases]
speccing = "planner"
refining = "balanced"
reevaluating = "fast"

[models.bench]
default = "balanced"
grader = "fast"

[bench]
timeout_s = 600
max_iterations = 20
jobs = 4

[review]
requires_approval_by_default = false
review_on_completion = true

[second_opinion]
mode = "confidence"
confidence_threshold = 0.7
sampling_rate = 0.1
reviewer_model = "fast"

[notification]
enabled = true
desktop_enabled = false

[prompts.default]
system = "Follow the repository instructions and complete the requested work."

[prompts.workers.edit]
system_file = "prompts/edit.md"

[prompts.coordinators.decompose]
system_file = "prompts/decompose.md"

[prompts.phases.speccing]
system = "Produce a clear implementation specification."

[tool_profiles.edit]
allowed_tools = ["read_file", "write_file", "edit_file", "glob", "grep", "bash"]

[tool_profiles.research]
allowed_tools = ["read_file", "glob", "grep", "web_fetch", "write_file"]
```

Omit any optional section or field to inherit the lower-precedence value.

## Main fields

| Field | Default | Meaning |
|---|---:|---|
| `config_version` | `1` | Configuration schema marker. Existing unversioned configs remain compatible. |
| `worker_binary` | unset | Heddle worker executable. Required for worker-spawning commands unless overridden. |
| `default_model` | `openrouter/free` | Final fallback model selector. |
| `max_concurrency` | `4` | Default concurrent worker dispatch limit. |

## Models

Model selectors can be a key under `[models.options]` or a raw
`provider/model` string. Role-specific mappings win over `[models.default]`,
which wins over `default_model`.

- `[models.default]`: `worker`, `coordinator`, `phase`, `reviewer`, `bench`, and `chat` defaults.
- `[models.workers.<type>]`: worker types such as `execute`, `edit`, `research`, `review`, and `test`.
- `[models.coordinators.<name>]`: `decompose` and `aggregate`.
- `[models.phases.<name>]`: `speccing`, `decomposing`, `refining`, and `reevaluating`.
- `[models.bench]`: `default` for workers and `grader` for benchmark grading.
- `[models.options.<key>]`: a catalog entry. `model` is required; `description`, `provider`, `router`, `reasoning`, and `effort` are optional metadata.

If `router` is omitted, it defaults to `openrouter` for validation metadata.

## Benchmark settings

Put benchmark policy in either the normal project config or, preferably for a
portable corpus, `<bench-root>/config.toml`:

```toml
[bench]
timeout_s = 900
max_iterations = 30
jobs = 4

[models.bench]
default = "balanced"
grader = "fast"
```

Then run normally:

```bash
orboros bench --bench-root ../orboros-bench run --tier t2
```

`jobs` is the number of benchmark cases run concurrently. `bench run --jobs 8`
is an explicit one-run override; otherwise the resolved `[bench].jobs` value is
used, falling back to serial execution (`1`) when absent. `timeout_s` and
`max_iterations` use the same layered/benchmark-overlay precedence. A benchmark
`--model` overrides the benchmark model mapping for that run.

## Prompts and tools

`[prompts.default]`, `[prompts.workers.<type>]`,
`[prompts.coordinators.<name>]`, and `[prompts.phases.<name>]` each accept:

- `system`: inline system prompt text.
- `system_file`: path to a system prompt file. Relative project paths resolve
  from the project configuration context.

Command-level system-prompt flags override these settings for their invocation.

`[tool_profiles.<worker_type>]` has `allowed_tools = ["..."]`. This is a
capability allowlist; `allowed_tools = []` explicitly grants no tools. A
`default` profile applies when there is no exact worker-type profile. The
packaged template defines `read_only`, `research`, `test`, `edit`, and
`execute`; runtime code still intersects policy with its safety ceiling.

## Review, notifications, and hooks

- `[review]`: `requires_approval_by_default` and `review_on_completion`.
- `[second_opinion]`: `mode` (`off`, `always`, `confidence`, or `sampling`),
  `confidence_threshold`, `sampling_rate`, and optional `reviewer_model`.
- `[notification]`: `enabled` and `desktop_enabled`.
- `[logging]`: optional `level` tracing filter (for example
  `orboros=debug,tokio=warn`) and `file` for foreground command logs. The
  global `--log-level` and `--log-file` flags override these settings.
  `retention_days` and `max_size` reserve opt-in age and size limits for
  project evidence; both are unset by default, so logs are retained
  indefinitely until a maintenance policy is deliberately enabled. `max_size`
  accepts a bare byte count (`1024`) or a quoted `K`, `M`, or `G` suffix
  (`"1024K"`, `"1024M"`).
- `[daemon]`: optional `pid_file`, `log_file`, `log_max_size`, and
  `tick_interval_ms` process settings. `global_max_concurrency` optionally
  caps all workers across a multi-project supervisor; each project retains its
  own `max_concurrency` local cap. Set `shared_state = true` only for a
  registered project that is run by the shared supervisor; its orb and
  dependency writes then go through the local authority rather than the
  worktree `.orbs` files. Project `max_concurrency` controls that
  project's dispatch cap; explicit daemon CLI flags override these settings.
  Without a configured file, the multi-project supervisor appends to
  `~/.orboros/supervisor.log`; a daemon targeted to one project appends
  to that project's `logs/daemon.log`. Foreground project commands default
  to the sibling `logs/cli.log` path.
- `[refinement]`: `max_rounds` controls the bounded number of structured
  Refining workers (default `1`); `stop_on_no_material_change` stops when a
  round leaves description/design/acceptance criteria unchanged, and
  `stop_on_model_complete` honors a worker's explicit `"complete": true`
  response after applying that round's edits.
- `[heddle]`: `config_path` is an optional Heddle headless configuration file
  passed as `runtime.config_path` on worker initialization. The normal layered
  config merge applies, so a project setting overrides a global setting. Use
  an absolute path because workers may run from different worktrees.

### Worker evidence

Every queue-dispatched Heddle worker receives an isolated runtime placement.
For a registered project, all operational evidence lives beneath the stable
user-local project log home:

```
~/.orboros/projects/<project-key>/logs/
  daemon.log                 # targeted daemon output
  cli.log                    # foreground command output
  executions.jsonl           # one durable record per outer worker attempt
  heddle/<orb-id>/<phase>/attempt-<n>/
    worker-<worker-id>.jsonl # Heddle transcript
    state/                   # Heddle isolated runtime state
```

This preserves worker evidence across worktrees and across local versus shared
orb state. Use an orb ID, phase, `attempt-<n>`, or worker ID to locate the matching
record and transcript. `orboros orb logs <orb-id>` lists the durable attempts
and transcripts; when there is exactly one transcript it prints it directly.
For retries or multiple phases, use `--attempt <n>` to print the selected
transcript. An explicit unregistered state root uses the equivalent
`<state-root>/logs/` layout. Benchmark runs remain isolated under their case
state directory and are not written into a registered project's log home.

Hooks intentionally use a separate schema and files: `~/.orboros/hooks.toml`
followed by `<project>/.orboros/hooks.toml` (with `<state-dir>/hooks.toml` as
legacy fallback). They are ordered global first, then
project; they are not fields in `config.toml`.

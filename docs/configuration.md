# Configuration Reference

Orboros uses TOML for its main execution policy. This document is the
reference for every currently supported main-config field.

## Files and precedence

Configuration is merged at the TOML-table level, from lowest to highest
precedence:

1. Built-in compatible defaults.
2. `~/.orboros/config.toml` — user-wide defaults.
3. `<project>/.orbs/config.toml` — project policy.
4. `<bench-root>/config.toml`, or the file supplied through `--bench-config` — benchmark-only overlay.
5. Explicit CLI options. `--worker-binary` wins over `HEDDLE_BINARY`; either wins over TOML.

Only supplied CLI options override config. In particular, `--model` and
`bench run --jobs` have no implicit CLI default.

Create starter files with:

```bash
orboros config init
orboros config init --global
orboros config init --minimal
```

The normal template is the packaged, complete, reviewable policy; it is also
what `config init --global` installs for a new user-wide configuration.
`--minimal` creates only `config_version = 2`, allowing the project to inherit
user-wide and built-in values until it adds an override. Use `orboros config
show` for a small effective-settings summary.

`orboros config upgrade` advances only schema markers and imports unconflicted
legacy tool profiles; it never fills omitted policy fields. It also previews
new optional fields introduced by each schema version, with their default TOML
example and explanation, but never writes those examples automatically. This
preserves a project's deliberate inheritance from global configuration. To
regenerate the complete packaged template, use `config init --force` only after
reviewing or backing up the current file.

Never put provider credentials in TOML. Set `OPENROUTER_API_KEY`,
`ANTHROPIC_API_KEY`, or `OPENAI_API_KEY` in the process environment (or a
loaded `.env` file) as required by the resolved router.

## Complete example

```toml
config_version = 2
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
| `config_version` | `2` | Configuration schema marker. Existing unversioned configs remain compatible. |
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

Hooks intentionally use a separate schema and files: `~/.orboros/hooks.toml`
followed by `<state-dir>/hooks.toml`. They are ordered global first, then
project; they are not fields in `config.toml`.

## Legacy routing migration

`routing.toml` is no longer a runtime model-routing source. Before deleting an
old state-dir file, run `orboros config upgrade --apply` from the project: it
imports unconflicted legacy `[profiles]` entries as `[tool_profiles.*]` and
reports that old model rules must be replaced with `[models]` mappings.

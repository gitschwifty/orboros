# CLI polish implementation scope

These changes were implemented without running commands under test, builds,
formatters, linters, or other verification, at the owner's request.

## Workflow guidance (tasks 146 / 121)

The existing normal/shallow scaffold contract is retained. `plan --status ID`
now distinguishes waiting parents with ready children from those with none,
provides inspection/recovery commands, propagates dependency-read errors, and
labels an execution marker as evidence rather than proof of worker liveness.
Added unexecuted unit cases for those guidance contracts.

Remaining: the full first-use journey, command-by-command runtime review matrix,
shared/local state parity, and lifecycle acceptance tests require a later
verification pass. No private review artifacts were changed. When using an
explicit state directory, retain the same `--state-dir` on follow-up commands.

## Low-confidence surfacing

`orboros review-queue --max-confidence 0.5` is a read-only, opt-in human
inspection list across lifecycle states. It includes the threshold, excludes
unscored orbs, and prints each orb's lifecycle and inspection command. The
existing no-argument revise queue is unchanged. Thresholds must be finite and
within [0, 1]; selection and error-path unit cases were added but not run.

Remaining: daemon notification policy and deduplication are deliberately
undecided. A confidence score does not automatically block, reset, or approve
work, and a missing score is not treated as low confidence.

## Live in-flight observations (task 151, bounded first slice)

Worker sends now emit a start, a snapshot every 30 seconds, and a snapshot on
result or transport-read failure through operational tracing. Snapshots contain
worker/session/send IDs, elapsed and last-event age, last tool name, completed
tool count, observed retryable-error count, and latest provisional usage. Usage
snapshots replace prior values; they never enter settled telemetry accounting.
Timer ticks retain the pending IPC read to avoid losing partial JSON lines.
No tool arguments, results, content, or provider error bodies are added.

Remaining: an admission-scoped registry, daemon/orb cross-process views,
model/route attribution, reliable assistant-turn signals, provider retry versus
fresh-worker lineage, cancellation/draining terminal records, durable bounded
restart snapshots, and final reconciliation tests. Event silence is observable
as age, not a claim that the worker has stalled. Heartbeats from Heddle count
as activity. Tests added for snapshot replacement and tool completion were not
executed. This slice does not claim the full task 151 acceptance contract.

## JSON operational logs (task 136, bounded first slice)

`--log-json` opts the general file sink into schema version 1 JSON lines.
Terminal and benchmark sinks retain readable output. The selected operational
path gains `.jsonl` so switching formats does not append JSON to its text file;
rotation uses the actual sink path plus `.1`. Existing project/supervisor log
home selection remains in force. No new dependency is required.

Schema 1 fields: `schema_version`, RFC3339 UTC `timestamp`, `level`, `target`,
`fields` (event key/value map, including message), and `spans` (root-first name
and readable fields). Additive fields are compatible; changing field types or
semantics requires a schema bump. Correlation fields survive where instrumented.
A capture test was added for line boundaries, types, and span context, not run.

Remaining: TOML selection, structured span-field maps, complete attribution
coverage and project-event routing in multi-project supervisors, redaction
review of existing tracing call sites, and rotation/concurrency/isolation
acceptance tests. This formatter adds no prompt/body fields, but is not a
redaction layer for existing tracing events. Explicitly assigning a text sink
to a prior JSON file is unsupported; retain the generated format suffix.

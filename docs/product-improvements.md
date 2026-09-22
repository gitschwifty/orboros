# Near-term product improvements

## Task 121: plan workflow

`orboros plan --file spec.md` constructs a local scaffold, with pending task
children and sequential dependency edges, and leaves the epic in Refining.
It does not run a specification, decomposition, or refinement worker.
`--shallow` leaves the same scaffold in Decomposing; execution can replace it
with worker-generated decomposition before refinement. These phase values
identify queued work, not evidence that earlier workers ran.

Use the same `--state-dir PATH` for creation, `plan --status EPIC`, and
`execute EPIC --wait`. The creation summary prints the selected state root.
A daemon using that state may also advance eligible work. Review gates still
require the configured review decision before children execute.

Status distinguishes an execution marker from verified worker liveness.
An interrupted marker blocks dispatch: inspect daemon and attempt status before
using the existing orb recovery/reset workflow. Dependency store errors are
reported rather than rendered as an empty ready queue.

Remaining: replace scaffold phase shortcuts with durable scaffold provenance,
show project-root selection and actual supervisor ownership/admission in status,
and exercise queued/running/blocked/review/completed CLI fixtures. Existing
normal/shallow CLI assertions were extended but not run by instruction.

## Task 135: retry diagnostics

Attempt records now carry an optional stable `failure_cause`; historical
records without the field still deserialize. Structured policy, provider,
cancellation, malformed-tool-call and terminal-worker causes are distinguished;
unclassified failures explicitly remain `unknown`. Retry cause lines include
termination reason and configured retry limit. Structured failures supply a
human-readable error even when the worker omits its error string.

The retry counter now advances on every retry rather than resetting to one,
which otherwise permits an unbounded loop with a configured limit above one.

Remaining: typed transport attribution through spawn/send error paths, phase
recovery labels, telemetry-level indexing and exhaustive fixtures. Existing
human-readable error storage is unchanged; complete redaction of arbitrary
upstream error bodies needs a separate boundary audit. No checks were run.

## Tasks 138–142: graph, admission, boundaries and terminal validity

138: explicit local IDs and `depends_on` now override legacy order groups.
Validation rejects mixed schemas, duplicate/empty IDs, unknown references,
self-dependencies and cycles before materialization. Explicit graphs persist
exact prerequisite edges, including fan-in/fan-out; node summaries are logged.
The prompt requests explicit dependencies and artifact rationale. Historical
order-only output remains accepted. Remaining: enforce graph-only new worker
output separately from historical readers, durable local-ID graph evidence,
rationale validation and updated end-to-end worker fixtures.

139: each queue dispatch pass checks declared child edges and current child
statuses before admitting a child-bearing phase parent for final execution.
Missing edge targets and reset/failed children block admission, as does a false
final-work decision. Logs expose the checkpoint. Remaining: durable accepted
checkpoint/attempt lineage, atomic admission against concurrent reset, and
restart/rollback exactly-once guarantees. A log checkpoint is not durable proof.

140: dispatch logs report effective local cap, queued candidates and available
aggregate permits, explicitly distinguishing queued state from in-flight work.
Remaining: configuration-layer provenance, startup aggregate `unbounded` output,
periodic usage/status integration and individual capacity-blocked decisions.

141: no enforcement change. Current profiles filter tool names, not filesystem
access. A correct denial covering bash, symlinks and traversal requires an
execution-runtime capability in Heddle. No cross-repository changes were made.
Still needed: runtime path denial, bounded current-state capability, separate
research/artifact-write profile and durable denial diagnostics. Prompt advice
cannot satisfy this access boundary.

142: obvious terminal DSML/XML/native JSON tool-call envelopes fail closed with
`malformed_terminal_output`, enter bounded fresh-worker retry, and are removed
from the ordinary result. No worktree-edit requirement is imposed. The existing
protected worker transcript remains the place for raw response evidence.
Remaining: profile-specific structured completion fields, nested/mixed provider
fragments and broader protocol fixtures. Focused graph and terminal-envelope
tests were added but not executed.

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

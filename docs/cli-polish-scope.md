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

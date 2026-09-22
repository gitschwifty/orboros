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

# Reliability implementation boundaries

No tests, builds, formatting, linting, or verification were run, by request.

## Durable queue dispatch claims

Queue dispatch creates and syncs an exclusive `dispatch-claims/<sha256 orb id>.claim`
next to the orb store before launching any worker in that dispatch episode. A claim
covers its retries, structured repair, and reviewers. Normal completion syncs the
orb projection before archiving the claim as `<key>.<attempt id>.resolved`. Failed
setup, cancellation, ambiguous persistence and crashes leave the claim in place.
Even empty/torn claim files quarantine the orb. Resetting an orb does not clear it.

Recovery is deliberately manual: stop all project owners, establish that workers
and descendants are gone, inspect task side effects and evidence, reconcile the
orb, then archive the claim in the same directory and sync that directory before
restarting. Do not delete a claim merely because a PID is absent.

Limits: this is a durable local-filesystem dispatch-episode guard, not a new
revision-checked journal operation or a per-launch attempt ledger. Exported direct
worker/dispatcher APIs are outside this queue guard. Operator edits are reloaded
at admission but are not transactionally serialized with the claim or subsequent
outcome. Dependency checks reject missing predecessors at initial execution
admission; retry-time concurrent graph edits remain outside that contract.

## Queue ownership and shutdown admission

Dynamic ticks borrow cloned queue handles while the registry retains authority.
Detach stops admission immediately, but reports `project_draining` while a tick
or dispatch owns work; retry detach after draining to release the lease. Reattach
while draining is still the same stopped attachment. Snapshot release never
restores a queue. Shutdown stops dynamic queues as well as startup queues and
rejects new attachments. Stopped queues also skip lifecycle ticks.

Limits: detach uses explicit retry rather than an asynchronous completion reply.
A cancelled snapshot remains pinned until supervisor exit (safe loss of liveness).
Already-running workers drain using existing deadlines; this is not a hard
wall-clock bound on daemon shutdown. Synchronous transitions already in progress
can finish after admission closes.

## Owned worker teardown

Unix workers retain a process-group guard from spawn through init, send and
shutdown. Cleanup sends SIGKILL to the private group before reaping its leader;
a numeric group identity is never reused after reaping. Init timeout follows the
same explicit cleanup path. Graceful shutdown and forced stop also terminate
remaining inherited descendants. Cleanup errors stop dispatcher retries and
leave queue claims unresolved. Drop/cancellation has best-effort group signaling
plus Tokio child kill-on-drop. Shutdown handshakes default to five seconds;
explicit cleanup bounds leader reaping to five seconds.

Limits: descendants that create a new session/group escape this mechanism.
Non-Unix cleanup covers the direct child only. Descendants are signaled but
cannot be reaped portably by Orboros; this is not OS-level process-tree
containment. Drop cannot report failure or await cleanup. Runtime destruction
can still defeat asynchronous reaping. Direct callers of force_stop must handle
its newly returned Result before reusing a checkout.

## Completion review and reevaluation

Completion review persists the reserved label
`orboros:checkpoint:post_completion` in the same orb update as Review. It survives
clearing execution metadata and restart. CLI approval reaches Done; revision
returns to Executing and clears dispatch metadata. Review decisions remove the
label. New post-refinement reviews clear it. Existing provenance/child-based
legacy inference remains for old records. The label is a compatibility bridge
for the pinned external Orb schema, not a dedicated schema field; operators must
not edit this reserved label. Ambiguous legacy reviews still need manual review.

Reevaluation success now parses and applies Continue/Pivot/Abort directly from
Reevaluating, propagating transition errors. Invalid verdicts become Failed.
The queue no longer applies a second verdict after generic advancement.

## Parent/child lifecycle and decomposition reconciliation

Recovery preflights child hierarchy, surplus children and conflicting internal
ordering before writing anything. It retains existing children verbatim (including
edits, status, execution and results), adds only missing children/edges, and pins
the accepted plan in a synced `decomposition-plans/<parent hash>.json` before
materialization. Replaying the same plan can finish a partial graph. A changed
pinned plan is rejected for explicit generation reconciliation. Torn plan files
fail closed. Existing legacy graphs have no original plan identity: first recovery
pins the supplied saved plan while preserving every existing child record.

Missing/cyclic ancestors and failed/cancelled/tombstoned parents block descendants.
Task parents aggregate children instead of dispatching duplicate parent work;
phase parent final execution requires every child Done. Root completion reloads
state after admission transitions and never advances tombstoned parents.

Limits: graph writes remain multiple operations, not a revision-checked batch;
callers must serialize recovery with scheduling/operator writes. Retiring obsolete
children/edges and replacing a pinned plan require an explicit generation policy
and remain deferred. Parent aggregation still references child records rather
than storing an immutable child-revision evidence bundle. A child edited/reset
after parent completion does not automatically reopen the parent. The legacy
plan adoption path cannot distinguish changed plan text from prior child edits.

## Delivery status

Signed slice commits were blocked: the worktree Git metadata is under
`/Users/pjtaggart/repos/orboros/main/.git/worktrees/reliability-fixes`, outside
writable roots. Git could not create `index.lock`. No commits were created and
no review artifacts or main-worktree files were edited. Commit hooks were disabled
for the attempted commit to honor the prohibition on verification.

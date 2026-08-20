# Data retention and JSONL operations

Orboros uses append-only JSONL because it is inspectable, portable, and easy
to recover from a partial final line. It is not a transactional multi-writer
database: one Orboros process should own a state directory or benchmark
results directory at a time. Put shared state on reliable local storage,
serialize writers with the daemon/queue owner, and back it up before manual
maintenance.

## Store policy

| Store | Keep active | Compact/archive | Recovery and lookup |
| --- | --- | --- | --- |
| `orbs.jsonl`, `deps.jsonl` | Current canonical state | `orbs` pipeline merge writes final state; run `store.compact()` periodically for superseded canonical rows. Preserve the previous file in a dated backup before a manual rewrite. | Latest entry per ID is authoritative; restore a backup then replay JSONL if a compaction is interrupted. |
| `events.jsonl`, `hooks.log.jsonl` | Audit trail and hook outcomes | Never silently compact. Copy closed-pipeline audit files to a dated archive after project policy permits; retain event IDs and timestamps. | Archives remain JSONL and can be searched with `rg`/`jq`; copy them back beside the store for tooling that expects the active path. |
| Pipeline snapshots and `history/` | Current pipeline plus interruption-recovery snapshots | Retain every snapshot until the pipeline has merged and a backup exists. Archive the complete dated pipeline directory; do not delete the only snapshot. | Restore the complete snapshot directory, then use normal pipeline recovery. |
| Benchmark `runs.jsonl` and per-run evidence | `runs.jsonl` stays as the compact index; active run directories stay local for current analysis | `orboros bench archive <run-id>` atomically moves a completed run to `results/archive/YYYY-MM/<run-id>`. It never overwrites or deletes data. | `bench show`, `details`, `report`, and `prompts` still find archived runs. Move the directory back to restore it. |
| Heddle transcripts, workdirs, prompts, and execution ledgers | Keep structured execution telemetry and benchmark dispatch/prompt evidence needed for reports | Prune raw workdirs/transcripts only after their structured execution, policy, and prompt evidence has been retained. Benchmark artifacts may be archived with their run. | The execution ledger and benchmark dispatch records remain queryable when raw transcripts are removed. |

`orboros bench storage` measures active and archived benchmark evidence without
replaying JSONL. It warns at 512 MiB total evidence, 2 GiB archived evidence,
and when the oldest evidence is 90 days old. These are review prompts, not
automatic deletion thresholds.

## Backup, corruption, and maintenance

Back up `.orbs/` (including `pipelines/`, `snapshots/`, `history/`, execution
and prompt ledgers) and benchmark results as directory snapshots. JSONL readers
skip malformed lines so an interrupted final append does not prevent earlier
history from loading; investigate and preserve the original before repairing a
file. Archive moves use a same-filesystem rename, so the run is either at its
active location or its archive location, never a partially copied replacement.

Do not run compaction, archive, or a second daemon concurrently with a writer.
JSONL append and read behavior does not provide cross-process locking or
multi-record transactions.

## When to revisit an embedded database

Keep the measurements from `bench storage`, representative `orb list`/startup
timings, and writer errors in operational notes. Re-evaluate SQLite (or another
embedded database) only when realistic workloads show one or more of these:

- Canonical-store replay or normal CLI startup exceeds 2 seconds at the 95th
  percentile, or benchmark history queries exceed 5 seconds.
- Active JSONL evidence exceeds 512 MiB after normal archive/compaction, or
  local archives exceed 2 GiB and need indexed selective retention.
- A project needs reliable simultaneous writers, transactional updates across
  orb/dependency/audit records, or contention causes failed/lost operations.
- Retention requires selective queries/deletion across many runs that cannot be
  performed safely by archiving whole self-contained directories.

Before migrating, capture a representative corpus, compare correctness and
query latency before/after, preserve a JSONL export/recovery path, and test an
interrupted migration. Size alone is not sufficient reason to replace JSONL.

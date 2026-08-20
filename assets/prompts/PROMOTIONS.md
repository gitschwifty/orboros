# Packaged prompt promotion ledger

This ledger records benchmark-backed changes to prompts compiled into Orboros.
It contains identifiers and hashes only: private benchmark prompts, fixtures,
grader text, and run artifacts do not belong in this repository.

There are no entries yet. Existing packaged prompts predate this workflow.

## Entry template

```markdown
## YYYY-MM-DD — <role> — <short description>

- Destination: `assets/prompts/<role>.md`
- Destination SHA-256: `<hash after promotion>`
- Previous / rollback SHA-256: `<hash before promotion>`
- Benchmark source: prompt set `<name>`, role `<role>`, assembled SHA-256 `<hash>`
- Evidence: baseline `<run-id>`, candidate `<run-id>`, suite `<fingerprint>`
- Comparable configuration: `<config hash>`; worker `<model>`; grader `<model>`
- Result: `n=<n>`; pass-rate `<baseline> -> <candidate>`; cost `<delta>`; latency `<delta>`
- Review: `<reviewer and date>`
- Decision and notable failure modes: `<concise summary>`
- Product commit: `<commit>`
```

The destination hash must be produced with `shasum -a 256` after the file is
copied. See [`docs/benchmark-prompt-promotion.md`](../../docs/benchmark-prompt-promotion.md)
for the complete workflow and rollback procedure.

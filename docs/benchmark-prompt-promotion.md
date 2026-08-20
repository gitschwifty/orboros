# Benchmark prompt promotion

Benchmark prompt sets are private experimental inputs. Packaged prompts under
`assets/prompts/` are product defaults compiled into Orboros. Promotion is a
maintainer-reviewed product change, never an automatic consequence of a single
benchmark run. This workflow deliberately does not require a private corpus in
public CI or distribution.

## Evidence bar

A promotion request must name all of the following:

- the candidate prompt-set name, role, source file or fragment list, and
  assembled SHA-256;
- at least one baseline and candidate run ID, their suite fingerprints, prompt
  manifests, resolved worker/grader models, and configuration hashes;
- comparable sample size per affected category (normally at least three
  attempts per case, with more evidence for a default that affects many roles);
- pass/fail/error rate, cost, latency, and important failure-mode deltas; and
- a human reviewer who has read representative successful and failed artifacts.

The suites and prompt manifests must match except for the candidate prompt
input. A candidate must not materially regress correctness, scope adherence, or
review quality in any affected category. A gain that costs materially more or
adds unacceptable latency requires an explicit trade-off in the review. Small
samples are evidence for a follow-up, not a universal-default claim.

## Promote one role

Only a fully resolved role prompt is promoted. For a role assembled from
fragments, use the source fragments and order recorded in the selected run's
`run.json` prompt manifest to create the reviewed resolved Markdown file first.
Do not copy an unreviewed fragment into a packaged default.

The role must have a matching packaged destination. Today this is true for
benchmark `execute` -> `assets/prompts/execute.md`. Other benchmark roles need
a separate product change that adds an explicit packaged prompt/resolver path;
do not silently overload an unrelated worker prompt.

From an isolated checkout containing the approved resolved source file:

```bash
cp /absolute/path/to/resolved-execute.md assets/prompts/execute.md
shasum -a 256 assets/prompts/execute.md
```

Then add a dated entry to
[`assets/prompts/PROMOTIONS.md`](../assets/prompts/PROMOTIONS.md) with the
printed destination hash and the required evidence fields below. Include the
same compact provenance in the signed commit message. The PR/commit changes
only the packaged prompt and public ledger; it must not add the private source,
fixture, grader text, or results artifact.

```text
Promote execute prompt

Benchmark: prompt-set=<name> role=execute assembled-sha256=<hash>
Evidence: baseline=<run-id> candidate=<run-id> suite=<fingerprint>
Review: <reviewer>; rollback=<prior packaged sha256>
```

Run the ordinary public checks after the copy:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Public CI validates the prompt that was committed. It does not fetch or inspect
the private benchmark corpus; the ledger hashes and run IDs are provenance, not
runtime dependencies.

## Review checklist

The reviewer verifies that the destination hash in the ledger equals the file
in the diff, the source assembled hash matches the candidate run manifest, the
baseline/candidate suite and grader identities are comparable, and the listed
metrics accurately summarize the linked private evidence. They also check that
the prompt remains appropriate for the product role and contains no private
task data, secrets, or corpus paths.

## Versioning and rollback

The destination file's SHA-256 is its immutable content identity; the Git
commit is its release version. Never amend an existing ledger entry. A later
promotion appends a new entry with the previous destination hash as its rollback
target.

To roll back, restore the prior prompt content from its recorded Git commit,
verify that it hashes to the recorded `rollback_to_sha256`, append a new ledger
entry naming the incident/evidence, and run the same public checks. This makes
rollback an auditable new release rather than deleting history.

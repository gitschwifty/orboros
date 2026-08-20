# Model-by-diff-type benchmark pilot

This is the protocol for selecting Orboros models from benchmark evidence.
It intentionally keeps the private corpus outside this repository. A public
run artifact retains case hashes, taxonomy, rubric identity, selected prompt
manifest, model/configuration metadata, and measurements; it never needs the
private fixture or rubric text to remain interpretable.

## Classification

Every pilot case declares both axes in `case.toml`. They are independent: a
review of a cross-cutting change is `review` work over a `cross_cutting` diff,
not a new combined category. A case may list more than one value only when
both are intentionally measured; reports must display each axis separately,
not fold them into one score.

| Work type | Include when the case primarily assesses |
| --- | --- |
| `speccing` | turning a request into constraints, acceptance criteria, and a scoped plan |
| `decomposition` | splitting work, ordering dependencies, and retaining parent-final work |
| `exploration` | locating relevant code or evidence before proposing action |
| `execution` | making a correct implementation change |
| `refinement` | incorporating feedback while preserving prior correct work |
| `review` | finding material issues and communicating actionable, scope-aware feedback |

| Diff type | Include when the expected repository outcome is |
| --- | --- |
| `bugfix` | a targeted regression or incorrect behavior correction |
| `feature` | new user-visible behavior or capability |
| `refactor` | structural improvement with preserved behavior |
| `tests` | tests or test infrastructure as the principal change |
| `documentation_configuration` | docs, examples, configuration, or packaging behavior |
| `cross_cutting` | coordinated changes spanning three or more concerns/modules |

Do not use a label simply because a task contains a small incidental edit of
that kind. A pilot task needs an explicit expected outcome, a bounded fixture,
and a deterministic check where one is practical.

## Case metadata and rubrics

The loader accepts this optional metadata and snapshots it in the run's suite
manifest. `prompt_sha256` is the SHA-256 of the complete private grader prompt
after its task rubric is assembled.

```toml
[taxonomy]
work_types = ["execution", "review"]
diff_types = ["bugfix"]

[grader]
rubric_id = "review-bugfix"
rubric_version = "v1"
prompt_sha256 = "..."
```

Use `expected.kind = "rubric"` for the task-specific criteria. The grader must
return a criterion-level verdict and `OVERALL: PASS`/`FAIL`; preserve its
response in the private result artifact. Deterministic tests remain the source
of truth for build and behavior checks. The rubric evaluates what tests do not:
scope adherence, reasoning/review quality, and completeness.

Changing a case, taxonomy, rubric version, or grader prompt hash is a suite
change. It requires a new comparable cohort, not an append to an old result.

Tag every rubric-scored pilot case with `ai-graded` and `pilot-82`. This makes
the AI-graded subset explicit and runnable without maintaining a fragile list
of IDs:

```bash
orboros bench run --tag ai-graded --tag pilot-82 --model <catalog-key>
```

Repeated `--tag` filters are conjunctive: a selected case must have every
requested tag. Tags are selection labels only; the task taxonomy and grader
identity remain the recorded evaluation metadata.

## Initial corpus: 12-case pilot

Create two small, self-contained private cases for each selected category.
The identifiers below are the intended stable names; fixtures should remain
minimal and have no network dependency.

| Primary work | Diff type | Cases | Expected outcome |
| --- | --- | --- | --- |
| exploration | bugfix | `explore-regression-location`, `explore-config-precedence` | identifies the owning code path and evidence, without proposing unrelated changes |
| speccing | feature | `spec-cli-filter`, `spec-config-migration` | scoped acceptance criteria, risks, and non-goals are complete and testable |
| decomposition | cross_cutting | `decompose-worker-cancellation`, `decompose-result-retention` | dependency-aware children, safe parallelism, and parent-final work when needed |
| execution | bugfix | `fix-empty-selector`, `fix-retry-budget` | regression test and behavior both pass |
| execution | feature | `add-list-filter`, `add-config-override` | requested behavior, focused tests, and no API breakage |
| execution | refactor | `refactor-config-loader`, `refactor-result-store` | preserved behavior, tests pass, and duplication is reduced without speculative churn |
| execution | tests | `test-worker-timeout`, `test-phase-transition` | test catches the seeded fault and is deterministic |
| refinement | documentation_configuration | `refine-cli-docs`, `refine-default-config` | feedback is incorporated, examples are executable/accurate, and unrelated text is unchanged |
| review | cross_cutting | `review-routing-change`, `review-bench-provenance` | identifies seeded material defects with actionable, correctly scoped findings |

The table deliberately contains multiple representative tasks for every work
and diff category used by the pilot. A case can carry secondary labels when
necessary, but its primary row is the grouping used to ensure balanced sampling.

## Experiment and report

Start with `composable-v1` and one fixed suite fingerprint. Run each candidate
and single-model baseline at least three times per case, interleaving variants
instead of running all attempts of one model together. Record the model catalog
key and resolved provider/model; never compare dynamic router aliases whose
actual backend is unknown.

For T3, run the single-model baseline first, then phase-model assignments. A
phase-model variant must name every role assignment (for example,
`spec=fast,decompose=fast,execute=strong,parent_final=strong,grader=grader`).
The T3 summary reports end-to-end pass rate and total cost beside phase-level
failures, retries, assistant turns, tool calls, and timing. Planning models
therefore are not credited or blamed for later worker execution.

Use `orboros bench compare` only for matching suite and prompt manifests, and
`orboros bench report` to inspect retained dispatch telemetry. The private
results report must include this table for every work type and every diff type:

| Axis/value | Model or phase assignment | n | pass rate | 95% interval | median cost | median latency | failures |
| --- | --- | ---: | ---: | --- | ---: | ---: | --- |
| `diff_type=bugfix` | `baseline-strong` |  |  |  |  |  |  |

List notable failure modes verbatim at a useful level (for example, malformed
decomposition, scope expansion, test failure, timeout, provider failure, or
grader disagreement), together with the relevant case IDs. Do not publish a
single global ranking. With this small pilot, intervals and sample size are
decision context, not a claim of statistical certainty.

The pilot is actionable only if it yields a repeatable recommendation such as
"use model X for exploration and Y for execution bugfixes" without a material
end-to-end regression. Otherwise expand only the ambiguous categories, audit
the rubric disagreements, and keep the existing routing defaults.

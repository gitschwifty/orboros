//! T1 benchmark runner.
//!
//! Iterates cases, spawns a worker per case, compares the worker
//! response to the case's expected value, and writes per-attempt
//! results to the store. Each T1 case runs N=3 times and is graded
//! as Pass iff a majority of attempts match the expectation.
//! Determinism choice resolved with the user: 3-run majority is
//! cheap, robust to occasional model noise, and slows the benchmark
//! by ~3× without requiring provider-specific seed support.

use std::time::Instant;

use chrono::Utc;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::bench::case::{BenchCase, BenchExpected, BenchTier};
use crate::bench::store::{new_run_id, BenchResult, BenchRun, BenchStatus, BenchStore};
use crate::worker::process::{Worker, WorkerConfig};

/// Outcome of grading a single attempt's response against the case's
/// expectation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptOutcome {
    Pass,
    Fail,
    /// Expectation can't be evaluated by the in-process matcher (T2
    /// `TestsPass` and T3 `Rubric`) — the caller is responsible for
    /// running the external grader.
    Unsupported,
}

/// Grades a response against a [`BenchExpected`] for T1's in-process
/// matchers. Returns `Unsupported` for the matcher kinds that need
/// an external runner (T2/T3).
///
/// # Errors
///
/// Returns an error when the expectation is malformed (e.g. an
/// invalid regex).
pub fn grade_attempt(response: &str, expected: &BenchExpected) -> anyhow::Result<AttemptOutcome> {
    match expected {
        BenchExpected::Exact { text } => Ok(if response.trim() == text.trim() {
            AttemptOutcome::Pass
        } else {
            AttemptOutcome::Fail
        }),
        BenchExpected::Regex { pattern } => {
            let re = regex::Regex::new(pattern)
                .map_err(|e| anyhow::anyhow!("invalid regex `{pattern}`: {e}"))?;
            Ok(if re.is_match(response) {
                AttemptOutcome::Pass
            } else {
                AttemptOutcome::Fail
            })
        }
        BenchExpected::TestsPass { .. } | BenchExpected::Rubric { .. } => {
            Ok(AttemptOutcome::Unsupported)
        }
    }
}

/// Number of attempts the T1 runner makes per case before grading.
pub const T1_ATTEMPTS: u32 = 3;

/// Majority threshold for T1 — number of passing attempts required
/// to grade the case as Pass.
pub const T1_PASS_THRESHOLD: u32 = 2;

/// Options for [`run_t1_case`].
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// If false, the harness aborts further attempts on a case once
    /// `max_cost_cents` is exceeded. If true, all N attempts run
    /// regardless of accumulated cost.
    pub no_budget: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self { no_budget: false }
    }
}

/// SHA-256 of the prompt, hex-encoded. Used for `prompt_hash` on
/// every result so `bench compare` can detect prompt drift between
/// runs.
#[must_use]
pub fn prompt_hash(prompt: &str) -> String {
    let mut h = Sha256::new();
    h.update(prompt.as_bytes());
    let digest = h.finalize();
    digest.iter().fold(String::with_capacity(64), |mut acc, b| {
        use std::fmt::Write;
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

/// Runs one T1 case end-to-end: N attempts, in-process grader,
/// majority verdict. Returns the assembled [`BenchResult`].
///
/// # Errors
///
/// Returns an error if the worker can't be spawned at all. Per-attempt
/// failures are folded into the result rather than propagated.
pub async fn run_t1_case(
    case: &BenchCase,
    run_id: &str,
    base_worker_config: &WorkerConfig,
    opts: &RunOptions,
) -> anyhow::Result<BenchResult> {
    if case.tier != BenchTier::T1 {
        anyhow::bail!("run_t1_case called on non-T1 case {}", case.id);
    }
    let started = Instant::now();
    let mut passes: u32 = 0;
    let mut fails: u32 = 0;
    let mut accumulated_cost: u32 = 0;
    let mut last_confidence: Option<f32> = None;
    let mut total_iters: u32 = 0;
    let mut last_error: Option<String> = None;

    for attempt in 0..T1_ATTEMPTS {
        // Budget gate before spawning.
        if !opts.no_budget && accumulated_cost >= case.max_cost_cents {
            warn!(
                case = %case.id,
                attempt,
                accumulated_cost,
                max = case.max_cost_cents,
                "skipping remaining attempts: max_cost_cents exceeded"
            );
            break;
        }

        let mut wc = base_worker_config.clone();
        wc.system_prompt = "Answer concisely.".into();
        wc.max_iterations = Some(1);

        let mut worker = match Worker::spawn(&wc).await {
            Ok(w) => w,
            Err(e) => {
                last_error = Some(format!("spawn failed: {e}"));
                fails += 1;
                continue;
            }
        };

        let send_id = format!("{}-{}", case.id, attempt);
        let outcome = match worker.send(&send_id, &case.prompt).await {
            Ok(o) => o,
            Err(e) => {
                last_error = Some(format!("send failed: {e}"));
                fails += 1;
                let _ = worker.shutdown().await;
                continue;
            }
        };
        let _ = worker.shutdown().await;

        let response = outcome.response.clone().unwrap_or_default();
        let attempt_outcome = match grade_attempt(&response, &case.expected) {
            Ok(o) => o,
            Err(e) => {
                last_error = Some(format!("grade failed: {e}"));
                AttemptOutcome::Fail
            }
        };

        // Approximate per-attempt cost — placeholder until a usage→cost
        // mapping is wired up. Keeps the budget gate exercised.
        accumulated_cost = accumulated_cost.saturating_add(1);

        if outcome.confidence.is_some() {
            last_confidence = outcome.confidence;
        }
        total_iters = total_iters.saturating_add(outcome.iterations);

        match attempt_outcome {
            AttemptOutcome::Pass => passes += 1,
            AttemptOutcome::Fail => fails += 1,
            AttemptOutcome::Unsupported => {
                anyhow::bail!(
                    "T1 case {} uses an unsupported expectation (Tests/Rubric)",
                    case.id
                );
            }
        }
    }

    let attempts_run = passes + fails;
    let status = if attempts_run == 0 {
        BenchStatus::Error
    } else if passes >= T1_PASS_THRESHOLD {
        BenchStatus::Pass
    } else {
        BenchStatus::Fail
    };
    let score = if attempts_run == 0 {
        0.0
    } else {
        f64::from(passes) as f32 / f64::from(attempts_run) as f32
    };

    info!(
        case = %case.id,
        status = ?status,
        passes,
        fails,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "T1 case complete",
    );

    Ok(BenchResult {
        case_id: case.id.clone(),
        run_id: run_id.into(),
        tier: BenchTier::T1,
        status,
        score,
        latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        cost_cents: accumulated_cost,
        iterations: total_iters,
        worker_model: base_worker_config.model.clone(),
        prompt_hash: prompt_hash(&case.prompt),
        confidence: last_confidence,
        error: last_error.filter(|_| status == BenchStatus::Error),
    })
}

/// Result of a tier run — useful for the CLI's summary print.
#[derive(Debug)]
pub struct TierRunSummary {
    pub run_id: String,
    pub results: Vec<BenchResult>,
    pub summary: BenchRun,
}

/// Runs all cases in a slice (assumed to be from one tier) sequentially.
/// Writes per-case results AND a final run summary to the store.
///
/// # Errors
///
/// Returns an error if the store can't be written to.
pub async fn run_t1(
    cases: &[BenchCase],
    base_worker_config: &WorkerConfig,
    store: &BenchStore,
    opts: &RunOptions,
) -> anyhow::Result<TierRunSummary> {
    let run_id = new_run_id();
    let started_at = Utc::now();
    let mut results = Vec::with_capacity(cases.len());
    let mut total_cost: u32 = 0;

    for case in cases {
        let r = run_t1_case(case, &run_id, base_worker_config, opts).await?;
        total_cost = total_cost.saturating_add(r.cost_cents);
        store.append_result(&r)?;
        results.push(r);
    }

    let total = u32::try_from(results.len()).unwrap_or(u32::MAX);
    let passed = u32::try_from(
        results
            .iter()
            .filter(|r| r.status == BenchStatus::Pass)
            .count(),
    )
    .unwrap_or(u32::MAX);
    let failed = u32::try_from(
        results
            .iter()
            .filter(|r| r.status == BenchStatus::Fail)
            .count(),
    )
    .unwrap_or(u32::MAX);
    let errored = u32::try_from(
        results
            .iter()
            .filter(|r| r.status == BenchStatus::Error)
            .count(),
    )
    .unwrap_or(u32::MAX);
    let skipped = u32::try_from(
        results
            .iter()
            .filter(|r| r.status == BenchStatus::Skipped)
            .count(),
    )
    .unwrap_or(u32::MAX);

    let summary = BenchRun {
        run_id: run_id.clone(),
        started_at,
        finished_at: Utc::now(),
        tier: Some(BenchTier::T1),
        total,
        passed,
        failed,
        errored,
        skipped,
        config_hash: prompt_hash(&base_worker_config.system_prompt),
        total_cost_cents: total_cost,
    };
    store.append_run(&summary)?;

    Ok(TierRunSummary {
        run_id,
        results,
        summary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── grade_attempt ──────────────────────────────────────────

    #[test]
    fn grade_exact_match_passes() {
        let exp = BenchExpected::Exact { text: "hi".into() };
        assert_eq!(grade_attempt("hi", &exp).unwrap(), AttemptOutcome::Pass);
        assert_eq!(
            grade_attempt(" hi \n", &exp).unwrap(),
            AttemptOutcome::Pass,
            "leading/trailing whitespace ignored"
        );
    }

    #[test]
    fn grade_exact_mismatch_fails() {
        let exp = BenchExpected::Exact { text: "hi".into() };
        assert_eq!(grade_attempt("hello", &exp).unwrap(), AttemptOutcome::Fail);
    }

    #[test]
    fn grade_regex_match_passes() {
        let exp = BenchExpected::Regex {
            pattern: r"^hi\b".into(),
        };
        assert_eq!(
            grade_attempt("hi there", &exp).unwrap(),
            AttemptOutcome::Pass
        );
    }

    #[test]
    fn grade_regex_no_match_fails() {
        let exp = BenchExpected::Regex {
            pattern: r"^hi$".into(),
        };
        assert_eq!(
            grade_attempt("hi there", &exp).unwrap(),
            AttemptOutcome::Fail
        );
    }

    #[test]
    fn grade_invalid_regex_errors() {
        let exp = BenchExpected::Regex {
            pattern: "[invalid".into(),
        };
        assert!(grade_attempt("anything", &exp).is_err());
    }

    #[test]
    fn grade_unsupported_kinds_return_unsupported() {
        let exp = BenchExpected::TestsPass {
            command: "true".into(),
        };
        assert_eq!(
            grade_attempt("anything", &exp).unwrap(),
            AttemptOutcome::Unsupported
        );
        let exp = BenchExpected::Rubric {
            criteria: vec!["x".into()],
        };
        assert_eq!(
            grade_attempt("anything", &exp).unwrap(),
            AttemptOutcome::Unsupported
        );
    }

    // ── prompt_hash ────────────────────────────────────────────

    #[test]
    fn prompt_hash_is_stable() {
        let a = prompt_hash("hello world");
        let b = prompt_hash("hello world");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64, "sha-256 hex is 64 chars");
    }

    #[test]
    fn prompt_hash_differs_on_change() {
        let a = prompt_hash("hello world");
        let b = prompt_hash("hello world!");
        assert_ne!(a, b);
    }

    // ── majority verdict logic (synthetic) ────────────────────

    /// Driver that exercises the same passes/fails → status logic as
    /// `run_t1_case` without spawning a worker. Mirrors the body
    /// rather than calling it.
    fn verdict(passes: u32, fails: u32) -> BenchStatus {
        let attempts = passes + fails;
        if attempts == 0 {
            BenchStatus::Error
        } else if passes >= T1_PASS_THRESHOLD {
            BenchStatus::Pass
        } else {
            BenchStatus::Fail
        }
    }

    #[test]
    fn majority_2_of_3_passes() {
        assert_eq!(verdict(2, 1), BenchStatus::Pass);
        assert_eq!(verdict(3, 0), BenchStatus::Pass);
    }

    #[test]
    fn minority_passes_fails() {
        assert_eq!(verdict(1, 2), BenchStatus::Fail);
        assert_eq!(verdict(0, 3), BenchStatus::Fail);
    }

    #[test]
    fn no_attempts_errors() {
        assert_eq!(verdict(0, 0), BenchStatus::Error);
    }

    // ── run_options ────────────────────────────────────────────

    #[test]
    fn run_options_default_enforces_budget() {
        let opts = RunOptions::default();
        assert!(!opts.no_budget, "default must enforce budget");
    }
}

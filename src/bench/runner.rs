//! T1 benchmark runner.
//!
//! Iterates cases, spawns a worker per case, compares the worker
//! response to the case's expected value, and writes per-attempt
//! results to the store. Each T1 case runs N=3 times and is graded
//! as Pass iff a majority of attempts match the expectation.
//! Determinism choice resolved with the user: 3-run majority is
//! cheap, robust to occasional model noise, and slows the benchmark
//! by ~3× without requiring provider-specific seed support.

use std::path::Path;
use std::time::{Duration, Instant};

use chrono::Utc;
use tracing::{info, warn};

use crate::bench::case::{BenchCase, BenchExpected, BenchTier, DEFAULT_TIMEOUT_S};
use crate::bench::store::{new_run_id, BenchResult, BenchRun, BenchStatus, BenchStore};
use crate::ipc::types::{ResultStatus, RuntimeMode, RuntimePlacementConfig};
use crate::routing::profile::builtin_tools;
use crate::worker::process::{SendOutcome, Worker, WorkerConfig};

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

const T1_SYSTEM_PROMPT_BASE: &str = "Answer concisely.";
const T1_SYSTEM_PROMPT_SOURCE: &str = "bench_t1_builtin";

/// Options for [`run_t1_case`].
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// If false, the harness aborts further attempts on a case once
    /// `max_cost_cents` is exceeded and actual provider cost is
    /// available. If true, all N attempts run regardless of cost.
    pub no_budget: bool,
    /// Overall benchmark timeout in seconds, overridden by
    /// `BenchCase::timeout_s`.
    pub timeout_s: Option<u32>,
    /// Overall worker iteration/tool-call budget, overridden by
    /// `BenchCase::max_iterations`.
    pub max_iterations: Option<u32>,
}

#[must_use]
pub fn effective_timeout_s(case: &BenchCase, opts: &RunOptions) -> u32 {
    case.timeout_s
        .or(opts.timeout_s)
        .unwrap_or(DEFAULT_TIMEOUT_S)
}

#[must_use]
pub fn effective_max_iterations(case: &BenchCase, opts: &RunOptions) -> Option<u32> {
    case.max_iterations.or(opts.max_iterations)
}

#[must_use]
pub fn nonzero_u32(value: u32) -> Option<u32> {
    (value > 0).then_some(value)
}

#[must_use]
pub fn nonzero_u64(value: u64) -> Option<u64> {
    (value > 0).then_some(value)
}

#[must_use]
pub fn timeout_bench_result(
    case: &BenchCase,
    run_id: &str,
    worker_model: &str,
    timeout_s: u32,
) -> BenchResult {
    BenchResult {
        case_id: case.id.clone(),
        run_id: run_id.into(),
        tier: case.tier,
        status: BenchStatus::Error,
        score: 0.0,
        latency_ms: u64::from(timeout_s).saturating_mul(1000),
        cost_cents: None,
        iterations: 0,
        prompt_tokens: None,
        completion_tokens: None,
        total_tokens: None,
        worker_model: worker_model.into(),
        prompt_hash: prompt_hash(&case.prompt),
        system_prompt_hash: None,
        system_prompt_source: None,
        confidence: None,
        output: None,
        error: Some(format!("benchmark case timed out after {timeout_s}s")),
    }
}

/// Metadata that describes how a benchmark run was configured.
/// Stored on [`BenchRun`] so comparisons can distinguish model,
/// prompt, and corpus changes.
#[derive(Debug, Clone, Default)]
pub struct BenchRunConfig {
    pub variant: Option<String>,
    pub model_selector: Option<String>,
    pub model_key: Option<String>,
    pub worker_model: Option<String>,
    pub grader_model: Option<String>,
    pub prompt_variant: Option<String>,
    pub cases_root: Option<String>,
    pub bench_config_path: Option<String>,
    pub orboros_commit: Option<String>,
    pub bench_commit: Option<String>,
    pub timeout_s: Option<u32>,
    pub max_iterations: Option<u32>,
}

impl BenchRunConfig {
    #[must_use]
    pub fn config_hash_input(&self, base_worker_config: &WorkerConfig) -> String {
        format!(
            "variant={:?}\nmodel_selector={:?}\nmodel_key={:?}\nworker_model={:?}\ngrader_model={:?}\nprompt_variant={:?}\ncases_root={:?}\nbench_config_path={:?}\norboros_commit={:?}\nbench_commit={:?}\ntimeout_s={:?}\nmax_iterations={:?}\nworker_command={}\nsystem_prompt={}",
            self.variant,
            self.model_selector,
            self.model_key,
            self.worker_model,
            self.grader_model,
            self.prompt_variant,
            self.cases_root,
            self.bench_config_path,
            self.orboros_commit,
            self.bench_commit,
            self.timeout_s,
            self.max_iterations,
            base_worker_config.command,
            base_worker_config.system_prompt,
        )
    }
}

/// SHA-256 of the prompt, hex-encoded. Used for `prompt_hash` on
/// every result so `bench compare` can detect prompt drift between
/// runs.
#[must_use]
pub fn prompt_hash(prompt: &str) -> String {
    crate::prompt::prompt_hash(prompt)
}

/// Runs one T1 case end-to-end: N attempts, in-process grader,
/// majority verdict. Returns the assembled [`BenchResult`].
///
/// # Errors
///
/// Returns an error if the worker can't be spawned at all. Per-attempt
/// failures are folded into the result rather than propagated.
#[allow(clippy::too_many_lines)]
pub async fn run_t1_case(
    case: &BenchCase,
    run_id: &str,
    base_worker_config: &WorkerConfig,
    opts: &RunOptions,
) -> anyhow::Result<BenchResult> {
    run_t1_case_with_artifacts(case, run_id, base_worker_config, opts, None).await
}

/// Runs a T1 case with optional per-attempt Heddle artifact placement.
///
/// Benchmark command execution supplies the case artifact directory, while
/// callers such as focused unit tests can omit it and retain the historical
/// worker behavior.
#[allow(clippy::too_many_lines)]
pub async fn run_t1_case_with_artifacts(
    case: &BenchCase,
    run_id: &str,
    base_worker_config: &WorkerConfig,
    opts: &RunOptions,
    artifact_dir: Option<&Path>,
) -> anyhow::Result<BenchResult> {
    if case.tier != BenchTier::T1 {
        anyhow::bail!("run_t1_case called on non-T1 case {}", case.id);
    }
    let started = Instant::now();
    let mut passes: u32 = 0;
    let mut fails: u32 = 0;
    let mut errors: u32 = 0;
    let mut accumulated_cost: Option<u32> = None;
    let mut last_confidence: Option<f32> = None;
    let mut total_iters: u32 = 0;
    let mut last_error: Option<String> = None;
    let mut output = String::new();
    let mut prompt_tokens: u64 = 0;
    let mut completion_tokens: u64 = 0;
    let mut total_tokens: u64 = 0;

    for attempt in 0..T1_ATTEMPTS {
        // Budget gate before spawning.
        if let Some(cost) = accumulated_cost {
            if !opts.no_budget && cost >= case.max_cost_cents {
                warn!(
                    case = %case.id,
                    attempt,
                    accumulated_cost = cost,
                    max = case.max_cost_cents,
                    "skipping remaining attempts: max_cost_cents exceeded"
                );
                break;
            }
        }

        let mut wc = base_worker_config.clone();
        wc.system_prompt = t1_system_prompt();
        wc.max_iterations = effective_max_iterations(case, opts).or(Some(1));
        wc.tools = builtin_tools("bench_t1")
            .iter()
            .map(ToString::to_string)
            .collect();
        wc.runtime = artifact_dir.map(|dir| benchmark_runtime_placement(dir, attempt));

        let mut worker = match Worker::spawn(&wc).await {
            Ok(w) => w,
            Err(e) => {
                let err = format!("spawn failed: {e}");
                warn!(
                    run_id,
                    case = %case.id,
                    attempt,
                    error = %err,
                    "T1 worker spawn failed"
                );
                append_attempt_output(&mut output, attempt, "spawn_error", &err);
                let fatal = is_fatal_worker_error_text(&err);
                last_error = Some(err);
                errors += 1;
                if fatal {
                    break;
                }
                continue;
            }
        };

        let send_id = format!("{}-{}", case.id, attempt);
        let outcome = match worker.send(&send_id, &case.prompt).await {
            Ok(o) => o,
            Err(e) => {
                let err = format!("send failed: {e}");
                warn!(
                    run_id,
                    case = %case.id,
                    attempt,
                    error = %err,
                    "T1 worker send failed"
                );
                append_attempt_output(&mut output, attempt, "send_error", &err);
                let fatal = is_fatal_worker_error_text(&err);
                last_error = Some(err);
                errors += 1;
                let _ = worker.shutdown().await;
                if fatal {
                    break;
                }
                continue;
            }
        };
        let _ = worker.shutdown().await;
        add_usage(
            &mut prompt_tokens,
            &mut completion_tokens,
            &mut total_tokens,
            &mut accumulated_cost,
            &outcome,
        );

        if outcome.status != ResultStatus::Ok {
            let err = send_outcome_error(&outcome);
            warn!(
                run_id,
                case = %case.id,
                attempt,
                status = ?outcome.status,
                error = %err,
                "T1 worker returned error status"
            );
            append_attempt_output(&mut output, attempt, "worker_error", &err);
            last_error = Some(err);
            errors += 1;
            if outcome.confidence.is_some() {
                last_confidence = outcome.confidence;
            }
            total_iters = total_iters.saturating_add(outcome.iterations);
            continue;
        }

        let response = outcome.response.clone().unwrap_or_default();
        let attempt_outcome = match grade_attempt(&response, &case.expected) {
            Ok(o) => o,
            Err(e) => {
                let err = format!("grade failed: {e}");
                warn!(
                    run_id,
                    case = %case.id,
                    attempt,
                    error = %err,
                    "T1 grading failed"
                );
                last_error = Some(err.clone());
                append_attempt_output(&mut output, attempt, "grade_error", &err);
                AttemptOutcome::Fail
            }
        };
        append_attempt_output(
            &mut output,
            attempt,
            match attempt_outcome {
                AttemptOutcome::Pass => "pass",
                AttemptOutcome::Fail => "fail",
                AttemptOutcome::Unsupported => "unsupported",
            },
            &response,
        );

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

    let attempts_run = passes + fails + errors;
    let status = if attempts_run == 0 {
        BenchStatus::Error
    } else if passes >= T1_PASS_THRESHOLD {
        BenchStatus::Pass
    } else if errors > 0 {
        BenchStatus::Error
    } else {
        BenchStatus::Fail
    };
    #[allow(clippy::cast_possible_truncation)]
    let score = if attempts_run == 0 {
        0.0
    } else {
        f64::from(passes) as f32 / f64::from(attempts_run) as f32
    };
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    if status == BenchStatus::Error {
        warn!(
            run_id,
            case = %case.id,
            error = %last_error.as_deref().unwrap_or("unknown error"),
            "T1 case errored"
        );
    }

    info!(
        case = %case.id,
        status = ?status,
        passes,
        fails,
        errors,
        elapsed_ms,
        "T1 case complete",
    );

    Ok(BenchResult {
        case_id: case.id.clone(),
        run_id: run_id.into(),
        tier: BenchTier::T1,
        status,
        score,
        latency_ms: elapsed_ms,
        cost_cents: accumulated_cost,
        iterations: total_iters,
        prompt_tokens: nonzero_u64(prompt_tokens),
        completion_tokens: nonzero_u64(completion_tokens),
        total_tokens: nonzero_u64(total_tokens),
        worker_model: base_worker_config.model.clone(),
        prompt_hash: prompt_hash(&case.prompt),
        system_prompt_hash: Some(prompt_hash(&t1_system_prompt())),
        system_prompt_source: Some(T1_SYSTEM_PROMPT_SOURCE.into()),
        confidence: last_confidence,
        output: (!output.is_empty()).then_some(output),
        error: last_error.filter(|_| status == BenchStatus::Error),
    })
}

fn benchmark_runtime_placement(artifact_dir: &Path, attempt: u32) -> RuntimePlacementConfig {
    let artifact_dir = absolute_artifact_dir(artifact_dir);
    let worker_dir = artifact_dir
        .join("heddle")
        .join(format!("attempt-{attempt}"));
    RuntimePlacementConfig {
        mode: Some(RuntimeMode::Isolated),
        state_root: Some(worker_dir.join("state").to_string_lossy().into_owned()),
        transcript_path: Some(
            worker_dir
                .join("transcript.jsonl")
                .to_string_lossy()
                .into_owned(),
        ),
        inherit_ambient_config: Some(false),
    }
}

fn absolute_artifact_dir(path: &Path) -> std::path::PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn add_usage(
    prompt_tokens: &mut u64,
    completion_tokens: &mut u64,
    total_tokens: &mut u64,
    cost_cents: &mut Option<u32>,
    outcome: &SendOutcome,
) {
    if let Some(usage) = &outcome.usage {
        *prompt_tokens = prompt_tokens.saturating_add(usage.prompt_tokens);
        *completion_tokens = completion_tokens.saturating_add(usage.completion_tokens);
        *total_tokens = total_tokens.saturating_add(usage.total_tokens);
        if let Some(cents) = usage.cost_micros.map(cost_micros_to_cents_ceil) {
            *cost_cents = Some(cost_cents.unwrap_or(0).saturating_add(cents));
        }
    }
}

fn cost_micros_to_cents_ceil(cost_micros: u64) -> u32 {
    if cost_micros == 0 {
        return 0;
    }
    let cents = cost_micros.saturating_add(9_999) / 10_000;
    crate::ipc::types::u64_to_u32_saturating(cents)
}

fn send_outcome_error(outcome: &SendOutcome) -> String {
    outcome.error.as_ref().map_or_else(
        || {
            outcome
                .response
                .clone()
                .unwrap_or_else(|| format!("worker returned status {:?}", outcome.status))
        },
        |e| e.message.clone(),
    )
}

fn append_attempt_output(out: &mut String, attempt: u32, label: &str, body: &str) {
    use std::fmt::Write as _;

    let _ = writeln!(out, "== attempt {attempt} {label} ==");
    out.push_str(body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
}

fn t1_system_prompt() -> String {
    format!(
        "{T1_SYSTEM_PROMPT_BASE}{}",
        crate::worker::process::CONFIDENCE_PROMPT_ADDENDUM
    )
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
    run_config: &BenchRunConfig,
) -> anyhow::Result<TierRunSummary> {
    let run_id = new_run_id();
    let started_at = Utc::now();
    let mut results = Vec::with_capacity(cases.len());
    let mut total_cost: Option<u32> = None;

    for case in cases {
        let timeout_s = effective_timeout_s(case, opts);
        let r = match tokio::time::timeout(
            Duration::from_secs(u64::from(timeout_s)),
            run_t1_case_with_artifacts(
                case,
                &run_id,
                base_worker_config,
                opts,
                Some(&store.case_artifact_dir(&run_id, &case.id)),
            ),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => timeout_bench_result(case, &run_id, &base_worker_config.model, timeout_s),
        };
        if let Some(cost) = r.cost_cents {
            total_cost = Some(total_cost.unwrap_or(0).saturating_add(cost));
        }
        store.append_result(&r)?;
        let fatal = is_fatal_worker_error(&r);
        results.push(r);
        if fatal {
            warn!(
                case = %case.id,
                "stopping T1 run after fatal worker/provider error"
            );
            break;
        }
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
        tiers: vec![BenchTier::T1],
        variant: run_config.variant.clone(),
        model_selector: run_config.model_selector.clone(),
        model_key: run_config.model_key.clone(),
        worker_model: run_config
            .worker_model
            .clone()
            .or_else(|| Some(base_worker_config.model.clone())),
        grader_model: run_config.grader_model.clone(),
        prompt_variant: run_config.prompt_variant.clone(),
        cases_root: run_config.cases_root.clone(),
        bench_config_path: run_config.bench_config_path.clone(),
        orboros_commit: run_config.orboros_commit.clone(),
        bench_commit: run_config.bench_commit.clone(),
        total,
        passed,
        failed,
        errored,
        skipped,
        config_hash: prompt_hash(&run_config.config_hash_input(base_worker_config)),
        total_cost_cents: total_cost,
        prompt_tokens: sum_result_tokens(&results, |r| r.prompt_tokens),
        completion_tokens: sum_result_tokens(&results, |r| r.completion_tokens),
        total_tokens: sum_result_tokens(&results, |r| r.total_tokens),
    };
    store.append_run(&summary)?;

    Ok(TierRunSummary {
        run_id,
        results,
        summary,
    })
}

fn sum_result_tokens(
    results: &[BenchResult],
    field: impl Fn(&BenchResult) -> Option<u64>,
) -> Option<u64> {
    nonzero_u64(
        results
            .iter()
            .filter_map(field)
            .fold(0u64, u64::saturating_add),
    )
}

#[must_use]
pub fn is_fatal_worker_error(result: &BenchResult) -> bool {
    if result.status != BenchStatus::Error {
        return false;
    }
    let text = result
        .error
        .as_deref()
        .or(result.output.as_deref())
        .unwrap_or("");
    is_fatal_worker_error_text(text)
}

fn is_fatal_worker_error_text(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("not a valid model id")
        || text.contains("missing credentials")
        || text.contains("api key")
        || text.contains("unauthorized")
        || text.contains("protocol version mismatch")
        || text.contains("protocol_version_mismatch")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bench_result(case_id: &str, status: BenchStatus) -> BenchResult {
        BenchResult {
            case_id: case_id.into(),
            run_id: "run-x".into(),
            tier: BenchTier::T1,
            status,
            score: 0.0,
            latency_ms: 0,
            cost_cents: None,
            iterations: 0,
            prompt_tokens: None,
            completion_tokens: None,
            total_tokens: None,
            worker_model: "mock/model".into(),
            prompt_hash: "hash".into(),
            system_prompt_hash: None,
            system_prompt_source: None,
            confidence: None,
            output: None,
            error: None,
        }
    }

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

    #[test]
    fn fatal_worker_error_detects_provider_level_failures() {
        let mut result = bench_result("case", BenchStatus::Error);
        result.error = Some("openrouter/foo is not a valid model ID".into());
        assert!(is_fatal_worker_error(&result));

        result.error =
            Some("spawn failed: protocol version mismatch: expected 0.4.0, got 1.0.0".into());
        assert!(is_fatal_worker_error(&result));

        result.error = Some("assertion failed in grader".into());
        assert!(!is_fatal_worker_error(&result));

        result.status = BenchStatus::Fail;
        result.error = Some("openrouter/foo is not a valid model ID".into());
        assert!(!is_fatal_worker_error(&result));
    }

    // ── run_options ────────────────────────────────────────────

    #[test]
    fn run_options_default_enforces_budget() {
        let opts = RunOptions::default();
        assert!(!opts.no_budget, "default must enforce budget");
    }
}

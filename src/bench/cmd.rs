//! CLI command handlers for `orboros bench`.
//!
//! Each handler takes plain arguments and a store/corpus root —
//! main.rs is the only place that talks to clap. Print-and-return
//! style mirrors the rest of the CLI surface in `orb_cmd` and
//! `hooks::cmd`.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, NaiveDate, Utc};
use sha2::{Digest, Sha256};

use crate::bench::case::{load_all, load_tier, BenchCase, BenchTier, DEFAULT_TIMEOUT_S};
use crate::bench::prompts::BenchPromptSet;
use crate::bench::runner::{
    effective_timeout_s, is_fatal_worker_error, run_t1_case_with_artifacts, run_t1_with_run_id,
    timeout_bench_result, BenchRunConfig, RunOptions,
};
use crate::bench::store::{
    BenchDispatchRecord, BenchPromptRecord, BenchResult, BenchRun, BenchStatus, BenchStore,
    BenchSuiteCase, BenchSuiteManifest,
};
use crate::worker::process::WorkerConfig;

pub struct BenchRunRequest<'a> {
    pub bench_root: &'a Path,
    pub store: &'a BenchStore,
    pub tier: Option<BenchTier>,
    pub case_id: Option<&'a str>,
    pub worker_config: &'a WorkerConfig,
    pub no_budget: bool,
    /// Maximum independent benchmark cases in flight. One preserves the
    /// historical serial behavior.
    pub jobs: usize,
    pub timeout_s: Option<u32>,
    pub max_iterations: Option<u32>,
    pub run_config: &'a BenchRunConfig,
    pub prompt_set: Option<&'a BenchPromptSet>,
}

struct CaseCompletion {
    index: usize,
    artifact_dir: PathBuf,
    result: BenchResult,
}

/// Prints every case in the corpus, grouped by tier.
///
/// # Errors
///
/// Returns an error if loading the corpus fails (malformed TOML, etc.).
pub fn cmd_bench_list(bench_root: &Path) -> anyhow::Result<()> {
    let cases = load_all(bench_root).context("failed to load benchmark corpus")?;
    if cases.is_empty() {
        println!("No benchmark cases found under {}", bench_root.display());
        return Ok(());
    }
    let id_width = cases
        .iter()
        .map(|case| case.id.len())
        .max()
        .unwrap_or(24)
        .max(24);
    let mut tier = None;
    for case in &cases {
        if tier != Some(case.tier) {
            tier = Some(case.tier);
            println!("\n== {} ==", case.tier);
        }
        let cost = case.max_cost_cents;
        let timeout = case.timeout_s.unwrap_or(DEFAULT_TIMEOUT_S);
        println!(
            "  {selector:<id_width$} {name}  ({id}; max ${cost_dollars:.2}, {timeout}s)",
            selector = case.selector,
            id = case.id,
            name = case.name,
            cost_dollars = f64::from(cost) / 100.0,
            id_width = id_width,
        );
    }
    println!("\n{} case(s)", cases.len());
    Ok(())
}

/// Runs every case of the given tier (or all tiers when `tier` is
/// `None`). Writes per-case results AND the run summary to the
/// store, then prints a short outcome table.
///
/// Only T1 actually executes today; T2/T3 use the scaffolded stubs
/// that record an Error result. Composability of mixed-tier runs
/// is preserved.
///
/// # Errors
///
/// Returns an error if loading the corpus or writing results fails.
#[allow(clippy::too_many_lines)]
pub async fn cmd_bench_run(req: BenchRunRequest<'_>) -> anyhow::Result<()> {
    let mut cases = match req.tier {
        Some(t) => load_tier(req.bench_root, t)?,
        None => load_all(req.bench_root)?,
    };
    if let Some(id) = req.case_id {
        cases.retain(|c| c.id == id || c.selector == id);
        if cases.is_empty() {
            anyhow::bail!("no case found with id or selector `{id}`");
        }
    }
    if cases.is_empty() {
        println!("No matching cases.");
        return Ok(());
    }
    let case_labels: HashMap<String, (String, String)> = cases
        .iter()
        .map(|case| (case.id.clone(), (case.selector.clone(), case.name.clone())))
        .collect();
    let resource_guidance: HashMap<String, crate::bench::case::BenchResourceGuidance> = cases
        .iter()
        .filter_map(|case| {
            case.resource_guidance
                .clone()
                .map(|guidance| (case.id.clone(), guidance))
        })
        .collect();
    let mut run_config = req.run_config.clone();
    run_config.suite_manifest = Some(build_suite_manifest(&cases, req.prompt_set)?);

    if req.jobs == 0 {
        anyhow::bail!("--jobs must be at least 1");
    }
    if req.jobs > 1 {
        return cmd_bench_run_parallel(req, cases, case_labels, resource_guidance, run_config)
            .await;
    }

    // Split by tier and dispatch. Today only T1 actually runs the
    // pipeline; T2/T3 fall through to scaffolded error rows.
    let (t1, other): (Vec<BenchCase>, Vec<BenchCase>) =
        cases.into_iter().partition(|c| c.tier == BenchTier::T1);

    let opts = RunOptions {
        no_budget: req.no_budget,
        timeout_s: req.timeout_s,
        max_iterations: req.max_iterations,
    };
    let mut all_results = Vec::new();
    let run_id = crate::bench::store::new_run_id();
    let run_started_at = Utc::now();
    crate::bench::log::start(&req.store.run_dir(&run_id).join("cli.log"))?;
    tracing::info!(run_id = %run_id, "benchmark run logging started");
    let mut summary_run_id = Some(run_id.clone());

    if !t1.is_empty() {
        let summary = run_t1_with_run_id(
            &t1,
            req.worker_config,
            req.store,
            &opts,
            &run_config,
            run_id.clone(),
        )
        .await?;
        summary_run_id = Some(summary.run_id);
        all_results.extend(summary.results);
        if all_results.iter().any(is_fatal_worker_error) {
            eprintln!("stopping benchmark run after fatal worker/provider error");
            print_result_table(&all_results, Some(&case_labels), Some(&resource_guidance));
            if let Some(ref id) = summary_run_id {
                if let Some(prompt_set) = req.prompt_set {
                    prompt_set.copy_to_run(&req.store.run_dir(id))?;
                }
                println!("\nRun id: {id}");
            }
            return Ok(());
        }
    }

    for case in &other {
        let run_id = summary_run_id
            .clone()
            .expect("benchmark run ID initialized");
        let timeout_s = effective_timeout_s(case, &opts);
        let artifact_dir = req.store.case_artifact_dir(&run_id, &case.id);
        let result = match case.tier {
            BenchTier::T2 => match tokio::time::timeout(
                Duration::from_secs(u64::from(timeout_s)),
                crate::bench::runner_t2t3::run_t2_case(
                    case,
                    &run_id,
                    req.worker_config,
                    &opts,
                    Some(&artifact_dir),
                    req.prompt_set,
                ),
            )
            .await
            {
                Ok(result) => result.map_or_else(
                    |e| {
                        Ok::<_, anyhow::Error>(crate::bench::store::BenchResult {
                            case_id: case.id.clone(),
                            run_id: run_id.clone(),
                            tier: BenchTier::T2,
                            status: BenchStatus::Error,
                            score: 0.0,
                            process_score: None,
                            process_annotations: Vec::new(),
                            resource_guidance: case.resource_guidance.clone(),
                            latency_ms: 0,
                            model_latency_ms: None,
                            tool_latency_ms: None,
                            total_latency_ms: None,
                            cost_cents: None,
                            cost_micros: None,
                            iterations: 0,
                            assistant_turns: None,
                            tool_calls: None,
                            prompt_tokens: None,
                            completion_tokens: None,
                            total_tokens: None,
                            cache_read_tokens: None,
                            cache_write_tokens: None,
                            worker_model: String::new(),
                            prompt_hash: crate::bench::runner::prompt_hash(&case.prompt),
                            system_prompt_hash: None,
                            system_prompt_source: None,
                            confidence: None,
                            output: None,
                            error: Some(e.to_string()),
                        })
                    },
                    Ok,
                )?,
                Err(_) => timeout_bench_result(case, &run_id, &req.worker_config.model, timeout_s),
            },
            BenchTier::T3 => {
                match tokio::time::timeout(Duration::from_secs(u64::from(timeout_s)), async {
                    crate::bench::runner_t2t3::run_t3_case_stub(case, &run_id, &opts)
                })
                .await
                {
                    Ok(result) => result.map_or_else(
                        |e| {
                            Ok::<_, anyhow::Error>(crate::bench::store::BenchResult {
                                case_id: case.id.clone(),
                                run_id: run_id.clone(),
                                tier: BenchTier::T3,
                                status: BenchStatus::Error,
                                score: 0.0,
                                process_score: None,
                                process_annotations: Vec::new(),
                                resource_guidance: case.resource_guidance.clone(),
                                latency_ms: 0,
                                model_latency_ms: None,
                                tool_latency_ms: None,
                                total_latency_ms: None,
                                cost_cents: None,
                                cost_micros: None,
                                iterations: 0,
                                assistant_turns: None,
                                tool_calls: None,
                                prompt_tokens: None,
                                completion_tokens: None,
                                total_tokens: None,
                                cache_read_tokens: None,
                                cache_write_tokens: None,
                                worker_model: String::new(),
                                prompt_hash: crate::bench::runner::prompt_hash(&case.prompt),
                                system_prompt_hash: None,
                                system_prompt_source: None,
                                confidence: None,
                                output: None,
                                error: Some(e.to_string()),
                            })
                        },
                        Ok,
                    )?,
                    Err(_) => {
                        timeout_bench_result(case, &run_id, &req.worker_config.model, timeout_s)
                    }
                }
            }
            BenchTier::T1 => unreachable!("T1 partitioned out above"),
        };
        if summary_run_id.is_none() {
            summary_run_id = Some(run_id.clone());
        }
        if result.status == BenchStatus::Error {
            tracing::warn!(
                run_id = %result.run_id,
                case = %result.case_id,
                tier = ?result.tier,
                error = %result.error.as_deref().unwrap_or("unknown error"),
                "benchmark case errored"
            );
        }
        let ledger = crate::execution::ExecutionStore::new(
            artifact_dir
                .join("workdir")
                .join(".orbs")
                .join("executions.jsonl"),
        );
        req.store
            .append_dispatches(&run_id, &case.id, &ledger.read_all()?)?;
        let prompt_ledger = crate::execution::PromptStore::new(
            artifact_dir
                .join("workdir")
                .join(".orbs")
                .join("prompts.jsonl"),
        );
        req.store
            .append_prompts(&run_id, &case.id, &prompt_ledger.read_all()?)?;
        req.store.retain_orb_state(
            &run_id,
            &case.id,
            &artifact_dir.join("workdir").join(".orbs"),
        )?;
        req.store.append_result(&result)?;
        let fatal = is_fatal_worker_error(&result);
        all_results.push(result);
        if fatal {
            eprintln!("stopping benchmark run after fatal worker/provider error");
            break;
        }
    }

    let mut completed_run = None;
    if let Some(ref id) = summary_run_id {
        if let Some(prompt_set) = req.prompt_set {
            prompt_set.copy_to_run(&req.store.run_dir(id))?;
        }
        let run = summarize_run(
            id,
            run_started_at,
            common_tier(&all_results),
            &all_results,
            &run_config,
            req.worker_config,
        );
        req.store.append_run(&run)?;
        completed_run = Some(run.clone());
    }

    if let Some(run) = completed_run.as_ref() {
        print_completed_run(run, &all_results, &case_labels, &resource_guidance);
    } else {
        print_result_table(&all_results, Some(&case_labels), Some(&resource_guidance));
    }
    if let Some(ref id) = summary_run_id {
        println!("\nRun id: {id}");
    }
    Ok(())
}

async fn cmd_bench_run_parallel(
    req: BenchRunRequest<'_>,
    mut cases: Vec<BenchCase>,
    case_labels: HashMap<String, (String, String)>,
    resource_guidance: HashMap<String, crate::bench::case::BenchResourceGuidance>,
    run_config: BenchRunConfig,
) -> anyhow::Result<()> {
    use tokio::task::JoinSet;

    cases.sort_by(|left, right| left.selector.cmp(&right.selector));
    let opts = RunOptions {
        no_budget: req.no_budget,
        timeout_s: req.timeout_s,
        max_iterations: req.max_iterations,
    };
    let run_id = crate::bench::store::new_run_id();
    let run_started_at = Utc::now();
    crate::bench::log::start(&req.store.run_dir(&run_id).join("cli.log"))?;
    println!(
        "Running {} benchmark case(s) with {} case jobs. Nested T2 orb dispatches may use additional workers.",
        cases.len(),
        req.jobs
    );
    tracing::info!(run_id = %run_id, jobs = req.jobs, "parallel benchmark run logging started");

    let mut pending = cases.into_iter().enumerate().collect::<VecDeque<_>>();
    let mut in_flight = JoinSet::new();
    let mut completions = Vec::new();
    let mut stop_scheduling = false;

    loop {
        while !stop_scheduling && in_flight.len() < req.jobs {
            let Some((index, case)) = pending.pop_front() else {
                break;
            };
            let artifact_dir = req.store.case_artifact_dir(&run_id, &case.id);
            in_flight.spawn(run_case(
                index,
                case,
                run_id.clone(),
                req.worker_config.clone(),
                opts.clone(),
                artifact_dir,
                req.prompt_set.cloned(),
            ));
        }

        let Some(joined) = in_flight.join_next().await else {
            break;
        };
        let completion =
            joined.map_err(|error| anyhow::anyhow!("benchmark case task failed: {error}"))?;
        if is_fatal_worker_error(&completion.result) {
            stop_scheduling = true;
            eprintln!(
                "fatal worker/provider error: no further benchmark cases will be scheduled; waiting for {} in-flight case(s)",
                in_flight.len()
            );
        }
        completions.push(completion);
    }

    completions.sort_by_key(|completion| completion.index);
    let mut all_results = Vec::with_capacity(completions.len());
    for completion in completions {
        persist_case_evidence(req.store, &run_id, &completion)?;
        if completion.result.status == BenchStatus::Error {
            tracing::warn!(
                run_id = %completion.result.run_id,
                case = %completion.result.case_id,
                tier = ?completion.result.tier,
                error = %completion.result.error.as_deref().unwrap_or("unknown error"),
                "benchmark case errored"
            );
        }
        req.store.append_result(&completion.result)?;
        all_results.push(completion.result);
    }

    if let Some(prompt_set) = req.prompt_set {
        prompt_set.copy_to_run(&req.store.run_dir(&run_id))?;
    }
    let run = summarize_run(
        &run_id,
        run_started_at,
        common_tier(&all_results),
        &all_results,
        &run_config,
        req.worker_config,
    );
    req.store.append_run(&run)?;
    print_completed_run(&run, &all_results, &case_labels, &resource_guidance);
    print_parallel_timing(&run, &all_results, req.jobs);
    println!("\nRun id: {run_id}");
    Ok(())
}

fn persist_case_evidence(
    store: &BenchStore,
    run_id: &str,
    completion: &CaseCompletion,
) -> anyhow::Result<()> {
    let ledger = crate::execution::ExecutionStore::new(
        completion
            .artifact_dir
            .join("workdir")
            .join(".orbs")
            .join("executions.jsonl"),
    );
    store.append_dispatches(run_id, &completion.result.case_id, &ledger.read_all()?)?;
    let prompt_ledger = crate::execution::PromptStore::new(
        completion
            .artifact_dir
            .join("workdir")
            .join(".orbs")
            .join("prompts.jsonl"),
    );
    store.append_prompts(
        run_id,
        &completion.result.case_id,
        &prompt_ledger.read_all()?,
    )?;
    store.retain_orb_state(
        run_id,
        &completion.result.case_id,
        &completion.artifact_dir.join("workdir").join(".orbs"),
    )?;
    Ok(())
}

async fn run_case(
    index: usize,
    case: BenchCase,
    run_id: String,
    worker_config: WorkerConfig,
    opts: RunOptions,
    artifact_dir: PathBuf,
    prompt_set: Option<BenchPromptSet>,
) -> CaseCompletion {
    let timeout_s = effective_timeout_s(&case, &opts);
    let result = match case.tier {
        BenchTier::T1 => match tokio::time::timeout(
            Duration::from_secs(u64::from(timeout_s)),
            run_t1_case_with_artifacts(&case, &run_id, &worker_config, &opts, Some(&artifact_dir)),
        )
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => error_bench_result(&case, &run_id, error),
            Err(_) => timeout_bench_result(&case, &run_id, &worker_config.model, timeout_s),
        },
        BenchTier::T2 => match tokio::time::timeout(
            Duration::from_secs(u64::from(timeout_s)),
            crate::bench::runner_t2t3::run_t2_case(
                &case,
                &run_id,
                &worker_config,
                &opts,
                Some(&artifact_dir),
                prompt_set.as_ref(),
            ),
        )
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => error_bench_result(&case, &run_id, error),
            Err(_) => timeout_bench_result(&case, &run_id, &worker_config.model, timeout_s),
        },
        BenchTier::T3 => {
            match tokio::time::timeout(Duration::from_secs(u64::from(timeout_s)), async {
                crate::bench::runner_t2t3::run_t3_case_stub(&case, &run_id, &opts)
            })
            .await
            {
                Ok(Ok(result)) => result,
                Ok(Err(error)) => error_bench_result(&case, &run_id, error),
                Err(_) => timeout_bench_result(&case, &run_id, &worker_config.model, timeout_s),
            }
        }
    };
    CaseCompletion {
        index,
        artifact_dir,
        result,
    }
}

fn error_bench_result(
    case: &BenchCase,
    run_id: &str,
    error: impl std::fmt::Display,
) -> BenchResult {
    BenchResult {
        case_id: case.id.clone(),
        run_id: run_id.into(),
        tier: case.tier,
        status: BenchStatus::Error,
        score: 0.0,
        process_score: None,
        process_annotations: Vec::new(),
        resource_guidance: case.resource_guidance.clone(),
        latency_ms: 0,
        model_latency_ms: None,
        tool_latency_ms: None,
        total_latency_ms: None,
        cost_cents: None,
        cost_micros: None,
        iterations: 0,
        assistant_turns: None,
        tool_calls: None,
        prompt_tokens: None,
        completion_tokens: None,
        total_tokens: None,
        cache_read_tokens: None,
        cache_write_tokens: None,
        worker_model: String::new(),
        prompt_hash: crate::bench::runner::prompt_hash(&case.prompt),
        system_prompt_hash: None,
        system_prompt_source: None,
        confidence: None,
        output: None,
        error: Some(error.to_string()),
    }
}

fn build_suite_manifest(
    cases: &[BenchCase],
    prompt_set: Option<&BenchPromptSet>,
) -> anyhow::Result<BenchSuiteManifest> {
    let mut suite_cases = Vec::with_capacity(cases.len());
    for case in cases {
        suite_cases.push(BenchSuiteCase {
            id: case.id.clone(),
            selector: case.selector.clone(),
            case_toml_sha256: hash_file(&case.case_dir.join("case.toml"))?,
            fixture_sha256: case.fixture_dir.as_deref().map(hash_tree).transpose()?,
            overlay_sha256: case
                .test_overlay_dir
                .as_deref()
                .map(hash_tree)
                .transpose()?,
        });
    }
    suite_cases.sort_by(|a, b| a.selector.cmp(&b.selector));
    let prompt_manifest_sha256 = prompt_set
        .map(BenchPromptSet::manifest)
        .map(|manifest| serde_json::to_vec(&manifest))
        .transpose()?
        .map(|bytes| sha256(&bytes));
    let fingerprint = sha256(&serde_json::to_vec(&(
        suite_cases.clone(),
        &prompt_manifest_sha256,
    ))?);
    Ok(BenchSuiteManifest {
        fingerprint,
        cases: suite_cases,
        prompt_manifest_sha256,
    })
}

fn hash_file(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read suite input {}", path.display()))?;
    Ok(sha256(&bytes))
}

fn hash_tree(root: &Path) -> anyhow::Result<String> {
    let mut files = Vec::new();
    collect_files(root, root, &mut files)?;
    files.sort();
    let mut hasher = Sha256::new();
    for relative in files {
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(std::fs::read(root.join(&relative)).with_context(|| {
            format!(
                "failed to read suite input {}",
                root.join(&relative).display()
            )
        })?);
        hasher.update([0]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn collect_files(root: &Path, dir: &Path, files: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("failed to read suite input directory {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, files)?;
        } else if path.is_file() {
            files.push(path.strip_prefix(root)?.to_path_buf());
        }
    }
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Prints a saved run's per-case results.
///
/// # Errors
///
/// Returns an error if the store can't be read.
pub fn cmd_bench_show(store: &BenchStore, run_id: &str) -> anyhow::Result<()> {
    let results = store.read_results(run_id)?;
    if results.is_empty() {
        anyhow::bail!("no results found for run `{run_id}`");
    }
    if let Some(run) = store.read_runs()?.into_iter().find(|r| r.run_id == run_id) {
        let case_labels = case_labels_for_run(&run);
        let resource_guidance = resource_guidance_for_run(&run);
        print_completed_run(&run, &results, &case_labels, &resource_guidance);
    } else {
        print_result_table(&results, None, None);
    }
    let dispatches = store.read_dispatches(run_id)?;
    for result in results
        .iter()
        .filter(|result| result.status == BenchStatus::Error)
    {
        print_failure_attribution(&dispatches, &result.case_id);
    }
    Ok(())
}

/// Prints saved error/output details for a run. Defaults to non-pass
/// cases so failed benchmark runs are inspectable without scrolling
/// through every passing row.
///
/// # Errors
///
/// Returns an error if the store can't be read or no matching rows exist.
pub fn cmd_bench_details(
    store: &BenchStore,
    run_id: &str,
    case_id: Option<&str>,
    include_passes: bool,
) -> anyhow::Result<()> {
    let results = store.read_results(run_id)?;
    if results.is_empty() {
        anyhow::bail!("no results found for run `{run_id}`");
    }

    if let Some(run) = store.read_runs()?.into_iter().find(|r| r.run_id == run_id) {
        println!("== summary ==");
        print_run_summary(&run);
    }
    let dispatches = store.read_dispatches(run_id)?;

    let mut printed = 0usize;
    for result in &results {
        if let Some(case_id) = case_id {
            if result.case_id != case_id {
                continue;
            }
        }
        if !include_passes && result.status == BenchStatus::Pass {
            continue;
        }
        print_result_details(result);
        print_failure_attribution(&dispatches, &result.case_id);
        printed += 1;
    }

    if printed == 0 {
        match case_id {
            Some(case_id) => {
                anyhow::bail!("no matching details for case `{case_id}` in `{run_id}`")
            }
            None if include_passes => anyhow::bail!("no result rows found for `{run_id}`"),
            None => println!("No failed/error cases."),
        }
    }
    Ok(())
}

fn print_failure_attribution(records: &[BenchDispatchRecord], case_id: &str) {
    let failed: Vec<_> = records
        .iter()
        .filter(|record| {
            record.case_id == case_id
                && record
                    .execution
                    .attempts
                    .iter()
                    .any(|attempt| attempt.status != "done")
        })
        .collect();
    if failed.is_empty() {
        return;
    }
    println!("\n-- worker attempts --");
    for record in failed {
        for (index, attempt) in record.execution.attempts.iter().enumerate() {
            let transcript = attempt
                .runtime
                .as_ref()
                .map_or("-", |runtime| runtime.transcript_path.as_str());
            let failure = attempt
                .failure
                .as_ref()
                .map(|failure| failure.code.as_str())
                .or(attempt.error.as_deref())
                .unwrap_or("-");
            println!(
                "{} attempt={} status={} worker={} session={} transcript={} error={}",
                record.execution.dispatch_kind,
                index + 1,
                attempt.status,
                attempt.worker_id.as_deref().unwrap_or("-"),
                attempt.session_id.as_deref().unwrap_or("-"),
                transcript,
                failure,
            );
        }
    }
}

/// Prints persisted dispatch telemetry grouped by case and dispatch kind.
///
/// # Errors
///
/// Returns an error when the run is unknown or its dispatch ledger cannot be
/// read.
pub fn cmd_bench_report(
    store: &BenchStore,
    run_id: &str,
    case_id: Option<&str>,
) -> anyhow::Result<()> {
    let run = store
        .read_runs()?
        .into_iter()
        .find(|run| run.run_id == run_id)
        .context("no saved benchmark run with that id")?;
    let mut records = store.read_dispatches(run_id)?;
    if let Some(case_id) = case_id {
        records.retain(|record| record.case_id == case_id);
    }
    if records.is_empty() {
        if case_id.is_some() {
            anyhow::bail!("no persisted dispatch telemetry for that case in `{run_id}`");
        }
        anyhow::bail!("run `{run_id}` has no persisted dispatch telemetry");
    }

    println!("== dispatch report ==");
    println!(
        "run={run_id}  model={model}  dispatches={}",
        records.len(),
        model = run.worker_model.as_deref().unwrap_or("-")
    );
    let case_labels = case_labels_for_run(&run);
    let summaries = summarize_dispatches(&records);
    let mut summary_rows: Vec<_> = summaries.into_iter().collect();
    summary_rows.sort_by(|((case_a, kind_a), _), ((case_b, kind_b), _)| {
        display_case_label(&case_labels, case_a)
            .0
            .cmp(&display_case_label(&case_labels, case_b).0)
            .then_with(|| kind_a.cmp(kind_b))
    });
    let (selector_width, name_width) = report_case_widths(
        summary_rows
            .iter()
            .map(|((case_id, _), _)| case_id.as_str()),
        &case_labels,
    );
    println!(
        "{selector:<selector_width$}  {name:<name_width$}  {kind:<18} {done:>4} {failed:>6} {error:>5} {retry:>5} {turns:>6} {tools:>6} {input:>9} {output:>8} {cache:>8} {cost:>10} {wall:>9}",
        selector = "case",
        name = "name",
        kind = "dispatch",
        done = "done",
        failed = "failed",
        error = "error",
        retry = "retry",
        turns = "turns",
        tools = "tools",
        input = "in",
        output = "out",
        cache = "cache_r",
        cost = "cost",
        wall = "wall",
        selector_width = selector_width,
        name_width = name_width,
    );
    for ((case_id, kind), summary) in summary_rows {
        let (selector, name) = display_case_label(&case_labels, &case_id);
        println!(
            "{selector:<selector_width$}  {name:<name_width$}  {kind:<18} {done:>4} {failed:>6} {error:>5} {retries:>5} {turns:>6} {tools:>6} {input:>9} {output:>8} {cache_read:>8} {cost:>10} {wall:>9}",
            selector = selector,
            name = name,
            done = summary.done,
            failed = summary.failed,
            error = summary.errors,
            retries = summary.retries,
            turns = display_count(summary.assistant_turns),
            tools = display_count(summary.tool_calls),
            input = display_count(summary.prompt_tokens),
            output = display_count(summary.completion_tokens),
            cache_read = display_count(summary.cache_read_tokens),
            cost = format_cost(summary.cost_micros, None),
            wall = format_elapsed_ms(summary.wall_ms),
            selector_width = selector_width,
            name_width = name_width,
        );
    }
    print_tool_policy_report(&records, &case_labels);
    print_prompt_context_report(store, run_id, case_id, &case_labels)?;
    Ok(())
}

/// Aggregates canonical dispatch ledgers across historical runs that share a
/// suite, prompt, worker-model, and config identity. Runs without a retained
/// suite identity stay in their own compatibility group rather than being
/// silently compared to newer data.
pub fn cmd_bench_report_history(store: &BenchStore) -> anyhow::Result<()> {
    let runs = store.read_runs()?;
    let mut groups = BTreeMap::<
        (String, String, String, String),
        Vec<(BenchRun, Vec<BenchDispatchRecord>)>,
    >::new();
    for run in runs {
        let records = store.read_dispatches(&run.run_id)?;
        if records.is_empty() {
            continue;
        }
        let suite = run.suite_manifest.as_ref().map_or_else(
            || format!("historical:{}", run.run_id),
            |manifest| manifest.fingerprint.clone(),
        );
        let prompt = run.prompt_variant.clone().unwrap_or_else(|| "-".into());
        let model = run.worker_model.clone().unwrap_or_else(|| "-".into());
        groups
            .entry((suite, prompt, model, run.config_hash.clone()))
            .or_default()
            .push((run, records));
    }
    if groups.is_empty() {
        anyhow::bail!("no saved runs with persisted dispatch telemetry");
    }

    println!("== historical dispatch report ==");
    for ((suite, prompt, model, config), runs) in groups {
        let mut records = Vec::new();
        for (_, run_records) in &runs {
            records.extend(run_records.iter().cloned());
        }
        println!(
            "\n-- runs={} suite={} prompt={} model={} config={} dispatches={} --",
            runs.len(),
            short_hash(&suite),
            prompt,
            model,
            short_hash(&config),
            records.len(),
        );
        let summaries = summarize_dispatches(&records);
        for ((case_id, kind), summary) in summaries {
            let total = summary
                .done
                .saturating_add(summary.failed)
                .saturating_add(summary.errors);
            let pass_rate = if total == 0 {
                0.0
            } else {
                f64::from(summary.done) / f64::from(total)
            };
            println!(
                "{case_id:<28} {kind:<18} runs={total:>3} done={done:>3} error={error:>3} pass={pass_rate:>5.1}% retry={retries:>3} in={input:>8} cache_r={cache:>8} cost={cost:>10} wall={wall:>9}",
                done = summary.done,
                error = summary.errors,
                retries = summary.retries,
                input = display_count(summary.prompt_tokens),
                cache = display_count(summary.cache_read_tokens),
                cost = format_cost(summary.cost_micros, None),
                wall = format_elapsed_ms(summary.wall_ms),
                pass_rate = pass_rate * 100.0,
            );
        }
    }
    Ok(())
}

/// Prints resolved system/user prompt snapshots for a single benchmark case.
///
/// # Errors
///
/// Returns an error when no retained snapshots match the requested case/orb.
pub fn cmd_bench_prompts(
    store: &BenchStore,
    run_id: &str,
    case_id: &str,
    orb_id: Option<&str>,
) -> anyhow::Result<()> {
    let mut prompts = store.read_prompts(run_id)?;
    prompts.retain(|record| {
        record.case_id == case_id && orb_id.is_none_or(|orb_id| record.prompt.orb_id == orb_id)
    });
    if prompts.is_empty() {
        anyhow::bail!("no retained prompt snapshots match that run, case, and orb");
    }
    let run = store
        .read_runs()?
        .into_iter()
        .find(|run| run.run_id == run_id)
        .context("no saved benchmark run with that id")?;
    let case_labels = case_labels_for_run(&run);
    for record in prompts {
        let (selector, name) = display_case_label(&case_labels, &record.case_id);
        println!(
            "== {selector} {name} · {} [{}] ==",
            record.prompt.orb_id, record.prompt.dispatch_kind,
        );
        println!(
            "input_tokens={} system_hash={} user_hash={}",
            display_count(record.prompt.input_tokens),
            record.prompt.system_prompt_hash,
            record.prompt.user_prompt_hash,
        );
        println!("\n-- system prompt --\n{}", record.prompt.system_prompt);
        println!("\n-- user prompt --\n{}", record.prompt.user_prompt);
    }
    Ok(())
}

#[derive(Default)]
struct DispatchSummary {
    done: u32,
    failed: u32,
    errors: u32,
    retries: u32,
    assistant_turns: Option<u64>,
    tool_calls: Option<u64>,
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cost_micros: Option<u64>,
    wall_ms: u64,
}

fn summarize_dispatches(
    records: &[BenchDispatchRecord],
) -> BTreeMap<(String, String), DispatchSummary> {
    let mut summaries = BTreeMap::new();
    for record in records {
        let summary = summaries
            .entry((
                record.case_id.clone(),
                record.execution.dispatch_kind.clone(),
            ))
            .or_insert_with(DispatchSummary::default);
        match record.execution.status.as_str() {
            "done" => summary.done = summary.done.saturating_add(1),
            "failed" => summary.failed = summary.failed.saturating_add(1),
            "error" => summary.errors = summary.errors.saturating_add(1),
            _ => {}
        }
        summary.retries = summary.retries.saturating_add(record.execution.retries);
        add_optional_u32(
            &mut summary.assistant_turns,
            record.execution.assistant_turns,
        );
        add_optional_u32(&mut summary.tool_calls, record.execution.tool_calls);
        add_optional_u64(&mut summary.prompt_tokens, record.execution.prompt_tokens);
        add_optional_u64(
            &mut summary.completion_tokens,
            record.execution.completion_tokens,
        );
        add_optional_u64(
            &mut summary.cache_read_tokens,
            record.execution.cache_read_tokens,
        );
        add_optional_u64(&mut summary.cost_micros, record.execution.cost_micros);
        let duration = (record.execution.completed_at - record.execution.dispatched_at)
            .num_milliseconds()
            .max(0);
        summary.wall_ms = summary
            .wall_ms
            .saturating_add(u64::try_from(duration).unwrap_or(u64::MAX));
    }
    summaries
}

fn print_tool_policy_report(
    records: &[BenchDispatchRecord],
    case_labels: &HashMap<String, (String, String)>,
) {
    let mut counts = BTreeMap::<(String, String, String, String, String), u32>::new();
    for record in records {
        let policy = record
            .execution
            .tool_policy
            .as_deref()
            .unwrap_or("-")
            .to_string();
        let source = record
            .execution
            .tool_policy_source
            .as_deref()
            .unwrap_or("-")
            .to_string();
        let tools = match record.execution.allowed_tools.as_deref() {
            None => "-".to_string(),
            Some([]) => "(none)".to_string(),
            Some(tools) => tools.join(","),
        };
        let key = (
            record.case_id.clone(),
            record.execution.dispatch_kind.clone(),
            policy,
            source,
            tools,
        );
        let count = counts.entry(key).or_default();
        *count = count.saturating_add(1);
    }
    if counts.is_empty() {
        return;
    }

    let mut rows: Vec<_> = counts.into_iter().collect();
    rows.sort_by(
        |((case_a, kind_a, _, _, _), _), ((case_b, kind_b, _, _, _), _)| {
            display_case_label(case_labels, case_a)
                .0
                .cmp(&display_case_label(case_labels, case_b).0)
                .then_with(|| kind_a.cmp(kind_b))
        },
    );
    let (selector_width, name_width) = report_case_widths(
        rows.iter()
            .map(|((case_id, _, _, _, _), _)| case_id.as_str()),
        case_labels,
    );
    println!("\n== effective tool policies ==");
    println!(
        "{selector:<selector_width$}  {name:<name_width$}  {kind:<18} {policy:<14} {source:<14} {tools:<40} {count:>5}",
        selector = "case",
        name = "name",
        kind = "dispatch",
        policy = "policy",
        source = "source",
        tools = "allowed tools",
        count = "count",
        selector_width = selector_width,
        name_width = name_width,
    );
    for ((case_id, kind, policy, source, tools), count) in rows {
        let (selector, name) = display_case_label(case_labels, &case_id);
        let row = format!(
            "{selector:<selector_width$}  {name:<name_width$}  {kind:<18} {policy:<14} {source:<14} {tools:<40} {count:>5}"
        );
        println!("{row}");
    }
}

fn add_optional_u32(total: &mut Option<u64>, value: Option<u32>) {
    add_optional_u64(total, value.map(u64::from));
}

fn add_optional_u64(total: &mut Option<u64>, value: Option<u64>) {
    if let Some(value) = value {
        *total = Some(total.unwrap_or(0).saturating_add(value));
    }
}

#[derive(Default)]
struct PromptSummary {
    count: u32,
    input_tokens: Option<u64>,
    system_prompt_chars: u64,
    final_user_prompt_chars: u64,
    task_context_chars: u64,
    task_context_overhead_chars: u64,
    current_orb_chars: u64,
    parent_and_root_chars: u64,
    sibling_orbs_chars: u64,
    child_orbs_chars: u64,
    upstream_dependency_chars: u64,
}

fn print_prompt_context_report(
    store: &BenchStore,
    run_id: &str,
    case_id: Option<&str>,
    case_labels: &HashMap<String, (String, String)>,
) -> anyhow::Result<()> {
    let mut prompts = store.read_prompts(run_id)?;
    if let Some(case_id) = case_id {
        prompts.retain(|record| record.case_id == case_id);
    }
    if prompts.is_empty() {
        println!("\n== prompt context ==\nno retained prompt snapshots (historical run)");
        return Ok(());
    }
    println!("\n== prompt context (Orboros-owned chars) ==");
    let summaries = summarize_prompts(&prompts);
    let mut summary_rows: Vec<_> = summaries.into_iter().collect();
    summary_rows.sort_by(|((case_a, kind_a), _), ((case_b, kind_b), _)| {
        display_case_label(case_labels, case_a)
            .0
            .cmp(&display_case_label(case_labels, case_b).0)
            .then_with(|| kind_a.cmp(kind_b))
    });
    let (selector_width, name_width) = report_case_widths(
        summary_rows
            .iter()
            .map(|((case_id, _), _)| case_id.as_str()),
        case_labels,
    );
    println!(
        "{selector:<selector_width$}  {name:<name_width$}  {kind:<18} {count:>5} {tokens:>9} {system:>7} {user:>7} {context:>7} {overhead:>8} {current:>7} {parent:>7} {siblings:>8} {children:>8} {deps:>7}",
        selector = "case",
        name = "name",
        kind = "dispatch",
        count = "count",
        tokens = "in_tok",
        system = "system",
        user = "user",
        context = "context",
        overhead = "overhead",
        current = "current",
        parent = "parent",
        siblings = "siblings",
        children = "children",
        deps = "deps",
        selector_width = selector_width,
        name_width = name_width,
    );
    for ((case_id, kind), summary) in summary_rows {
        let (selector, name) = display_case_label(case_labels, &case_id);
        println!(
            "{selector:<selector_width$}  {name:<name_width$}  {kind:<18} {count:>5} {tokens:>9} {system:>7} {user:>7} {context:>7} {overhead:>8} {current:>7} {parent:>7} {siblings:>8} {children:>8} {deps:>7}",
            selector = selector,
            name = name,
            count = summary.count,
            tokens = display_count(summary.input_tokens),
            system = summary.system_prompt_chars,
            user = summary.final_user_prompt_chars,
            context = summary.task_context_chars,
            overhead = summary.task_context_overhead_chars,
            current = summary.current_orb_chars,
            parent = summary.parent_and_root_chars,
            siblings = summary.sibling_orbs_chars,
            children = summary.child_orbs_chars,
            deps = summary.upstream_dependency_chars,
            selector_width = selector_width,
            name_width = name_width,
        );
    }
    println!(
        "input tokens are provider-reported; opaque Heddle/provider context is not measured here."
    );
    Ok(())
}

fn summarize_prompts(records: &[BenchPromptRecord]) -> BTreeMap<(String, String), PromptSummary> {
    let mut summaries = BTreeMap::new();
    for record in records {
        let summary = summaries
            .entry((record.case_id.clone(), record.prompt.dispatch_kind.clone()))
            .or_insert_with(PromptSummary::default);
        summary.count = summary.count.saturating_add(1);
        add_optional_u64(&mut summary.input_tokens, record.prompt.input_tokens);
        let metrics = &record.prompt.prompt_context;
        summary.system_prompt_chars = summary
            .system_prompt_chars
            .saturating_add(u64::from(metrics.effective_system_prompt_chars));
        summary.final_user_prompt_chars = summary
            .final_user_prompt_chars
            .saturating_add(u64::from(metrics.final_user_prompt_chars));
        summary.task_context_chars = summary
            .task_context_chars
            .saturating_add(u64::from(metrics.task_context_chars));
        summary.task_context_overhead_chars = summary
            .task_context_overhead_chars
            .saturating_add(u64::from(metrics.task_context_overhead_chars));
        summary.current_orb_chars = summary
            .current_orb_chars
            .saturating_add(u64::from(metrics.current_orb_chars));
        summary.parent_and_root_chars = summary
            .parent_and_root_chars
            .saturating_add(u64::from(metrics.parent_and_root_chars));
        summary.sibling_orbs_chars = summary
            .sibling_orbs_chars
            .saturating_add(u64::from(metrics.sibling_orbs_chars));
        summary.child_orbs_chars = summary
            .child_orbs_chars
            .saturating_add(u64::from(metrics.child_orbs_chars));
        summary.upstream_dependency_chars = summary
            .upstream_dependency_chars
            .saturating_add(u64::from(metrics.upstream_dependency_chars));
    }
    summaries
}

/// Compares two saved runs side by side. Highlights case status changes and
/// reports prompt-set provenance at the run level.
///
/// # Errors
///
/// Returns an error if either run id is unknown.
pub fn cmd_bench_compare(store: &BenchStore, run_a: &str, run_b: &str) -> anyhow::Result<()> {
    let a = store.read_results(run_a)?;
    let b = store.read_results(run_b)?;
    if a.is_empty() {
        anyhow::bail!("no results found for run `{run_a}`");
    }
    if b.is_empty() {
        anyhow::bail!("no results found for run `{run_b}`");
    }

    let by_case_b: std::collections::HashMap<&str, &crate::bench::store::BenchResult> =
        b.iter().map(|r| (r.case_id.as_str(), r)).collect();
    let runs = store.read_runs()?;
    let run_meta_a = runs.iter().find(|r| r.run_id == run_a);
    let run_meta_b = runs.iter().find(|r| r.run_id == run_b);

    if let Some(run) = run_meta_a {
        print_run_summary(run);
    }
    if let Some(run) = run_meta_b {
        print_run_summary(run);
    }
    warn_on_run_metadata_drift(run_meta_a, run_meta_b);
    report_suite_manifest_difference(run_meta_a, run_meta_b);
    report_prompt_manifest_difference(run_meta_a, run_meta_b);

    let case_labels = run_meta_a
        .or(run_meta_b)
        .map_or_else(HashMap::new, case_labels_for_run);
    let (selector_width, name_width) = report_case_widths(
        a.iter().chain(&b).map(|result| result.case_id.as_str()),
        &case_labels,
    );
    let a_label = run_display_label(run_a);
    let b_label = run_display_label(run_b);
    let a_width = a_label.len().max(8);
    let b_width = b_label.len().max(8);
    println!(
        "{selector:<selector_width$}  {name:<name_width$}  {a_status:<a_width$}  {b_status:<b_width$}  change",
        selector = "case",
        name = "name",
        a_status = a_label,
        b_status = b_label,
        selector_width = selector_width,
        name_width = name_width,
    );
    let mut improved = 0;
    let mut regressed = 0;
    let mut only_in_a = 0;
    let mut only_in_b = 0;
    for r in &a {
        if let Some(rb) = by_case_b.get(r.case_id.as_str()) {
            let change = match (r.status, rb.status) {
                (BenchStatus::Pass, BenchStatus::Pass) => "—",
                (BenchStatus::Fail | BenchStatus::Error, BenchStatus::Pass) => {
                    improved += 1;
                    "improved"
                }
                (BenchStatus::Pass, BenchStatus::Fail | BenchStatus::Error) => {
                    regressed += 1;
                    "regressed"
                }
                _ => "changed",
            };
            let (selector, name) = display_case_label(&case_labels, &r.case_id);
            println!(
                "{selector:<selector_width$}  {name:<name_width$}  {a:<a_width$?}  {b:<b_width$?}  {change}",
                selector = selector,
                name = name,
                a = r.status,
                b = rb.status,
                selector_width = selector_width,
                name_width = name_width,
            );
        } else {
            only_in_a += 1;
            let (selector, name) = display_case_label(&case_labels, &r.case_id);
            println!(
                "{selector:<selector_width$}  {name:<name_width$}  {a:<a_width$?}  {b:<b_width$}  only in {a_label}",
                selector = selector,
                name = name,
                a = r.status,
                b = "-",
                selector_width = selector_width,
                name_width = name_width,
            );
        }
    }
    for rb in &b {
        if !a.iter().any(|ra| ra.case_id == rb.case_id) {
            only_in_b += 1;
            let (selector, name) = display_case_label(&case_labels, &rb.case_id);
            println!(
                "{selector:<selector_width$}  {name:<name_width$}  {a:<a_width$}  {b:<b_width$?}  only in {b_label}",
                selector = selector,
                name = name,
                a = "-",
                b = rb.status,
                selector_width = selector_width,
                name_width = name_width,
            );
        }
    }

    println!("\nimproved: {improved}, regressed: {regressed}");
    if only_in_a > 0 || only_in_b > 0 {
        eprintln!(
            "warning: case sets differ ({only_in_a} only in {run_a}, {only_in_b} only in {run_b})"
        );
    }
    Ok(())
}

/// Lists every recorded run, newest first.
///
/// # Errors
///
/// Returns an error if the store can't be read.
pub struct BenchRunFilter<'a> {
    pub model: Option<&'a str>,
    pub tier: Option<BenchTier>,
    pub suite: Option<&'a str>,
    pub prompt_set: Option<&'a str>,
    pub since: Option<NaiveDate>,
    pub limit: Option<usize>,
}

pub fn cmd_bench_list_runs(store: &BenchStore) -> anyhow::Result<()> {
    cmd_bench_list_runs_filtered(
        store,
        &BenchRunFilter {
            model: None,
            tier: None,
            suite: None,
            prompt_set: None,
            since: None,
            limit: None,
        },
    )
}

/// Lists saved runs with optional metadata filters and resource-guidance
/// status totals calculated from immutable result snapshots.
pub fn cmd_bench_list_runs_filtered(
    store: &BenchStore,
    filter: &BenchRunFilter<'_>,
) -> anyhow::Result<()> {
    let mut runs = store.read_runs()?;
    runs.sort_by_key(|run| std::cmp::Reverse(run.started_at));
    runs.retain(|run| run_matches_filter(run, filter));
    if let Some(limit) = filter.limit {
        runs.truncate(limit);
    }
    if runs.is_empty() {
        println!("No matching runs recorded.");
        return Ok(());
    }

    println!(
        "run           started              suite         tier  model                         pass fail err  cost         guidance"
    );
    for run in &runs {
        let results = store.read_results(&run.run_id)?;
        let (under, over, investigate) = stored_resource_target_counts(&results);
        let guidance = if under + over + investigate == 0 {
            "-".to_string()
        } else {
            format!("{under}U {over}O {investigate}I")
        };
        println!(
            "{run_id:<12}  {started:<19}  {suite:<12}  {tier:<4}  {model:<28}  {passed:>4} {failed:>4} {errored:>3}  {cost:<11}  {guidance}",
            run_id = run_display_label(&run.run_id),
            started = run.started_at.format("%Y-%m-%d %H:%M:%S"),
            suite = run
                .suite_manifest
                .as_ref()
                .map_or("legacy", |suite| short_hash(&suite.fingerprint)),
            tier = run_tier_label(run),
            model = run.worker_model.as_deref().unwrap_or("-"),
            passed = run.passed,
            failed = run.failed,
            errored = run.errored,
            cost = format_cost(run.total_cost_micros, run.total_cost_cents),
            guidance = guidance,
        );
    }
    println!("\n{} run(s)", runs.len());
    Ok(())
}

fn run_matches_filter(run: &BenchRun, filter: &BenchRunFilter<'_>) -> bool {
    if let Some(model) = filter.model {
        let matches = [
            run.model_selector.as_deref(),
            run.worker_model.as_deref(),
            run.model_key.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|candidate| candidate.contains(model));
        if !matches {
            return false;
        }
    }
    if let Some(tier) = filter.tier {
        if !run.tiers.contains(&tier) && run.tier != Some(tier) {
            return false;
        }
    }
    if let Some(suite) = filter.suite {
        if !run
            .suite_manifest
            .as_ref()
            .is_some_and(|manifest| manifest.fingerprint.starts_with(suite))
        {
            return false;
        }
    }
    if let Some(prompt_set) = filter.prompt_set {
        if run.prompt_variant.as_deref() != Some(prompt_set) {
            return false;
        }
    }
    filter
        .since
        .is_none_or(|since| run.started_at.date_naive() >= since)
}

fn stored_resource_target_counts(results: &[BenchResult]) -> (u32, u32, u32) {
    let mut under = 0;
    let mut over = 0;
    let mut investigate = 0;
    for result in results {
        match result_resource_guidance(result, None)
            .and_then(|guidance| resource_target_status(result, guidance))
        {
            Some(ResourceTargetStatus::Under) => under += 1,
            Some(ResourceTargetStatus::Over) => over += 1,
            Some(ResourceTargetStatus::Investigate) => investigate += 1,
            None => {}
        }
    }
    (under, over, investigate)
}

fn print_completed_run(
    run: &BenchRun,
    results: &[BenchResult],
    case_labels: &HashMap<String, (String, String)>,
    resource_guidance: &HashMap<String, crate::bench::case::BenchResourceGuidance>,
) {
    println!("\n== summary ==");
    print_run_summary(run);
    print_result_table(results, Some(case_labels), Some(resource_guidance));
    print_run_completion(run, results, resource_guidance);
}

/// Shows the aggregate case work separately from elapsed wall-clock time.
/// This only appears for opt-in parallel runs: serial output remains unchanged.
fn print_parallel_timing(run: &BenchRun, results: &[BenchResult], jobs: usize) {
    let case_work_ms = results.iter().fold(0_u64, |total, result| {
        total.saturating_add(result.latency_ms)
    });
    let wall_ms = u64::try_from((run.finished_at - run.started_at).num_milliseconds()).unwrap_or(0);
    let speedup = effective_speedup(case_work_ms, wall_ms);
    println!("\n== parallel timing ==");
    println!(
        "aggregate case work: {} (sum of per-case elapsed time)",
        format_elapsed_ms(case_work_ms)
    );
    println!("wall-clock elapsed:   {}", format_elapsed_ms(wall_ms));
    match speedup {
        Some(speedup) => println!("effective speedup:    {speedup:.2}x ({jobs} case jobs)"),
        None => println!("effective speedup:    unavailable (zero wall-clock elapsed)"),
    }
}

fn effective_speedup(case_work_ms: u64, wall_ms: u64) -> Option<f64> {
    (wall_ms != 0).then(|| {
        let case_work_ms = u32::try_from(case_work_ms).unwrap_or(u32::MAX);
        let wall_ms = u32::try_from(wall_ms).unwrap_or(u32::MAX);
        f64::from(case_work_ms) / f64::from(wall_ms)
    })
}

fn print_result_table(
    results: &[crate::bench::store::BenchResult],
    case_labels: Option<&HashMap<String, (String, String)>>,
    resource_guidance: Option<&HashMap<String, crate::bench::case::BenchResourceGuidance>>,
) {
    let selector_width = case_labels
        .into_iter()
        .flat_map(|labels| labels.values().map(|(selector, _)| selector.len()))
        .max()
        .unwrap_or(4)
        .max(8);
    let name_width = case_labels
        .into_iter()
        .flat_map(|labels| labels.values().map(|(_, name)| name.len()))
        .max()
        .unwrap_or(4)
        .max(20);
    println!(
        "{selector:<selector_width$}  {name:<name_width$}  {status:<8}  {score:>5}  {process:>7}  {elapsed:>9}  {cost:>10}  {turns:>5}  {tools:>5}  {input:>8}  {output:>8}  {cache_r:>8}  {cache_w:>8}  {conf:>5}  {target:>11}",
        selector = "case",
        name = "name",
        status = "status",
        score = "score",
        process = "process",
        elapsed = "elapsed",
        cost = "cost",
        turns = "turns",
        tools = "tools",
        input = "in",
        output = "out",
        cache_r = "cache_r",
        cache_w = "cache_w",
        conf = "conf",
        target = "target",
        selector_width = selector_width,
        name_width = name_width,
    );
    for r in results {
        let (selector, name) = case_labels
            .and_then(|labels| labels.get(&r.case_id))
            .map_or_else(|| (r.tier.to_string(), "-".to_string()), Clone::clone);
        let status = format!("{:?}", r.status);
        let latency = format_elapsed_ms(r.latency_ms);
        let cost = format_cost(r.cost_micros, r.cost_cents);
        let turns = r
            .assistant_turns
            .map_or(String::from("-"), |turns| turns.to_string());
        let tools = r
            .tool_calls
            .map_or(String::from("-"), |calls| calls.to_string());
        let conf = r
            .confidence
            .map_or(String::from("-"), |c| format!("{c:.2}"));
        let process = r
            .process_score
            .map_or(String::from("-"), |score| format!("{score:.2}"));
        let input = r
            .prompt_tokens
            .map_or(String::from("-"), |tokens| tokens.to_string());
        let output = r
            .completion_tokens
            .map_or(String::from("-"), |tokens| tokens.to_string());
        let cache_read = r
            .cache_read_tokens
            .map_or(String::from("-"), |tokens| tokens.to_string());
        let cache_write = r
            .cache_write_tokens
            .map_or(String::from("-"), |tokens| tokens.to_string());
        let target = result_resource_guidance(r, resource_guidance)
            .and_then(|guidance| resource_target_status(r, guidance))
            .map_or("-", ResourceTargetStatus::label);
        println!(
            "{selector:<selector_width$}  {name:<name_width$}  {status:<8}  {score:>5.2}  {process:>7}  {elapsed:>9}  {cost:>10}  {turns:>5}  {tools:>5}  {input:>8}  {output:>8}  {cache_read:>8}  {cache_write:>8}  {conf:>5}  {target:>11}",
            selector = selector,
            name = name,
            status = status,
            score = r.score,
            process = process,
            elapsed = latency,
            cost = cost,
            turns = turns,
            tools = tools,
            input = input,
            output = output,
            cache_read = cache_read,
            cache_write = cache_write,
            conf = conf,
            target = target,
            selector_width = selector_width,
            name_width = name_width,
        );
    }
}

fn format_elapsed_ms(elapsed_ms: u64) -> String {
    if elapsed_ms < 1_000 {
        return format!("{elapsed_ms}ms");
    }
    let seconds = elapsed_ms / 1_000;
    if seconds < 60 {
        let tenths = elapsed_ms / 100;
        return format!("{}.{}s", tenths / 10, tenths % 10);
    }
    let minutes = seconds / 60;
    let remaining_seconds = seconds % 60;
    if minutes < 60 {
        return format!("{minutes}m {remaining_seconds:02}s");
    }
    format!(
        "{}h {:02}m {remaining_seconds:02}s",
        seconds / 3_600,
        minutes % 60
    )
}

fn print_run_completion(
    run: &BenchRun,
    results: &[BenchResult],
    resource_guidance: &HashMap<String, crate::bench::case::BenchResourceGuidance>,
) {
    let model = run
        .worker_model
        .as_deref()
        .or_else(|| results.first().map(|result| result.worker_model.as_str()))
        .unwrap_or("-");
    println!("\n== run complete ==");
    println!("run: {}", run.run_id);
    println!("model: {model}");
    println!(
        "{} passed, {} failed, {} errored, {} skipped of {} total",
        run.passed, run.failed, run.errored, run.skipped, run.total
    );

    let process_results: Vec<&BenchResult> = results
        .iter()
        .filter(|result| result.process_score.is_some())
        .collect();
    if !process_results.is_empty() {
        let fully_met = process_results
            .iter()
            .filter(|result| result.process_score == Some(1.0))
            .count();
        let earned: f32 = process_results
            .iter()
            .filter_map(|result| result.process_score)
            .sum();
        println!(
            "process: {fully_met}/{} cases fully met their contract ({earned:.2}/{} points)",
            process_results.len(),
            process_results.len()
        );
    }

    let target_statuses: Vec<_> = results
        .iter()
        .filter_map(|result| {
            result_resource_guidance(result, Some(resource_guidance))
                .and_then(|guidance| resource_target_status(result, guidance))
        })
        .collect();
    if !target_statuses.is_empty() {
        let under = target_statuses
            .iter()
            .filter(|status| **status == ResourceTargetStatus::Under)
            .count();
        let over = target_statuses
            .iter()
            .filter(|status| **status == ResourceTargetStatus::Over)
            .count();
        let investigate = target_statuses
            .iter()
            .filter(|status| **status == ResourceTargetStatus::Investigate)
            .count();
        println!(
            "targets: {under} UNDER, {over} OVER, {investigate} INVESTIGATE of {} guided cases",
            target_statuses.len()
        );
    }

    let causes = failure_causes(results);
    if !causes.is_empty() {
        let causes = causes
            .iter()
            .map(|(cause, count)| format!("{cause}={count}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!("failure causes: {causes}");
    }

    println!(
        "cache tokens: {} read, {} written",
        display_count(run.cache_read_tokens),
        display_count(run.cache_write_tokens)
    );
    println!(
        "tokens: {} prompt + {} completion = {} tokens, {}",
        display_count(run.prompt_tokens),
        display_count(run.completion_tokens),
        display_count(run.total_tokens),
        format_cost(run.total_cost_micros, run.total_cost_cents)
    );
    println!(
        "activity: {} assistant turns, {} tool calls",
        display_count(run.assistant_turns),
        display_count(run.tool_calls)
    );
    let elapsed_ms = u64::try_from((run.finished_at - run.started_at).num_milliseconds().max(0))
        .unwrap_or(u64::MAX);
    println!("wall time: {}", format_elapsed_ms(elapsed_ms));
}

fn display_count(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_string(), |value| value.to_string())
}

fn failure_causes(results: &[BenchResult]) -> Vec<(&'static str, u32)> {
    let mut causes = HashMap::new();
    for result in results {
        if !matches!(result.status, BenchStatus::Fail | BenchStatus::Error) {
            continue;
        }
        let detail = result
            .error
            .as_deref()
            .or(result.output.as_deref())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let cause = if detail.contains("[worker_error]")
            || detail.contains("provider")
            || detail.contains("streaming response")
        {
            "provider_api"
        } else if detail.contains("timed out") {
            "timeout"
        } else if result.status == BenchStatus::Fail {
            "tests_failed"
        } else {
            "worker_or_harness"
        };
        *causes.entry(cause).or_insert(0u32) += 1;
    }
    let mut causes: Vec<_> = causes.into_iter().collect();
    causes.sort_unstable_by_key(|(cause, _)| *cause);
    causes
}

fn print_result_details(result: &BenchResult) {
    println!(
        "\n== {case} [{tier}] {status:?} ==",
        case = result.case_id,
        tier = result.tier,
        status = result.status,
    );
    println!(
        "score={score:.2} process_score={process_score} elapsed={elapsed}ms tokens_in={input} tokens_out={output} cache_r={cache_read} cache_w={cache_write} cost={cost}",
        score = result.score,
        process_score = result
            .process_score
            .map_or_else(|| "-".to_string(), |score| format!("{score:.2}")),
        elapsed = result.latency_ms,
        input = result
            .prompt_tokens
            .map_or_else(|| "-".to_string(), |tokens| tokens.to_string()),
        output = result
            .completion_tokens
            .map_or_else(|| "-".to_string(), |tokens| tokens.to_string()),
        cache_read = result
            .cache_read_tokens
            .map_or_else(|| "-".to_string(), |tokens| tokens.to_string()),
        cache_write = result
            .cache_write_tokens
            .map_or_else(|| "-".to_string(), |tokens| tokens.to_string()),
        cost = format_cost(result.cost_micros, result.cost_cents),
    );
    for annotation in &result.process_annotations {
        println!("{annotation}");
    }
    println!(
        "worker_latency model={model}ms tool={tool}ms total={total}ms turns={turns} tools={tools}",
        model = result
            .model_latency_ms
            .map_or_else(|| "-".to_string(), |value| value.to_string()),
        tool = result
            .tool_latency_ms
            .map_or_else(|| "-".to_string(), |value| value.to_string()),
        total = result
            .total_latency_ms
            .map_or_else(|| "-".to_string(), |value| value.to_string()),
        turns = result
            .assistant_turns
            .map_or_else(|| "-".to_string(), |value| value.to_string()),
        tools = result
            .tool_calls
            .map_or_else(|| "-".to_string(), |value| value.to_string()),
    );
    println!("worker_model={}", result.worker_model);
    if let Some(confidence) = result.confidence {
        println!("confidence={confidence:.2}");
    }
    if let Some(error) = result.error.as_deref() {
        println!("\n-- error --");
        println!("{error}");
    }
    if let Some(output) = result.output.as_deref() {
        println!("\n-- output --");
        println!("{output}");
    }
    if result.error.is_none() && result.output.is_none() {
        println!("\n(no saved error/output)");
    }
}

fn case_labels_for_run(run: &BenchRun) -> HashMap<String, (String, String)> {
    run.cases_root
        .as_deref()
        .and_then(|root| load_all(Path::new(root)).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|case| (case.id, (case.selector, case.name)))
        .collect()
}

fn resource_guidance_for_run(
    run: &BenchRun,
) -> HashMap<String, crate::bench::case::BenchResourceGuidance> {
    run.cases_root
        .as_deref()
        .and_then(|root| load_all(Path::new(root)).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|case| case.resource_guidance.map(|guidance| (case.id, guidance)))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceTargetStatus {
    Under,
    Over,
    Investigate,
}

impl ResourceTargetStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Under => "UNDER",
            Self::Over => "OVER",
            Self::Investigate => "INVESTIGATE",
        }
    }
}

fn resource_target_status(
    result: &BenchResult,
    guidance: &crate::bench::case::BenchResourceGuidance,
) -> Option<ResourceTargetStatus> {
    let measurements = [
        (guidance.cost_micros.as_ref(), result.cost_micros),
        (guidance.input_tokens.as_ref(), result.prompt_tokens),
        (
            guidance.cache_read_tokens.as_ref(),
            result.cache_read_tokens,
        ),
        (guidance.elapsed_ms.as_ref(), Some(result.latency_ms)),
        (
            guidance.assistant_turns.as_ref(),
            result.assistant_turns.map(u64::from),
        ),
        (
            guidance.tool_calls.as_ref(),
            result.tool_calls.map(u64::from),
        ),
    ];
    let mut over = false;
    let mut measured = false;
    for (threshold, value) in measurements {
        let Some(threshold) = threshold else {
            continue;
        };
        let value = value?;
        measured = true;
        if value > threshold.investigate {
            return Some(ResourceTargetStatus::Investigate);
        }
        if value > threshold.target {
            over = true;
        }
    }
    measured.then_some(if over {
        ResourceTargetStatus::Over
    } else {
        ResourceTargetStatus::Under
    })
}

fn result_resource_guidance<'a>(
    result: &'a BenchResult,
    live_guidance: Option<&'a HashMap<String, crate::bench::case::BenchResourceGuidance>>,
) -> Option<&'a crate::bench::case::BenchResourceGuidance> {
    result
        .resource_guidance
        .as_ref()
        .or_else(|| live_guidance.and_then(|guidance| guidance.get(&result.case_id)))
}

fn display_case_label(
    case_labels: &HashMap<String, (String, String)>,
    case_id: &str,
) -> (String, String) {
    case_labels
        .get(case_id)
        .cloned()
        .unwrap_or_else(|| (case_id.into(), "-".into()))
}

fn report_case_widths<'a>(
    ids: impl Iterator<Item = &'a str>,
    case_labels: &HashMap<String, (String, String)>,
) -> (usize, usize) {
    ids.map(|id| display_case_label(case_labels, id)).fold(
        (4, 4),
        |(selector_width, name_width), (selector, name)| {
            (
                selector_width.max(selector.len()),
                name_width.max(name.len()),
            )
        },
    )
}

fn run_display_label(run_id: &str) -> &str {
    run_id.rsplit_once('-').map_or(run_id, |(_, suffix)| suffix)
}

fn report_prompt_manifest_difference(a: Option<&BenchRun>, b: Option<&BenchRun>) {
    match (
        a.and_then(|run| run.prompt_manifest.as_ref()),
        b.and_then(|run| run.prompt_manifest.as_ref()),
    ) {
        (Some(a), Some(b)) if a == b => {
            println!("prompt set: {} (matching manifest)", a.prompt_set);
        }
        (Some(a), Some(b)) => {
            eprintln!(
                "warning: prompt manifests differ ({} vs {}); direct comparison may be misleading",
                a.prompt_set, b.prompt_set
            );
        }
        (None, None) => {}
        _ => eprintln!(
            "note: prompt manifest is unavailable for one run; prompt-set compatibility cannot be verified"
        ),
    }
}

fn report_suite_manifest_difference(a: Option<&BenchRun>, b: Option<&BenchRun>) {
    match (
        a.and_then(|run| run.suite_manifest.as_ref()),
        b.and_then(|run| run.suite_manifest.as_ref()),
    ) {
        (Some(a), Some(b)) if a.fingerprint == b.fingerprint => {
            println!("suite: {} (matching)", short_hash(&a.fingerprint));
        }
        (Some(a), Some(b)) => {
            eprintln!(
                "warning: suite fingerprints differ ({} vs {}); direct comparison may be misleading",
                short_hash(&a.fingerprint),
                short_hash(&b.fingerprint)
            );
        }
        (None, None) => {}
        _ => eprintln!(
            "note: suite fingerprint is unavailable for one run; suite compatibility cannot be verified"
        ),
    }
}

fn summarize_run(
    run_id: &str,
    started_at: DateTime<Utc>,
    tier: Option<BenchTier>,
    results: &[BenchResult],
    run_config: &BenchRunConfig,
    base_worker_config: &WorkerConfig,
) -> BenchRun {
    let total = u32::try_from(results.len()).unwrap_or(u32::MAX);
    let passed = count_status(results, BenchStatus::Pass);
    let failed = count_status(results, BenchStatus::Fail);
    let errored = count_status(results, BenchStatus::Error);
    let skipped = count_status(results, BenchStatus::Skipped);
    let total_cost_micros = sum_cost_micros(results);
    let total_cost_cents = total_cost_micros
        .map(crate::bench::runner::cost_micros_to_cents_ceil)
        .or_else(|| sum_costs(results));
    let prompt_tokens = sum_tokens(results, |r| r.prompt_tokens);
    let completion_tokens = sum_tokens(results, |r| r.completion_tokens);
    let total_tokens = sum_tokens(results, |r| r.total_tokens);
    let cache_read_tokens = sum_tokens(results, |r| r.cache_read_tokens);
    let cache_write_tokens = sum_tokens(results, |r| r.cache_write_tokens);
    let assistant_turns = sum_u32(results, |r| r.assistant_turns);
    let tool_calls = sum_u32(results, |r| r.tool_calls);
    BenchRun {
        run_id: run_id.into(),
        started_at,
        finished_at: Utc::now(),
        tier,
        tiers: tiers_in_results(results),
        variant: run_config.variant.clone(),
        model_selector: run_config.model_selector.clone(),
        model_key: run_config.model_key.clone(),
        worker_model: run_config
            .worker_model
            .clone()
            .or_else(|| Some(base_worker_config.model.clone())),
        grader_model: run_config.grader_model.clone(),
        prompt_variant: run_config.prompt_variant.clone(),
        prompt_manifest: run_config.prompt_manifest.clone(),
        suite_manifest: run_config.suite_manifest.clone(),
        cases_root: run_config.cases_root.clone(),
        bench_config_path: run_config.bench_config_path.clone(),
        orboros_commit: run_config.orboros_commit.clone(),
        bench_commit: run_config.bench_commit.clone(),
        orboros_dirty: run_config.orboros_dirty,
        bench_dirty: run_config.bench_dirty,
        total,
        passed,
        failed,
        errored,
        skipped,
        config_hash: crate::bench::runner::prompt_hash(
            &run_config.config_hash_input(base_worker_config),
        ),
        total_cost_cents,
        total_cost_micros,
        prompt_tokens,
        completion_tokens,
        total_tokens,
        cache_read_tokens,
        cache_write_tokens,
        assistant_turns,
        tool_calls,
    }
}

fn sum_tokens(results: &[BenchResult], field: impl Fn(&BenchResult) -> Option<u64>) -> Option<u64> {
    let mut measured = false;
    let total = results
        .iter()
        .filter_map(|result| {
            let value = field(result);
            measured |= value.is_some();
            value
        })
        .fold(0u64, u64::saturating_add);
    measured.then_some(total)
}

fn sum_u32(results: &[BenchResult], field: impl Fn(&BenchResult) -> Option<u32>) -> Option<u64> {
    let mut measured = false;
    let total = results
        .iter()
        .filter_map(|result| {
            let value = field(result);
            measured |= value.is_some();
            value
        })
        .map(u64::from)
        .fold(0u64, u64::saturating_add);
    measured.then_some(total)
}

fn sum_costs(results: &[BenchResult]) -> Option<u64> {
    results
        .iter()
        .filter_map(|r| r.cost_cents)
        .fold(None, |sum: Option<u64>, cost| {
            Some(sum.unwrap_or(0).saturating_add(cost))
        })
}

fn sum_cost_micros(results: &[BenchResult]) -> Option<u64> {
    results
        .iter()
        .filter_map(|result| result.cost_micros)
        .fold(None, |sum, cost| {
            Some(sum.unwrap_or(0).saturating_add(cost))
        })
}

fn format_cost(micros: Option<u64>, cents: Option<u64>) -> String {
    if let Some(micros) = micros {
        let dollars = micros / 1_000_000;
        let remainder = micros % 1_000_000;
        format!("${dollars}.{remainder:06}")
    } else if let Some(cents) = cents {
        format!("${}.{:02}", cents / 100, cents % 100)
    } else {
        "-".into()
    }
}

fn count_status(results: &[BenchResult], status: BenchStatus) -> u32 {
    u32::try_from(results.iter().filter(|r| r.status == status).count()).unwrap_or(u32::MAX)
}

fn common_tier(results: &[BenchResult]) -> Option<BenchTier> {
    let first = results.first()?.tier;
    results.iter().all(|r| r.tier == first).then_some(first)
}

fn tiers_in_results(results: &[BenchResult]) -> Vec<BenchTier> {
    let mut tiers = Vec::new();
    for result in results {
        if !tiers.contains(&result.tier) {
            tiers.push(result.tier);
        }
    }
    tiers
}

fn print_run_summary(r: &BenchRun) {
    println!(
        "run={id}  started={when}  tier={tier}  status={status}  variant={variant}",
        id = r.run_id,
        when = r.started_at.to_rfc3339(),
        tier = run_tier_label(r),
        status = run_status_label(r),
        variant = r.variant.as_deref().unwrap_or("-"),
    );
    println!(
        "  results: pass={passed} fail={failed} error={errored} skipped={skipped} total={total} cost={cost} turns={turns} tools={tools} tokens_in={input} tokens_out={output} cache_r={cache_read} cache_w={cache_write}",
        passed = r.passed,
        failed = r.failed,
        errored = r.errored,
        skipped = r.skipped,
        total = r.total,
        cost = format_cost(r.total_cost_micros, r.total_cost_cents),
        input = r
            .prompt_tokens
            .map_or_else(|| "-".to_string(), |tokens| tokens.to_string()),
        output = r
            .completion_tokens
            .map_or_else(|| "-".to_string(), |tokens| tokens.to_string()),
        cache_read = r
            .cache_read_tokens
            .map_or_else(|| "-".into(), |v| v.to_string()),
        cache_write = r
            .cache_write_tokens
            .map_or_else(|| "-".into(), |v| v.to_string()),
        turns = r
            .assistant_turns
            .map_or_else(|| "-".into(), |v| v.to_string()),
        tools = r.tool_calls.map_or_else(|| "-".into(), |v| v.to_string()),
    );
    println!(
        "  models: worker={worker} grader={grader} selector={selector} key={key}",
        worker = r.worker_model.as_deref().unwrap_or("-"),
        grader = r.grader_model.as_deref().unwrap_or("-"),
        selector = r.model_selector.as_deref().unwrap_or("-"),
        key = r.model_key.as_deref().unwrap_or("-"),
    );
    if r.model_selector.is_some()
        || r.model_key.is_some()
        || r.prompt_variant.is_some()
        || r.suite_manifest.is_some()
        || r.cases_root.is_some()
        || r.bench_config_path.is_some()
        || r.orboros_commit.is_some()
        || r.bench_commit.is_some()
    {
        println!(
            "  metadata: suite={suite} prompt={prompt} cases={cases} bench_config={bench_config} orboros_commit={orboros_commit} bench_commit={bench_commit} orboros_dirty={orboros_dirty} bench_dirty={bench_dirty} config={config}",
            suite = r.suite_manifest.as_ref().map_or("-", |suite| short_hash(&suite.fingerprint)),
            prompt = r.prompt_variant.as_deref().unwrap_or("-"),
            cases = r.cases_root.as_deref().unwrap_or("-"),
            bench_config = r.bench_config_path.as_deref().unwrap_or("-"),
            orboros_commit = short_commit(r.orboros_commit.as_deref()),
            bench_commit = short_commit(r.bench_commit.as_deref()),
            orboros_dirty = r
                .orboros_dirty
                .map_or_else(|| "-".to_string(), |value| value.to_string()),
            bench_dirty = r
                .bench_dirty
                .map_or_else(|| "-".to_string(), |value| value.to_string()),
            config = r.config_hash,
        );
    }
}

fn run_tier_label(r: &BenchRun) -> String {
    if !r.tiers.is_empty() {
        return r
            .tiers
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
    }
    r.tier
        .map_or_else(|| "mixed".to_string(), |tier| tier.to_string())
}

fn run_status_label(r: &BenchRun) -> &'static str {
    if r.total == 0 || r.skipped == r.total {
        "skipped"
    } else if r.errored > 0 {
        "error"
    } else if r.failed > 0 {
        "fail"
    } else if r.passed == r.total {
        "pass"
    } else {
        "mixed"
    }
}

fn short_commit(commit: Option<&str>) -> &str {
    commit.and_then(|c| c.get(..12)).unwrap_or("-")
}

fn short_hash(value: &str) -> &str {
    value.get(..12).unwrap_or(value)
}

fn warn_on_run_metadata_drift(a: Option<&BenchRun>, b: Option<&BenchRun>) {
    let Some(a) = a else { return };
    let Some(b) = b else { return };
    let mut drift = Vec::new();
    if a.worker_model != b.worker_model {
        drift.push("worker model");
    }
    if a.grader_model != b.grader_model {
        drift.push("grader model");
    }
    if a.config_hash != b.config_hash {
        drift.push("execution config");
    }
    if a.prompt_variant != b.prompt_variant {
        drift.push("prompt variant");
    }
    if a.cases_root != b.cases_root {
        drift.push("cases root");
    }
    if a.config_hash != b.config_hash {
        drift.push("config hash");
    }
    if !drift.is_empty() {
        eprintln!("warning: run metadata differs: {}", drift.join(", "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::case::{BenchExpected, BenchResourceGuidance, BenchResourceThreshold};
    use crate::bench::store::BenchResult;
    use chrono::Utc;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    fn sample_result(case_id: &str, run_id: &str, status: BenchStatus) -> BenchResult {
        BenchResult {
            case_id: case_id.into(),
            run_id: run_id.into(),
            tier: BenchTier::T1,
            status,
            score: if status == BenchStatus::Pass {
                1.0
            } else {
                0.0
            },
            process_score: None,
            process_annotations: Vec::new(),
            resource_guidance: None,
            latency_ms: 100,
            model_latency_ms: None,
            tool_latency_ms: None,
            total_latency_ms: None,
            cost_cents: None,
            cost_micros: None,
            iterations: 1,
            assistant_turns: None,
            tool_calls: None,
            prompt_tokens: None,
            completion_tokens: None,
            total_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            worker_model: "m".into(),
            prompt_hash: "h1".into(),
            system_prompt_hash: None,
            system_prompt_source: None,
            confidence: None,
            output: None,
            error: None,
        }
    }

    fn sample_run(run_id: &str) -> BenchRun {
        BenchRun {
            run_id: run_id.into(),
            started_at: Utc::now(),
            finished_at: Utc::now(),
            tier: Some(BenchTier::T1),
            tiers: vec![BenchTier::T1],
            variant: None,
            model_selector: None,
            model_key: None,
            worker_model: Some("mock/test".into()),
            grader_model: None,
            prompt_variant: None,
            prompt_manifest: None,
            suite_manifest: None,
            cases_root: None,
            bench_config_path: None,
            orboros_commit: None,
            bench_commit: None,
            orboros_dirty: None,
            bench_dirty: None,
            total: 3,
            passed: 2,
            failed: 1,
            errored: 0,
            skipped: 0,
            config_hash: "h".into(),
            total_cost_cents: None,
            total_cost_micros: None,
            prompt_tokens: None,
            completion_tokens: None,
            total_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            assistant_turns: None,
            tool_calls: None,
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn parallel_case_jobs_overlap_and_persist_in_selector_order() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("slow-worker.sh");
        std::fs::write(
            &script,
            r#"#!/bin/sh
while IFS= read -r line; do
  type=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['type'])")
  id=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['id'])")
  case "$type" in
    init) echo '{"type":"init_ok","id":"'"$id"'","session_id":"s","protocol_version":"0.4.0"}' ;;
    send) sleep 0.10; echo '{"type":"result","id":"'"$id"'","status":"ok","response":"ok","tool_calls_made":[],"iterations":1}' ;;
    shutdown) echo '{"type":"shutdown_ok","id":"'"$id"'"}'; exit 0 ;;
  esac
done
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();

        let make_case = |id: &str, selector: &str| BenchCase {
            id: id.into(),
            tier: BenchTier::T1,
            name: id.into(),
            description: "test".into(),
            prompt: "reply ok".into(),
            expected: BenchExpected::Exact { text: "ok".into() },
            runner: None,
            timeout_s: Some(10),
            max_iterations: Some(1),
            max_cost_cents: 100,
            tool_policy: None,
            process: None,
            resource_guidance: None,
            selector: selector.into(),
            case_dir: dir.path().to_path_buf(),
            fixture_dir: None,
            test_overlay_dir: None,
        };
        let cases = vec![make_case("case-b", "t1.002"), make_case("case-a", "t1.001")];
        let store = BenchStore::new(dir.path().join("results"));
        let worker_config = WorkerConfig {
            command: "bash".into(),
            args: vec![script.to_string_lossy().into()],
            cwd: None,
            env: vec![],
            model: "mock/parallel".into(),
            system_prompt: String::new(),
            tools: vec![],
            max_iterations: Some(1),
            init_timeout: None,
            send_timeout: None,
            shutdown_timeout: None,
            task_id: None,
            worker_id: None,
            runtime: None,
            routing: None,
        };
        let request = BenchRunRequest {
            bench_root: dir.path(),
            store: &store,
            tier: Some(BenchTier::T1),
            case_id: None,
            worker_config: &worker_config,
            no_budget: false,
            jobs: 2,
            timeout_s: Some(10),
            max_iterations: Some(1),
            run_config: &BenchRunConfig::default(),
            prompt_set: None,
        };
        let labels = HashMap::from([
            ("case-b".into(), ("t1.002".into(), "case-b".into())),
            ("case-a".into(), ("t1.001".into(), "case-a".into())),
        ]);

        let started = Instant::now();
        cmd_bench_run_parallel(
            request,
            cases,
            labels,
            HashMap::new(),
            BenchRunConfig::default(),
        )
        .await
        .unwrap();
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap();
        let run_id = store.read_runs().unwrap().pop().unwrap().run_id;
        let results = store.read_results(&run_id).unwrap();
        let summed_case_latency = results.iter().map(|result| result.latency_ms).sum::<u64>();
        assert!(
            summed_case_latency > elapsed_ms.saturating_mul(3) / 2,
            "expected overlapping case work: summed={summed_case_latency}ms, wall={elapsed_ms}ms"
        );
        assert_eq!(
            results
                .iter()
                .map(|result| result.case_id.as_str())
                .collect::<Vec<_>>(),
            ["case-a", "case-b"],
        );
    }

    #[test]
    fn resource_target_status_uses_the_highest_exceeded_threshold() {
        let guidance = BenchResourceGuidance {
            cost_micros: None,
            input_tokens: Some(BenchResourceThreshold {
                target: 30,
                investigate: 50,
            }),
            cache_read_tokens: Some(BenchResourceThreshold {
                target: 20,
                investigate: 40,
            }),
            elapsed_ms: None,
            assistant_turns: None,
            tool_calls: Some(BenchResourceThreshold {
                target: 10,
                investigate: 20,
            }),
        };
        let mut result = sample_result("case", "run", BenchStatus::Pass);
        result.prompt_tokens = Some(30);
        result.cache_read_tokens = Some(20);
        result.tool_calls = Some(10);
        assert_eq!(
            resource_target_status(&result, &guidance),
            Some(ResourceTargetStatus::Under)
        );

        result.tool_calls = Some(11);
        assert_eq!(
            resource_target_status(&result, &guidance),
            Some(ResourceTargetStatus::Over)
        );

        result.cache_read_tokens = Some(41);
        assert_eq!(
            resource_target_status(&result, &guidance),
            Some(ResourceTargetStatus::Investigate)
        );

        result.cache_read_tokens = None;
        assert_eq!(resource_target_status(&result, &guidance), None);
    }

    #[test]
    fn suite_manifest_changes_when_a_selected_case_changes() {
        let dir = tempfile::tempdir().unwrap();
        write_case(dir.path(), BenchTier::T1, "alpha");
        let cases = load_all(dir.path()).unwrap();
        let original = build_suite_manifest(&cases, None).unwrap();

        let case_toml = dir.path().join("t1/001-alpha/case.toml");
        std::fs::write(&case_toml, "id = \"alpha\"\ntier = \"t1\"\nname = \"changed\"\ndescription = \"d\"\nprompt = \"p\"\n[expected]\nkind = \"exact\"\ntext = \"x\"\n").unwrap();
        let changed = build_suite_manifest(&load_all(dir.path()).unwrap(), None).unwrap();

        assert_ne!(original.fingerprint, changed.fingerprint);
    }

    fn write_case(dir: &Path, tier: BenchTier, id: &str) {
        let case_dir = dir.join(tier.as_str()).join(format!("001-{id}"));
        std::fs::create_dir_all(&case_dir).unwrap();
        if tier == BenchTier::T2 {
            std::fs::create_dir(case_dir.join("fixture")).unwrap();
        }
        std::fs::write(
            case_dir.join("case.toml"),
            format!(
                r#"
id = "{id}"
tier = "{tier_str}"
name = "{id}"
description = "d"
prompt = "p"
[expected]
kind = "exact"
text = "x"
"#,
                tier_str = tier.as_str(),
            ),
        )
        .unwrap();
    }

    #[test]
    fn elapsed_display_is_human_readable() {
        assert_eq!(format_elapsed_ms(42), "42ms");
        assert_eq!(format_elapsed_ms(6_349), "6.3s");
        assert_eq!(format_elapsed_ms(117_248), "1m 57s");
        assert_eq!(format_elapsed_ms(3_661_000), "1h 01m 01s");
    }

    #[test]
    fn effective_speedup_uses_case_work_over_wall_clock() {
        assert_eq!(effective_speedup(500, 0), None);
        assert_eq!(effective_speedup(600, 200), Some(3.0));
    }

    #[test]
    fn failure_causes_distinguish_provider_and_test_failures() {
        let mut provider = sample_result("provider", "run", BenchStatus::Error);
        provider.error = Some("[worker_error] error reading streaming response body".into());
        let mut tests = sample_result("tests", "run", BenchStatus::Fail);
        tests.error = Some("tests_pass command failed".into());

        assert_eq!(
            failure_causes(&[provider, tests]),
            vec![("provider_api", 1), ("tests_failed", 1)]
        );
    }

    // ── cmd_bench_list ────────────────────────────────────────

    #[test]
    fn list_handles_empty_corpus() {
        let dir = tempfile::tempdir().unwrap();
        cmd_bench_list(dir.path()).unwrap();
    }

    #[test]
    fn list_groups_by_tier() {
        let dir = tempfile::tempdir().unwrap();
        write_case(dir.path(), BenchTier::T1, "a");
        write_case(dir.path(), BenchTier::T2, "b");
        cmd_bench_list(dir.path()).unwrap();
    }

    // ── cmd_bench_show ────────────────────────────────────────

    #[test]
    fn show_errors_on_missing_run() {
        let dir = tempfile::tempdir().unwrap();
        let store = BenchStore::new(dir.path().join("bench"));
        assert!(cmd_bench_show(&store, "nope").is_err());
    }

    #[test]
    fn show_prints_existing_run() {
        let dir = tempfile::tempdir().unwrap();
        let store = BenchStore::new(dir.path().join("bench"));
        store
            .append_result(&sample_result("c", "run-1", BenchStatus::Pass))
            .unwrap();
        store
            .append_run(&BenchRun {
                run_id: "run-1".into(),
                started_at: Utc::now(),
                finished_at: Utc::now(),
                tier: Some(BenchTier::T1),
                tiers: vec![BenchTier::T1],
                variant: None,
                model_selector: None,
                model_key: None,
                worker_model: None,
                grader_model: None,
                prompt_variant: None,
                prompt_manifest: None,
                suite_manifest: None,
                cases_root: None,
                bench_config_path: None,
                orboros_commit: None,
                bench_commit: None,
                orboros_dirty: None,
                bench_dirty: None,
                total: 1,
                passed: 1,
                failed: 0,
                errored: 0,
                skipped: 0,
                config_hash: "h".into(),
                total_cost_cents: None,
                total_cost_micros: None,
                prompt_tokens: None,
                completion_tokens: None,
                total_tokens: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
                assistant_turns: None,
                tool_calls: None,
            })
            .unwrap();
        cmd_bench_show(&store, "run-1").unwrap();
    }

    // ── cmd_bench_compare ─────────────────────────────────────

    #[test]
    fn compare_errors_when_either_run_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = BenchStore::new(dir.path().join("bench"));
        store
            .append_result(&sample_result("c", "run-a", BenchStatus::Pass))
            .unwrap();
        assert!(cmd_bench_compare(&store, "run-a", "run-b").is_err());
        assert!(cmd_bench_compare(&store, "run-x", "run-a").is_err());
    }

    #[test]
    fn compare_runs_with_matching_cases() {
        let dir = tempfile::tempdir().unwrap();
        let store = BenchStore::new(dir.path().join("bench"));
        store
            .append_result(&sample_result("c1", "run-a", BenchStatus::Pass))
            .unwrap();
        store
            .append_result(&sample_result("c1", "run-b", BenchStatus::Fail))
            .unwrap();
        // Should not error.
        cmd_bench_compare(&store, "run-a", "run-b").unwrap();
    }

    #[test]
    fn compare_ignores_case_prompt_hash_drift() {
        let dir = tempfile::tempdir().unwrap();
        let store = BenchStore::new(dir.path().join("bench"));
        let mut a = sample_result("c1", "run-a", BenchStatus::Pass);
        let mut b = sample_result("c1", "run-b", BenchStatus::Pass);
        a.prompt_hash = "h-old".into();
        b.prompt_hash = "h-new".into();
        store.append_result(&a).unwrap();
        store.append_result(&b).unwrap();
        cmd_bench_compare(&store, "run-a", "run-b").unwrap();
    }

    // ── cmd_bench_list_runs ───────────────────────────────────

    #[test]
    fn list_runs_handles_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = BenchStore::new(dir.path().join("bench"));
        cmd_bench_list_runs(&store).unwrap();
    }

    #[test]
    fn run_status_label_marks_all_error_runs_as_error() {
        let mut run = sample_run("run-error");
        run.total = 12;
        run.passed = 0;
        run.failed = 0;
        run.errored = 12;
        run.skipped = 0;

        assert_eq!(run_status_label(&run), "error");
    }

    #[test]
    fn run_status_label_marks_pass_fail_and_partial_error() {
        let mut run = sample_run("run-pass");
        run.total = 3;
        run.passed = 3;
        run.failed = 0;
        run.errored = 0;
        run.skipped = 0;
        assert_eq!(run_status_label(&run), "pass");

        run.passed = 2;
        run.failed = 1;
        assert_eq!(run_status_label(&run), "fail");

        run.errored = 1;
        assert_eq!(run_status_label(&run), "error");
    }

    #[test]
    fn run_tier_label_lists_multiple_tiers() {
        let mut run = sample_run("run-mixed");
        run.tier = None;
        run.tiers = vec![BenchTier::T1, BenchTier::T2];

        assert_eq!(run_tier_label(&run), "t1,t2");
    }

    // ── corpus integration with cmd_bench_list ────────────────

    #[test]
    fn case_loader_round_trips_through_listing() {
        let dir = tempfile::tempdir().unwrap();
        write_case(dir.path(), BenchTier::T1, "alpha");
        let cases = load_all(dir.path()).unwrap();
        assert_eq!(cases.len(), 1);
        let _ = BenchExpected::Exact { text: "x".into() }; // ensure use of BenchExpected suppresses unused warning
    }
}

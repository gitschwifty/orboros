//! CLI command handlers for `orboros bench`.
//!
//! Each handler takes plain arguments and a store/corpus root —
//! main.rs is the only place that talks to clap. Print-and-return
//! style mirrors the rest of the CLI surface in `orb_cmd` and
//! `hooks::cmd`.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};

use crate::bench::case::{BenchCase, BenchTier, DEFAULT_TIMEOUT_S, load_all, load_tier};
use crate::bench::prompts::BenchPromptSet;
use crate::bench::runner::{
    BenchRunConfig, RunOptions, effective_timeout_s, is_fatal_worker_error, run_t1_with_run_id,
    timeout_bench_result,
};
use crate::bench::store::{
    BenchDispatchRecord, BenchPromptRecord, BenchResult, BenchRun, BenchStatus, BenchStore,
};
use crate::worker::process::WorkerConfig;

pub struct BenchRunRequest<'a> {
    pub bench_root: &'a Path,
    pub store: &'a BenchStore,
    pub tier: Option<BenchTier>,
    pub case_id: Option<&'a str>,
    pub worker_config: &'a WorkerConfig,
    pub no_budget: bool,
    pub timeout_s: Option<u32>,
    pub max_iterations: Option<u32>,
    pub run_config: &'a BenchRunConfig,
    pub prompt_set: Option<&'a BenchPromptSet>,
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

    let had_t1 = !t1.is_empty();
    let had_other = !other.is_empty();
    if had_t1 {
        let summary = run_t1_with_run_id(
            &t1,
            req.worker_config,
            req.store,
            &opts,
            req.run_config,
            run_id.clone(),
        )
        .await?;
        summary_run_id = Some(summary.run_id);
        all_results.extend(summary.results);
        if !had_other {
            println!("\n== summary ==");
            print_run_summary(&summary.summary);
        }
        if all_results.iter().any(is_fatal_worker_error) {
            eprintln!("stopping benchmark run after fatal worker/provider error");
            print_result_table(&all_results, Some(&case_labels));
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
            req.run_config,
            req.worker_config,
        );
        req.store.append_run(&run)?;
        completed_run = Some(run.clone());
        if !had_t1 || had_other {
            println!("\n== summary ==");
            print_run_summary(&run);
        }
    }

    print_result_table(&all_results, Some(&case_labels));
    if let Some(run) = completed_run.as_ref() {
        print_run_completion(run, &all_results);
    }
    if let Some(ref id) = summary_run_id {
        println!("\nRun id: {id}");
    }
    Ok(())
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
    print_result_table(&results, None);
    if let Some(run) = store.read_runs()?.into_iter().find(|r| r.run_id == run_id) {
        println!("\n== summary ==");
        print_run_summary(&run);
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
    let summaries = summarize_dispatches(&records);
    println!(
        "{case:<24} {kind:<18} {done:>4} {failed:>6} {error:>5} {retry:>5} {turns:>6} {tools:>6} {input:>9} {output:>8} {cache:>8} {cost:>10} {wall:>9}",
        case = "case",
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
    );
    for ((case_id, kind), summary) in summaries {
        println!(
            "{case_id:<24} {kind:<18} {done:>4} {failed:>6} {error:>5} {retries:>5} {turns:>6} {tools:>6} {input:>9} {output:>8} {cache_read:>8} {cost:>10} {wall:>9}",
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
        );
    }
    print_prompt_context_report(store, run_id, case_id)?;
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
    for record in prompts {
        println!(
            "== {} {} [{}] ==",
            record.case_id, record.prompt.orb_id, record.prompt.dispatch_kind
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
    println!(
        "{case:<24} {kind:<18} {count:>5} {tokens:>9} {system:>7} {user:>7} {context:>7} {overhead:>8} {current:>7} {parent:>7} {siblings:>8} {children:>8} {deps:>7}",
        case = "case",
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
    );
    for ((case_id, kind), summary) in summarize_prompts(&prompts) {
        println!(
            "{case_id:<24} {kind:<18} {count:>5} {tokens:>9} {system:>7} {user:>7} {context:>7} {overhead:>8} {current:>7} {parent:>7} {siblings:>8} {children:>8} {deps:>7}",
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

/// Compares two saved runs side by side. Highlights cases whose
/// status changed and warns when the case or resolved system prompt
/// hash differs (direct comparison may be misleading).
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

    let case_width = case_id_width(a.iter().chain(&b).map(|r| r.case_id.as_str()));
    let a_width = run_a.len().max(10);
    let b_width = run_b.len().max(10);
    println!(
        "{case:<case_width$} {a_status:<a_width$} {b_status:<b_width$} change",
        case = "case",
        a_status = run_a,
        b_status = run_b,
    );
    let mut prompt_changed = 0;
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
            let prompt_note = if r.prompt_hash != rb.prompt_hash {
                prompt_changed += 1;
                "  ⚠ case prompt changed"
            } else if r.system_prompt_hash.as_ref().zip(rb.system_prompt_hash.as_ref()).is_some_and(
                |(a, b)| a != b,
            ) {
                prompt_changed += 1;
                "  ⚠ system prompt changed"
            } else if r.system_prompt_hash.is_some() != rb.system_prompt_hash.is_some() {
                "  (system prompt hash unavailable in one run)"
            } else {
                ""
            };
            println!(
                "{case:<case_width$} {a:<a_width$?} {b:<b_width$?} {change}{prompt_note}",
                case = r.case_id,
                a = r.status,
                b = rb.status,
            );
        } else {
            only_in_a += 1;
            println!(
                "{case:<case_width$} {a:<a_width$?} {b:<b_width$} only in {run_a}",
                case = r.case_id,
                a = r.status,
                b = "-",
            );
        }
    }
    for rb in &b {
        if !a.iter().any(|ra| ra.case_id == rb.case_id) {
            only_in_b += 1;
            println!(
                "{case:<case_width$} {a:<a_width$} {b:<b_width$?} only in {run_b}",
                case = rb.case_id,
                a = "-",
                b = rb.status,
            );
        }
    }

    println!("\nimproved: {improved}, regressed: {regressed}, prompt-changed: {prompt_changed}");
    if prompt_changed > 0 {
        eprintln!(
            "warning: {prompt_changed} case(s) had a different prompt between runs - \
             direct status comparison may be misleading."
        );
    }
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
pub fn cmd_bench_list_runs(store: &BenchStore) -> anyhow::Result<()> {
    let mut runs = store.read_runs()?;
    runs.sort_by_key(|run| std::cmp::Reverse(run.started_at));
    if runs.is_empty() {
        println!("No runs recorded.");
        return Ok(());
    }
    for r in &runs {
        print_run_summary(r);
    }
    println!("\n{} run(s)", runs.len());
    Ok(())
}

fn print_result_table(
    results: &[crate::bench::store::BenchResult],
    case_labels: Option<&HashMap<String, (String, String)>>,
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
        "{selector:<selector_width$}  {name:<name_width$}  {status:<8}  {score:>5}  {process:>7}  {elapsed:>9}  {cost:>8}  {turns:>5}  {tools:>5}  {input:>8}  {output:>8}  {cache_r:>8}  {cache_w:>8}  {conf:>5}",
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
        println!(
            "{selector:<selector_width$}  {name:<name_width$}  {status:<8}  {score:>5.2}  {process:>7}  {elapsed:>9}  {cost:>8}  {turns:>5}  {tools:>5}  {input:>8}  {output:>8}  {cache_read:>8}  {cache_write:>8}  {conf:>5}",
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

fn print_run_completion(run: &BenchRun, results: &[BenchResult]) {
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

fn case_id_width<'a>(ids: impl Iterator<Item = &'a str>) -> usize {
    ids.map(str::len).max().unwrap_or(4).max(24)
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
        || r.cases_root.is_some()
        || r.bench_config_path.is_some()
        || r.orboros_commit.is_some()
        || r.bench_commit.is_some()
    {
        println!(
            "  metadata: prompt={prompt} cases={cases} bench_config={bench_config} orboros_commit={orboros_commit} bench_commit={bench_commit} orboros_dirty={orboros_dirty} bench_dirty={bench_dirty} config={config}",
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
    use crate::bench::case::BenchExpected;
    use crate::bench::store::BenchResult;
    use chrono::Utc;

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
    fn compare_detects_prompt_hash_drift() {
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

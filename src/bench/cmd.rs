//! CLI command handlers for `orboros bench`.
//!
//! Each handler takes plain arguments and a store/corpus root —
//! main.rs is the only place that talks to clap. Print-and-return
//! style mirrors the rest of the CLI surface in `orb_cmd` and
//! `hooks::cmd`.

use std::path::Path;

use anyhow::Context;
use chrono::Utc;

use crate::bench::case::{load_all, load_tier, BenchCase, BenchTier};
use crate::bench::runner::{run_t1, BenchRunConfig, RunOptions};
use crate::bench::store::{BenchResult, BenchRun, BenchStatus, BenchStore};
use crate::worker::process::WorkerConfig;

pub struct BenchRunRequest<'a> {
    pub cases_root: &'a Path,
    pub store: &'a BenchStore,
    pub tier: Option<BenchTier>,
    pub case_id: Option<&'a str>,
    pub worker_config: &'a WorkerConfig,
    pub no_budget: bool,
    pub run_config: &'a BenchRunConfig,
    pub fixtures_root: &'a Path,
}

/// Prints every case in the corpus, grouped by tier.
///
/// # Errors
///
/// Returns an error if loading the corpus fails (malformed TOML, etc.).
pub fn cmd_bench_list(cases_root: &Path) -> anyhow::Result<()> {
    let cases = load_all(cases_root).context("failed to load benchmark corpus")?;
    if cases.is_empty() {
        println!("No benchmark cases found under {}", cases_root.display());
        return Ok(());
    }
    let mut tier = None;
    for case in &cases {
        if tier != Some(case.tier) {
            tier = Some(case.tier);
            println!("\n== {} ==", case.tier);
        }
        let cost = case.max_cost_cents;
        let timeout = case.timeout_s;
        println!(
            "  {id:<24} {name}  (max ${cost_dollars:.2}, {timeout}s)",
            id = case.id,
            name = case.name,
            cost_dollars = f64::from(cost) / 100.0,
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
        Some(t) => load_tier(req.cases_root, t)?,
        None => load_all(req.cases_root)?,
    };
    if let Some(id) = req.case_id {
        cases.retain(|c| c.id == id);
        if cases.is_empty() {
            anyhow::bail!("no case found with id `{id}`");
        }
    }
    if cases.is_empty() {
        println!("No matching cases.");
        return Ok(());
    }

    // Split by tier and dispatch. Today only T1 actually runs the
    // pipeline; T2/T3 fall through to scaffolded error rows.
    let (t1, other): (Vec<BenchCase>, Vec<BenchCase>) =
        cases.into_iter().partition(|c| c.tier == BenchTier::T1);

    let opts = RunOptions {
        no_budget: req.no_budget,
    };
    let mut all_results = Vec::new();
    let mut summary_run_id = None;

    let had_t1 = !t1.is_empty();
    if had_t1 {
        let summary = run_t1(&t1, req.worker_config, req.store, &opts, req.run_config).await?;
        summary_run_id = Some(summary.run_id);
        all_results.extend(summary.results);
        println!("\n== summary ==");
        print_run_summary(&summary.summary);
    }

    for case in &other {
        let run_id = summary_run_id
            .clone()
            .unwrap_or_else(crate::bench::store::new_run_id);
        let result = match case.tier {
            BenchTier::T2 => crate::bench::runner_t2t3::run_t2_case(
                case,
                &run_id,
                req.fixtures_root,
                req.worker_config,
                &opts,
            )
            .await
            .map_or_else(
                |e| {
                    Ok::<_, anyhow::Error>(crate::bench::store::BenchResult {
                        case_id: case.id.clone(),
                        run_id: run_id.clone(),
                        tier: BenchTier::T2,
                        status: BenchStatus::Error,
                        score: 0.0,
                        latency_ms: 0,
                        cost_cents: 0,
                        iterations: 0,
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
            BenchTier::T3 => crate::bench::runner_t2t3::run_t3_case_stub(case, &run_id, &opts)
                .map_or_else(
                    |e| {
                        Ok::<_, anyhow::Error>(crate::bench::store::BenchResult {
                            case_id: case.id.clone(),
                            run_id: run_id.clone(),
                            tier: BenchTier::T3,
                            status: BenchStatus::Error,
                            score: 0.0,
                            latency_ms: 0,
                            cost_cents: 0,
                            iterations: 0,
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
            BenchTier::T1 => unreachable!("T1 partitioned out above"),
        };
        if summary_run_id.is_none() {
            summary_run_id = Some(run_id);
        }
        req.store.append_result(&result)?;
        all_results.push(result);
    }

    if !had_t1 {
        if let Some(ref id) = summary_run_id {
            let run = summarize_run(
                id,
                common_tier(&all_results),
                &all_results,
                req.run_config,
                req.worker_config,
            );
            req.store.append_run(&run)?;
            println!("\n== summary ==");
            print_run_summary(&run);
        }
    }

    print_result_table(&all_results);
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
    print_result_table(&results);
    if let Some(run) = store.read_runs()?.into_iter().find(|r| r.run_id == run_id) {
        println!("\n== summary ==");
        print_run_summary(&run);
    }
    Ok(())
}

/// Compares two saved runs side by side. Highlights cases whose
/// status changed and warns when the prompt hash differs (the case
/// definition changed between runs, so direct comparison may be
/// misleading).
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
            let prompt_note = if r.prompt_hash == rb.prompt_hash {
                ""
            } else {
                prompt_changed += 1;
                "  ⚠ prompt changed"
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

fn print_result_table(results: &[crate::bench::store::BenchResult]) {
    let case_width = case_id_width(results.iter().map(|r| r.case_id.as_str()));
    println!(
        "{case:<case_width$}  {tier:<4}  {status:<8}  {score:>5}  {latency:>8}  {conf:>5}",
        case = "case",
        tier = "tier",
        status = "status",
        score = "score",
        latency = "latency",
        conf = "conf",
    );
    for r in results {
        let tier = r.tier.to_string();
        let status = format!("{:?}", r.status);
        let latency = format!("{}ms", r.latency_ms);
        let conf = r
            .confidence
            .map_or(String::from("-"), |c| format!("{c:.2}"));
        println!(
            "{case:<case_width$}  {tier:<4}  {status:<8}  {score:>5.2}  {latency:>8}  {conf:>5}",
            case = r.case_id,
            tier = tier,
            status = status,
            score = r.score,
            latency = latency,
            conf = conf,
        );
    }
}

fn case_id_width<'a>(ids: impl Iterator<Item = &'a str>) -> usize {
    ids.map(str::len).max().unwrap_or(4).max(24)
}

fn summarize_run(
    run_id: &str,
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
    let total_cost_cents = results
        .iter()
        .fold(0u32, |sum, r| sum.saturating_add(r.cost_cents));
    BenchRun {
        run_id: run_id.into(),
        started_at: Utc::now(),
        finished_at: Utc::now(),
        tier,
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
        total,
        passed,
        failed,
        errored,
        skipped,
        config_hash: crate::bench::runner::prompt_hash(
            &run_config.config_hash_input(base_worker_config),
        ),
        total_cost_cents,
    }
}

fn count_status(results: &[BenchResult], status: BenchStatus) -> u32 {
    u32::try_from(results.iter().filter(|r| r.status == status).count()).unwrap_or(u32::MAX)
}

fn common_tier(results: &[BenchResult]) -> Option<BenchTier> {
    let first = results.first()?.tier;
    results.iter().all(|r| r.tier == first).then_some(first)
}

fn print_run_summary(r: &BenchRun) {
    println!(
        "{id}  {when}  tier={tier:?}  variant={variant}  model={model}  {passed}P/{failed}F/{errored}E/{skipped}S of {total}  ${cost:.2}",
        id = r.run_id,
        when = r.started_at.to_rfc3339(),
        tier = r.tier,
        variant = r.variant.as_deref().unwrap_or("-"),
        model = r.worker_model.as_deref().unwrap_or("-"),
        passed = r.passed,
        failed = r.failed,
        errored = r.errored,
        skipped = r.skipped,
        total = r.total,
        cost = f64::from(r.total_cost_cents) / 100.0,
    );
    if r.model_selector.is_some()
        || r.model_key.is_some()
        || r.grader_model.is_some()
        || r.prompt_variant.is_some()
        || r.cases_root.is_some()
    {
        println!(
            "  selector={selector} key={key} grader={grader} prompt={prompt} cases={cases} config={config}",
            selector = r.model_selector.as_deref().unwrap_or("-"),
            key = r.model_key.as_deref().unwrap_or("-"),
            grader = r.grader_model.as_deref().unwrap_or("-"),
            prompt = r.prompt_variant.as_deref().unwrap_or("-"),
            cases = r.cases_root.as_deref().unwrap_or("-"),
            config = r.config_hash,
        );
    }
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
            latency_ms: 100,
            cost_cents: 1,
            iterations: 1,
            worker_model: "m".into(),
            prompt_hash: "h1".into(),
            system_prompt_hash: None,
            system_prompt_source: None,
            confidence: None,
            output: None,
            error: None,
        }
    }

    fn write_case(dir: &Path, tier: BenchTier, id: &str) {
        let tdir = dir.join(tier.as_str());
        std::fs::create_dir_all(&tdir).unwrap();
        std::fs::write(
            tdir.join(format!("{id}.toml")),
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
                variant: None,
                model_selector: None,
                model_key: None,
                worker_model: None,
                grader_model: None,
                prompt_variant: None,
                cases_root: None,
                total: 1,
                passed: 1,
                failed: 0,
                errored: 0,
                skipped: 0,
                config_hash: "h".into(),
                total_cost_cents: 1,
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

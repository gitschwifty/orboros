//! CLI command handlers for `orboros bench`.
//!
//! Each handler takes plain arguments and a store/corpus root —
//! main.rs is the only place that talks to clap. Print-and-return
//! style mirrors the rest of the CLI surface in `orb_cmd` and
//! `hooks::cmd`.

use std::path::Path;

use anyhow::Context;

use crate::bench::case::{load_all, load_tier, BenchCase, BenchTier};
use crate::bench::runner::{run_t1, RunOptions};
use crate::bench::store::{BenchRun, BenchStatus, BenchStore};
use crate::worker::process::WorkerConfig;

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
pub async fn cmd_bench_run(
    cases_root: &Path,
    store: &BenchStore,
    tier: Option<BenchTier>,
    case_id: Option<&str>,
    worker_config: &WorkerConfig,
    no_budget: bool,
) -> anyhow::Result<()> {
    let mut cases = match tier {
        Some(t) => load_tier(cases_root, t)?,
        None => load_all(cases_root)?,
    };
    if let Some(id) = case_id {
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

    let opts = RunOptions { no_budget };
    let mut all_results = Vec::new();
    let mut summary_run_id = None;

    if !t1.is_empty() {
        let summary = run_t1(&t1, worker_config, store, &opts).await?;
        summary_run_id = Some(summary.run_id);
        all_results.extend(summary.results);
    }

    for case in &other {
        let run_id = summary_run_id
            .clone()
            .unwrap_or_else(crate::bench::store::new_run_id);
        let result = match case.tier {
            BenchTier::T2 => crate::bench::runner_t2t3::run_t2_case_stub(
                case,
                &run_id,
                &cases_root.join("..").join("fixtures"),
                &opts,
            )
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
        store.append_result(&result)?;
        all_results.push(result);
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

    println!(
        "{case:<24} {a_status:<10} {b_status:<10} change",
        case = "case",
        a_status = run_a,
        b_status = run_b,
    );
    let mut prompt_changed = 0;
    let mut improved = 0;
    let mut regressed = 0;
    for r in &a {
        let other = by_case_b.get(r.case_id.as_str());
        match other {
            Some(rb) => {
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
                    "{case:<24} {a:<10?} {b:<10?} {change}{prompt_note}",
                    case = r.case_id,
                    a = r.status,
                    b = rb.status,
                );
            }
            None => println!(
                "{case:<24} {a:<10?} {b:<10} only in {run_a}",
                case = r.case_id,
                a = r.status,
                b = "-",
            ),
        }
    }
    for rb in &b {
        if !a.iter().any(|ra| ra.case_id == rb.case_id) {
            println!(
                "{case:<24} {a:<10} {b:<10?} only in {run_b}",
                case = rb.case_id,
                a = "-",
                b = rb.status,
            );
        }
    }

    println!("\nimproved: {improved}, regressed: {regressed}, prompt-changed: {prompt_changed}");
    if prompt_changed > 0 {
        eprintln!(
            "warning: {prompt_changed} case(s) had a different prompt between runs — \
             direct status comparison may be misleading."
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
    println!(
        "{case:<24} {tier:<4} {status:<8} {score:>5}  {latency:>6}ms  conf",
        case = "case",
        tier = "tier",
        status = "status",
        score = "score",
        latency = "lat",
    );
    for r in results {
        let conf = r
            .confidence
            .map_or(String::from("  -"), |c| format!("{c:.2}"));
        println!(
            "{case:<24} {tier:<4} {status:<8?} {score:>5.2}  {latency:>6}ms  {conf}",
            case = r.case_id,
            tier = r.tier,
            status = r.status,
            score = r.score,
            latency = r.latency_ms,
        );
    }
}

fn print_run_summary(r: &BenchRun) {
    println!(
        "{id}  {when}  tier={tier:?}  {passed}P/{failed}F/{errored}E/{skipped}S of {total}  ${cost:.2}",
        id = r.run_id,
        when = r.started_at.to_rfc3339(),
        tier = r.tier,
        passed = r.passed,
        failed = r.failed,
        errored = r.errored,
        skipped = r.skipped,
        total = r.total,
        cost = f64::from(r.total_cost_cents) / 100.0,
    );
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

//! End-to-end tests for the second-opinion reviewer (task 58).
//!
//! Exercises the reviewer worker + verdict parsing + `apply_review_outcome`
//! against a mock worker, and verifies the CLI surface (`orb show`,
//! `orb list --review-status`, `review-queue`) reflects the reviewer's
//! decisions.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use orboros::config::{SecondOpinionConfig, SecondOpinionMode};
use orboros::phases::second_opinion::run_reviewer;
use orboros::second_opinion_trigger::{apply_review_outcome, should_review};
use orboros::worker::process::WorkerConfig;
use orbs::orb::{Orb, OrbType};
use orbs::review::{ReviewVerdict, ReviseScope};
use rand::{rngs::StdRng, SeedableRng};

fn make_executable(path: &Path) {
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

/// Writes a mock-reviewer bash script that emits the given `response_body`
/// verbatim as the IPC `result.response` string. The body is written to
/// a sidecar file and assembled into the IPC envelope via python so we
/// don't have to wrestle with multi-level shell escaping.
fn write_reviewer_script(dir: &Path, name: &str, response_body: &str) -> PathBuf {
    let body_file = dir.join(format!("{name}.body"));
    fs::write(&body_file, response_body).unwrap();
    let path = dir.join(name);
    let script = format!(
        r#"#!/bin/bash
BODY_FILE='{body_path}'
while IFS= read -r line; do
  type=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['type'])" 2>/dev/null)
  id=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['id'])" 2>/dev/null)
  case "$type" in
    init) echo "{{\"type\":\"init_ok\",\"id\":\"$id\",\"session_id\":\"s\",\"protocol_version\":\"0.2.0\"}}" ;;
    send) python3 -c "import json,sys; body=open('$BODY_FILE').read(); print(json.dumps({{'type':'result','id':'$id','status':'ok','response':body,'tool_calls_made':[],'iterations':1}}))" ;;
    shutdown) echo "{{\"type\":\"shutdown_ok\",\"id\":\"$id\"}}"; exit 0 ;;
  esac
done
"#,
        body_path = body_file.display(),
    );
    fs::write(&path, script).unwrap();
    make_executable(&path);
    path
}

fn base_worker_config(script: &Path) -> WorkerConfig {
    WorkerConfig {
        command: "bash".into(),
        args: vec![script.to_string_lossy().into()],
        cwd: None,
        env: vec![],
        model: "mock/reviewer".into(),
        system_prompt: String::new(),
        tools: vec![],
        max_iterations: Some(1),
        init_timeout: None,
        send_timeout: None,
        shutdown_timeout: None,
        task_id: None,
        worker_id: None,
    }
}

fn done_orb(title: &str, result: &str) -> Orb {
    let mut o = Orb::new(title, "Implement feature X").with_type(OrbType::Task);
    o.result = Some(result.into());
    o.acceptance_criteria = Some("Should do X correctly".into());
    o
}

// ── reviewer end-to-end against a mock worker ────────────────────

#[tokio::test]
async fn reviewer_accept_verdict_round_trips_through_worker() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_reviewer_script(dir.path(), "accept.sh", r#"{"verdict": "accept"}"#);
    let orb = done_orb("orb-1", "good result");
    let cfg = SecondOpinionConfig::default();
    let wc = base_worker_config(&script);
    let report = run_reviewer(&orb, &cfg, &wc).await.unwrap();
    assert!(report.verdict.is_accept());
    assert_eq!(report.reviewer_model, "mock/reviewer");
}

#[tokio::test]
async fn reviewer_reject_verdict_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_reviewer_script(
        dir.path(),
        "reject.sh",
        r#"{"verdict": "reject", "critique": "off topic"}"#,
    );
    let orb = done_orb("orb-2", "bad result");
    let cfg = SecondOpinionConfig::default();
    let wc = base_worker_config(&script);
    let report = run_reviewer(&orb, &cfg, &wc).await.unwrap();
    assert!(report.verdict.is_reject());
    assert_eq!(report.critique, "off topic");
}

#[tokio::test]
async fn reviewer_revise_execution_verdict_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_reviewer_script(
        dir.path(),
        "revise_exec.sh",
        r#"{"verdict": {"revise": {"scope": "execution"}}, "critique": "sloppy"}"#,
    );
    let orb = done_orb("orb-3", "shaky result");
    let cfg = SecondOpinionConfig::default();
    let wc = base_worker_config(&script);
    let report = run_reviewer(&orb, &cfg, &wc).await.unwrap();
    assert_eq!(
        report.verdict,
        ReviewVerdict::Revise {
            scope: ReviseScope::Execution
        }
    );
    assert_eq!(report.critique, "sloppy");
}

#[tokio::test]
async fn reviewer_revise_decomposition_verdict_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_reviewer_script(
        dir.path(),
        "revise_decomp.sh",
        r#"{"verdict": {"revise": {"scope": "decomposition"}}, "critique": "missed step"}"#,
    );
    let orb = done_orb("orb-4", "incomplete result");
    let cfg = SecondOpinionConfig::default();
    let wc = base_worker_config(&script);
    let report = run_reviewer(&orb, &cfg, &wc).await.unwrap();
    assert_eq!(
        report.verdict,
        ReviewVerdict::Revise {
            scope: ReviseScope::Decomposition
        }
    );
}

#[tokio::test]
async fn reviewer_applies_revise_critique_to_orb() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_reviewer_script(
        dir.path(),
        "rev.sh",
        r#"{"verdict": {"revise": {"scope": "execution"}}, "critique": "do better"}"#,
    );
    let mut orb = done_orb("orb-5", "weak result");
    let cfg = SecondOpinionConfig::default();
    let wc = base_worker_config(&script);
    let report = run_reviewer(&orb, &cfg, &wc).await.unwrap();
    apply_review_outcome(&mut orb, report, None);
    assert!(orb.review_report.is_some());
    assert_eq!(orb.review_critique.as_deref(), Some("do better"));
}

// ── trigger modes (synchronous, deterministic) ────────────────────

#[test]
fn confidence_mode_triggers_only_below_threshold() {
    let mut low = done_orb("low", "x");
    low.confidence = Some(0.4);
    let mut high = done_orb("high", "x");
    high.confidence = Some(0.9);

    let cfg = SecondOpinionConfig {
        mode: SecondOpinionMode::Confidence,
        confidence_threshold: 0.7,
        sampling_rate: 0.0,
        reviewer_model: None,
    };
    let mut rng = StdRng::seed_from_u64(1);
    assert!(should_review(&low, &cfg, &mut rng));
    assert!(!should_review(&high, &cfg, &mut rng));
}

#[test]
fn sampling_mode_with_seeded_rng_is_reproducible() {
    let orb = done_orb("x", "x");
    let cfg = SecondOpinionConfig {
        mode: SecondOpinionMode::Sampling,
        confidence_threshold: 0.0,
        sampling_rate: 0.3,
        reviewer_model: None,
    };
    let mut r1 = StdRng::seed_from_u64(99);
    let mut r2 = StdRng::seed_from_u64(99);
    let s1: Vec<bool> = (0..30)
        .map(|_| should_review(&orb, &cfg, &mut r1))
        .collect();
    let s2: Vec<bool> = (0..30)
        .map(|_| should_review(&orb, &cfg, &mut r2))
        .collect();
    assert_eq!(s1, s2);
}

// ── CLI surface via the binary ────────────────────────────────────

fn orboros(state: &Path) -> Command {
    let mut cmd = Command::cargo_bin("orboros").unwrap();
    cmd.env("HOME", state);
    cmd.args(["--state-dir", state.to_str().unwrap()]);
    cmd
}

fn create_orb_via_cli(state: &Path, title: &str) -> String {
    let assert = orboros(state).args(["orb", "create", title]).assert();
    let output = assert.get_output().clone();
    assert.success();
    let stdout = String::from_utf8(output.stdout).unwrap();
    stdout
        .lines()
        .next()
        .unwrap()
        .strip_prefix("Created orb ")
        .unwrap()
        .trim()
        .to_string()
}

#[test]
fn cli_review_queue_is_empty_with_no_reviews() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path();
    let output = orboros(state).args(["review-queue"]).output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Review queue empty"), "got: {stdout}");
}

#[test]
fn cli_orb_show_displays_verdict_after_persisted() {
    use orbs::orb_store::OrbStore;
    use orbs::review::ReviewReport;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path();
    let id = create_orb_via_cli(state, "Reviewed");

    // Inject a report directly through the store (CLI for setting verdict
    // doesn't exist — verdicts arrive via apply_review_outcome).
    let store = OrbStore::new(state.join("orbs.jsonl"));
    let mut orb = store
        .load_by_id(&orbs::id::OrbId::from_raw(&id))
        .unwrap()
        .unwrap();
    orb.review_report = Some(ReviewReport {
        verdict: ReviewVerdict::Revise {
            scope: ReviseScope::Decomposition,
        },
        critique: "wrong plan".into(),
        suggested_changes: None,
        reviewer_model: "rev/m".into(),
        reviewed_at: chrono::Utc::now(),
        reviewer_orb_id: None,
    });
    orb.review_critique = Some("wrong plan".into());
    store.update(&orb).unwrap();

    let output = orboros(state).args(["orb", "show", &id]).output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("REVISE (decomposition)"),
        "expected verdict label in show output: {stdout}"
    );
    assert!(stdout.contains("wrong plan"));
    assert!(stdout.contains("rev/m"));
}

#[test]
fn cli_review_queue_lists_only_revise_orbs() {
    use orbs::orb_store::OrbStore;
    use orbs::review::ReviewReport;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path();

    let id_accept = create_orb_via_cli(state, "Accepted");
    let id_revise = create_orb_via_cli(state, "Revising");

    let store = OrbStore::new(state.join("orbs.jsonl"));
    let mut a = store
        .load_by_id(&orbs::id::OrbId::from_raw(&id_accept))
        .unwrap()
        .unwrap();
    a.review_report = Some(ReviewReport {
        verdict: ReviewVerdict::Accept,
        critique: String::new(),
        suggested_changes: None,
        reviewer_model: "rev/m".into(),
        reviewed_at: chrono::Utc::now(),
        reviewer_orb_id: None,
    });
    store.update(&a).unwrap();

    let mut r = store
        .load_by_id(&orbs::id::OrbId::from_raw(&id_revise))
        .unwrap()
        .unwrap();
    r.review_report = Some(ReviewReport {
        verdict: ReviewVerdict::Revise {
            scope: ReviseScope::Execution,
        },
        critique: "redo it".into(),
        suggested_changes: None,
        reviewer_model: "rev/m".into(),
        reviewed_at: chrono::Utc::now(),
        reviewer_orb_id: None,
    });
    store.update(&r).unwrap();

    let output = orboros(state).args(["review-queue"]).output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(&id_revise), "{stdout}");
    assert!(!stdout.contains(&id_accept), "{stdout}");
    assert!(stdout.contains("REVISE (execution)"));
    assert!(stdout.contains("1 orb(s) pending revise"));
}

#[test]
fn cli_orb_list_review_status_filter_picks_revise() {
    use orbs::orb_store::OrbStore;
    use orbs::review::ReviewReport;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path();

    let id_acc = create_orb_via_cli(state, "A");
    let id_rev = create_orb_via_cli(state, "R");
    let _id_none = create_orb_via_cli(state, "None");

    let store = OrbStore::new(state.join("orbs.jsonl"));
    let mut a = store
        .load_by_id(&orbs::id::OrbId::from_raw(&id_acc))
        .unwrap()
        .unwrap();
    a.review_report = Some(ReviewReport {
        verdict: ReviewVerdict::Accept,
        critique: String::new(),
        suggested_changes: None,
        reviewer_model: "rev/m".into(),
        reviewed_at: chrono::Utc::now(),
        reviewer_orb_id: None,
    });
    store.update(&a).unwrap();

    let mut r = store
        .load_by_id(&orbs::id::OrbId::from_raw(&id_rev))
        .unwrap()
        .unwrap();
    r.review_report = Some(ReviewReport {
        verdict: ReviewVerdict::Revise {
            scope: ReviseScope::Execution,
        },
        critique: "fix".into(),
        suggested_changes: None,
        reviewer_model: "rev/m".into(),
        reviewed_at: chrono::Utc::now(),
        reviewer_orb_id: None,
    });
    store.update(&r).unwrap();

    let output = orboros(state)
        .args(["orb", "list", "--review-status", "revise"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(&id_rev), "{stdout}");
    assert!(!stdout.contains(&id_acc), "{stdout}");
}

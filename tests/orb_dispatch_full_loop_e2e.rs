//! Full-loop end-to-end tests for task 60: the orb → worker dispatch
//! path, plus the reviewer Revise re-entry that 60.5 unlocked.
//!
//! Covers:
//! - Pending orb → tick() promotes to Active → dispatch_ready_orbs
//!   populates result + confidence + execution.
//! - Reviewer `Revise{Execution}` on a Done orb triggers
//!   `try_begin_revision`, orb returns to Active, dispatch on the
//!   next tick re-runs it.
//! - Revision cap is enforced: after MAX_REVISIONS, the orb stays
//!   Done and an on-escalate hook fires.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use orboros::queue_loop::{DrainStopReason, QueueLoop};
use orboros::second_opinion_trigger::apply_review_outcome;
use orboros::worker::process::WorkerConfig;
use orbs::dep_store::DepStore;
use orbs::orb::{Orb, OrbStatus, OrbType, MAX_REVISIONS};
use orbs::orb_store::OrbStore;
use orbs::review::{ReviewReport, ReviewVerdict, ReviseScope};

fn make_executable(path: &Path) {
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

fn write_worker_script(dir: &Path, name: &str, response: &str, confidence: Option<f32>) -> PathBuf {
    let body_file = dir.join(format!("{name}.body"));
    fs::write(&body_file, response).unwrap();
    let path = dir.join(name);
    let conf_field = confidence.map_or(String::new(), |c| format!(",'confidence':{c}"));
    let body = format!(
        r#"#!/bin/bash
BODY_FILE='{body_path}'
while IFS= read -r line; do
  type=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['type'])" 2>/dev/null)
  id=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['id'])" 2>/dev/null)
  case "$type" in
    init) echo "{{\"type\":\"init_ok\",\"id\":\"$id\",\"session_id\":\"s\",\"protocol_version\":\"0.3.0\"}}" ;;
    send) python3 -c "import json,sys; body=open('$BODY_FILE').read(); print(json.dumps({{'type':'result','id':'$id','status':'ok','response':body,'tool_calls_made':[],'iterations':1{conf_field}}}))" ;;
    shutdown) echo "{{\"type\":\"shutdown_ok\",\"id\":\"$id\"}}"; exit 0 ;;
  esac
done
"#,
        body_path = body_file.display(),
    );
    fs::write(&path, body).unwrap();
    make_executable(&path);
    path
}

fn worker_config(script: &Path) -> WorkerConfig {
    WorkerConfig {
        command: "bash".into(),
        args: vec![script.to_string_lossy().into()],
        cwd: None,
        env: vec![],
        model: "mock/full-loop".into(),
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

fn make_review(verdict: ReviewVerdict, critique: &str) -> ReviewReport {
    ReviewReport {
        verdict,
        critique: critique.into(),
        suggested_changes: None,
        reviewer_model: "mock-reviewer".into(),
        reviewed_at: chrono::Utc::now(),
        reviewer_orb_id: None,
    }
}

// ── full pipeline: Pending → tick → Active → dispatch → Done ─────

#[tokio::test]
async fn full_loop_pending_to_done_via_tick_and_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_worker_script(dir.path(), "ok.sh", "did the thing", Some(0.87));
    let wc = worker_config(&script);

    let base = dir.path().to_path_buf();
    let orb_store = OrbStore::new(base.join("orbs.jsonl"));
    let dep_store = DepStore::new(base.join("deps.jsonl"));

    // Create a Pending task orb.
    let orb = Orb::new("Implement feature", "Build it").with_type(OrbType::Task);
    orb_store.append(&orb).unwrap();

    let ql = QueueLoop::new(orb_store.clone(), dep_store, base);

    // Step 1: tick() promotes Pending → Active (existing behavior).
    let result = ql.tick().unwrap();
    assert_eq!(
        result.orbs_executed, 1,
        "tick should promote Pending to Active"
    );
    let after_tick = orb_store.load_by_id(&orb.id).unwrap().unwrap();
    assert_eq!(after_tick.status, Some(OrbStatus::Active));
    assert!(after_tick.execution.is_none());

    // Step 2: dispatch_ready_orbs runs the worker and writes back.
    let completed = ql.dispatch_ready_orbs(&wc, 2).await.unwrap();
    assert_eq!(completed, 1);
    let after_dispatch = orb_store.load_by_id(&orb.id).unwrap().unwrap();
    assert_eq!(after_dispatch.status, Some(OrbStatus::Done));
    assert_eq!(after_dispatch.result.as_deref(), Some("did the thing"));
    assert_eq!(after_dispatch.confidence, Some(0.87));
    assert!(after_dispatch.execution.is_some());
    let exec = after_dispatch.execution.unwrap();
    assert_eq!(exec.worker_model.as_deref(), Some("mock/full-loop"));
}

#[tokio::test]
async fn drain_target_runs_pending_task_to_done() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_worker_script(dir.path(), "drain-ok.sh", "foreground done", Some(0.91));
    let wc = worker_config(&script);

    let base = dir.path().to_path_buf();
    let orb_store = OrbStore::new(base.join("orbs.jsonl"));
    let dep_store = DepStore::new(base.join("deps.jsonl"));

    let orb = Orb::new("Foreground task", "Run it now").with_type(OrbType::Task);
    orb_store.append(&orb).unwrap();

    let ql = QueueLoop::new(orb_store.clone(), dep_store, base);
    let result = ql
        .drain_target(
            &orb.id,
            &wc,
            1,
            true,
            5,
            std::time::Duration::from_millis(1),
        )
        .await
        .unwrap();

    assert_eq!(result.reason, DrainStopReason::TargetTerminal);
    assert_eq!(result.workers_completed, 1);
    let loaded = orb_store.load_by_id(&orb.id).unwrap().unwrap();
    assert_eq!(loaded.status, Some(OrbStatus::Done));
    assert_eq!(loaded.result.as_deref(), Some("foreground done"));
}

// ── Revise re-entry: Done → Active via apply_review_outcome ──────

#[tokio::test]
async fn revise_verdict_re_enters_and_redispatch_runs() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_worker_script(dir.path(), "ok.sh", "second pass", Some(0.5));
    let wc = worker_config(&script);

    let base = dir.path().to_path_buf();
    let orb_store = OrbStore::new(base.join("orbs.jsonl"));
    let dep_store = DepStore::new(base.join("deps.jsonl"));

    // Create a Done orb (simulating completion from a prior tick).
    let mut orb = Orb::new("Implement", "Build it").with_type(OrbType::Task);
    orb.set_status(OrbStatus::Active).unwrap();
    orb.result = Some("first pass output".into());
    orb.confidence = Some(0.3);
    // Pretend it was dispatched: set execution so dispatch_ready_orbs
    // would normally skip it.
    orb.execution = Some(orbs::orb::ExecutionMeta {
        worker_model: Some("mock".into()),
        ..Default::default()
    });
    orb.set_status(OrbStatus::Done).unwrap();
    orb_store.append(&orb).unwrap();

    // Reviewer says "Revise{Execution}".
    apply_review_outcome(
        &mut orb,
        make_review(
            ReviewVerdict::Revise {
                scope: ReviseScope::Execution,
            },
            "do it better",
        ),
        None,
    );
    // After re-entry: status flips to Active, revision_count bumps,
    // critique is stored. Execution stays set from the prior dispatch
    // — dispatch_ready_orbs will skip it. Clear it to simulate
    // pipeline re-prepping the orb for redispatch.
    assert_eq!(orb.status, Some(OrbStatus::Active));
    assert_eq!(orb.revision_count, 1);
    assert_eq!(orb.review_critique.as_deref(), Some("do it better"));
    orb.execution = None;
    orb_store.update(&orb).unwrap();

    // Redispatch produces a fresh result.
    let ql = QueueLoop::new(orb_store.clone(), dep_store, base);
    let completed = ql.dispatch_ready_orbs(&wc, 2).await.unwrap();
    assert_eq!(completed, 1);
    let reloaded = orb_store.load_by_id(&orb.id).unwrap().unwrap();
    assert_eq!(reloaded.status, Some(OrbStatus::Done));
    assert_eq!(reloaded.result.as_deref(), Some("second pass"));
}

// ── Revision cap: stays Done after MAX_REVISIONS ─────────────────

#[tokio::test]
async fn revision_cap_keeps_orb_done_after_max_revisions() {
    let dir = tempfile::tempdir().unwrap();
    let _script = write_worker_script(dir.path(), "ok.sh", "x", None);

    let mut orb = Orb::new("t", "d").with_type(OrbType::Task);
    orb.set_status(OrbStatus::Active).unwrap();
    orb.set_status(OrbStatus::Done).unwrap();
    orb.revision_count = MAX_REVISIONS;

    let prior_status = orb.status;
    apply_review_outcome(
        &mut orb,
        make_review(
            ReviewVerdict::Revise {
                scope: ReviseScope::Execution,
            },
            "still off",
        ),
        None,
    );
    // Cap was hit — no re-entry. Verdict was still recorded.
    assert_eq!(orb.status, prior_status, "stays Done after cap");
    assert_eq!(orb.revision_count, MAX_REVISIONS);
    assert!(orb.review_report.is_some());
}

// ── on-escalate hook fires when cap is hit ───────────────────────

#[test]
fn on_escalate_hook_fires_when_revision_cap_exceeded() {
    use orboros::hooks::sink::HookSink;

    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path();
    fs::write(
        state_dir.join("hooks.toml"),
        r#"
        [[hook]]
        name = "escalate-marker"
        on = "on-escalate"
        run = "true"
        sync = true
        "#,
    )
    .unwrap();
    let sink = HookSink::from_state_dir(state_dir, state_dir)
        .unwrap()
        .expect("hooks loaded");

    let mut orb = Orb::new("t", "d").with_type(OrbType::Task);
    orb.set_status(OrbStatus::Active).unwrap();
    orb.set_status(OrbStatus::Done).unwrap();
    orb.revision_count = MAX_REVISIONS;

    apply_review_outcome(
        &mut orb,
        make_review(
            ReviewVerdict::Revise {
                scope: ReviseScope::Execution,
            },
            "still off",
        ),
        Some(&sink),
    );

    let log = fs::read_to_string(state_dir.join("hooks.log.jsonl")).unwrap_or_default();
    assert!(
        log.contains("escalate-marker"),
        "on-escalate hook should fire when cap is hit: {log}"
    );
}

// ── Phase orbs: dispatch from Speccing populates design ──────────

#[tokio::test]
async fn phase_orb_in_speccing_gets_design_populated_via_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let response = r#"{"design": "use a workqueue", "acceptance_criteria": "- [ ] queue exists"}"#;
    let script = write_worker_script(dir.path(), "spec.sh", response, Some(0.9));
    let wc = worker_config(&script);

    let base = dir.path().to_path_buf();
    let orb_store = OrbStore::new(base.join("orbs.jsonl"));
    let dep_store = DepStore::new(base.join("deps.jsonl"));

    let mut orb = Orb::new("Build queue", "Async work").with_type(OrbType::Feature);
    orb.phase = Some(orbs::orb::OrbPhase::Speccing);
    orb_store.append(&orb).unwrap();

    let ql = QueueLoop::new(orb_store.clone(), dep_store, base);
    let completed = ql.dispatch_ready_orbs(&wc, 1).await.unwrap();
    assert_eq!(completed, 1);

    let reloaded = orb_store.load_by_id(&orb.id).unwrap().unwrap();
    assert_eq!(reloaded.design.as_deref(), Some("use a workqueue"));
    assert!(reloaded.acceptance_criteria.is_some());
}

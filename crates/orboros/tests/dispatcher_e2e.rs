//! End-to-end tests for `worker::dispatcher::dispatch_orb` against
//! a mock worker. Exercises the spawn → send → shutdown flow and
//! verifies the outcome carries the right shape for
//! `apply_dispatch_outcome` to mutate the orb.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use orboros::worker::dispatcher::{apply_dispatch_outcome, dispatch_orb, DispatchStatus};
use orboros::worker::process::WorkerConfig;
use orbs::orb::{Orb, OrbStatus, OrbType};

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
    init) echo "{{\"type\":\"init_ok\",\"id\":\"$id\",\"session_id\":\"s\",\"protocol_version\":\"0.2.0\"}}" ;;
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
        model: "mock/dispatcher".into(),
        system_prompt: "test".into(),
        tools: vec![],
        max_iterations: Some(1),
        init_timeout: None,
        send_timeout: None,
        shutdown_timeout: None,
        task_id: None,
        worker_id: Some("worker-test-1".into()),
    }
}

fn active_orb() -> Orb {
    let mut o = Orb::new("Test", "Do a thing").with_type(OrbType::Task);
    o.set_status(OrbStatus::Active).unwrap();
    o
}

#[tokio::test]
async fn dispatch_orb_done_outcome_propagates_response_and_confidence() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_worker_script(dir.path(), "ok.sh", "the answer", Some(0.91));
    let wc = worker_config(&script);
    let orb = active_orb();

    let outcome = dispatch_orb(&orb, "what is X?", &wc, None).await.unwrap();
    assert_eq!(outcome.status, DispatchStatus::Done);
    assert_eq!(outcome.response.as_deref(), Some("the answer"));
    assert_eq!(outcome.confidence, Some(0.91));
    assert_eq!(outcome.worker_model, "mock/dispatcher");
    assert_eq!(outcome.worker_id.as_deref(), Some("worker-test-1"));
}

#[tokio::test]
async fn dispatch_then_apply_persists_to_orb() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_worker_script(dir.path(), "ok.sh", "the answer", Some(0.7));
    let wc = worker_config(&script);
    let mut orb = active_orb();

    let outcome = dispatch_orb(&orb, "what is X?", &wc, None).await.unwrap();
    apply_dispatch_outcome(&mut orb, &outcome).unwrap();

    assert_eq!(orb.status, Some(OrbStatus::Done));
    assert_eq!(orb.result.as_deref(), Some("the answer"));
    assert_eq!(orb.confidence, Some(0.7));
    let exec = orb.execution.as_ref().unwrap();
    assert_eq!(exec.worker_model.as_deref(), Some("mock/dispatcher"));
    assert_eq!(exec.worker_id.as_deref(), Some("worker-test-1"));
    assert!(exec.dispatched_at.is_some());
    assert!(exec.completed_at.is_some());
}

#[tokio::test]
async fn dispatch_with_spawn_failure_returns_failed_status() {
    let dir = tempfile::tempdir().unwrap();
    // Point at a non-existent script.
    let bogus = dir.path().join("does-not-exist.sh");
    let mut wc = worker_config(&bogus);
    wc.command = bogus.to_string_lossy().into();
    wc.args = vec![];
    let orb = active_orb();

    let outcome = dispatch_orb(&orb, "x", &wc, None).await.unwrap();
    assert_eq!(outcome.status, DispatchStatus::Failed);
    assert!(outcome.error.is_some());
    assert!(outcome.response.is_none());
}

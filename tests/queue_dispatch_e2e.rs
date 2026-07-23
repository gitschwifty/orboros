//! End-to-end test that drives `QueueLoop::dispatch_ready_orbs`
//! against a mock worker. Creates an Active task orb in the store,
//! runs one dispatch tick, then reads back to verify result +
//! confidence + execution were persisted.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use orboros::queue_loop::QueueLoop;
use orboros::worker::process::WorkerConfig;
use orbs::dep::{DepEdge, EdgeType};
use orbs::dep_store::DepStore;
use orbs::id::OrbId;
use orbs::orb::{Orb, OrbStatus, OrbType};
use orbs::orb_store::OrbStore;

fn make_executable(path: &Path) {
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

fn write_worker_script(dir: &Path, name: &str, response: &str) -> PathBuf {
    let body_file = dir.join(format!("{name}.body"));
    fs::write(&body_file, response).unwrap();
    let path = dir.join(name);
    let body = format!(
        r#"#!/bin/bash
BODY_FILE='{body_path}'
while IFS= read -r line; do
  type=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['type'])" 2>/dev/null)
  id=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['id'])" 2>/dev/null)
  case "$type" in
    init) echo "{{\"type\":\"init_ok\",\"id\":\"$id\",\"session_id\":\"s\",\"protocol_version\":\"0.3.0\"}}" ;;
    send) python3 -c "import json,sys; body=open('$BODY_FILE').read(); print(json.dumps({{'type':'result','id':'$id','status':'ok','response':body,'tool_calls_made':[],'iterations':1,'confidence':0.88}}))" ;;
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

fn write_echo_prompt_worker_script(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    let body = r#"#!/bin/bash
while IFS= read -r line; do
  type=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['type'])" 2>/dev/null)
  id=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['id'])" 2>/dev/null)
  case "$type" in
    init) echo "{\"type\":\"init_ok\",\"id\":\"$id\",\"session_id\":\"s\",\"protocol_version\":\"0.3.0\"}" ;;
    send) python3 -c "import json,sys; req=json.loads(sys.stdin.read()); print(json.dumps({'type':'result','id':req['id'],'status':'ok','response':req['message'],'tool_calls_made':[],'iterations':1,'confidence':0.91}))" <<< "$line" ;;
    shutdown) echo "{\"type\":\"shutdown_ok\",\"id\":\"$id\"}"; exit 0 ;;
  esac
done
"#;
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
        model: "mock/queue".into(),
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
    }
}

fn active_task_orb(title: &str) -> Orb {
    let mut o = Orb::new(title, "Do the thing").with_type(OrbType::Task);
    o.set_status(OrbStatus::Active).unwrap();
    o
}

#[tokio::test]
async fn dispatch_ready_orbs_populates_result_and_confidence() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_worker_script(dir.path(), "ok.sh", "the answer");
    let wc = worker_config(&script);

    let base = dir.path().to_path_buf();
    let orb_store = OrbStore::new(base.join("orbs.jsonl"));
    let dep_store = DepStore::new(base.join("deps.jsonl"));
    let orb = active_task_orb("Run me");
    orb_store.append(&orb).unwrap();

    let ql = QueueLoop::new(orb_store.clone(), dep_store, base);
    let completed = ql.dispatch_ready_orbs(&wc, 2).await.unwrap();
    assert_eq!(completed, 1);

    let reloaded = orb_store.load_by_id(&orb.id).unwrap().unwrap();
    assert_eq!(reloaded.status, Some(OrbStatus::Done));
    assert_eq!(reloaded.result.as_deref(), Some("the answer"));
    assert_eq!(reloaded.confidence, Some(0.88));
    let execution = reloaded.execution.as_ref().unwrap();
    assert_eq!(execution.prompt_category.as_deref(), Some("worker.execute"));
    assert_eq!(execution.system_prompt_source.as_deref(), Some("built_in"));
    assert_eq!(
        execution.system_prompt_hash.as_deref(),
        Some(orboros::prompt::prompt_hash(
            orboros::prompt::built_in_worker_system_prompt("execute")
        ))
        .as_deref()
    );
}

#[tokio::test]
async fn dispatch_ready_orbs_injects_orb_context_into_user_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_echo_prompt_worker_script(dir.path(), "echo-prompt.sh");
    let wc = worker_config(&script);

    let base = dir.path().to_path_buf();
    let orb_store = OrbStore::new(base.join("orbs.jsonl"));
    let dep_store = DepStore::new(base.join("deps.jsonl"));

    let parent = Orb::new("Parent feature", "Parent spec").with_type(OrbType::Feature);
    let mut blocker = active_task_orb("Prepare dependency");
    blocker.set_status(OrbStatus::Done).unwrap();
    blocker.result = Some("dependency output".into());
    let mut orb = active_task_orb("Run with context");
    orb.parent_id = Some(parent.id.clone());
    orb.root_id = Some(parent.id.clone());
    orb.acceptance_criteria = Some("- [ ] include context".into());
    let mut sibling = Orb::new("Sibling task", "Nearby work").with_type(OrbType::Task);
    sibling.parent_id = Some(parent.id.clone());
    sibling.root_id = Some(parent.id.clone());

    orb_store.append(&parent).unwrap();
    orb_store.append(&blocker).unwrap();
    orb_store.append(&sibling).unwrap();
    orb_store.append(&orb).unwrap();
    dep_store
        .add_edge(DepEdge::new(
            blocker.id.clone(),
            orb.id.clone(),
            EdgeType::Blocks,
        ))
        .unwrap();

    let ql = QueueLoop::new(orb_store.clone(), dep_store, base);
    let completed = ql.dispatch_ready_orbs(&wc, 2).await.unwrap();
    assert_eq!(completed, 1);

    let reloaded = orb_store.load_by_id(&orb.id).unwrap().unwrap();
    let result = reloaded.result.as_deref().unwrap();
    assert!(result.starts_with("Do the thing"));
    assert!(result.contains("## Orboros Task Context"));
    assert!(result.contains("Parent feature"));
    assert!(result.contains("Sibling task"));
    assert!(result.contains("Prepare dependency"));
    assert!(result.contains("dependency output"));
    assert!(result.contains("acceptance_criteria"));
}

#[tokio::test]
async fn dispatch_ready_orbs_is_idempotent_once_execution_set() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_worker_script(dir.path(), "ok.sh", "x");
    let wc = worker_config(&script);

    let base = dir.path().to_path_buf();
    let orb_store = OrbStore::new(base.join("orbs.jsonl"));
    let dep_store = DepStore::new(base.join("deps.jsonl"));
    orb_store.append(&active_task_orb("A")).unwrap();

    let ql = QueueLoop::new(orb_store.clone(), dep_store, base);
    let first = ql.dispatch_ready_orbs(&wc, 2).await.unwrap();
    assert_eq!(first, 1);
    // Second call: orb is Done with execution set — no re-dispatch.
    let second = ql.dispatch_ready_orbs(&wc, 2).await.unwrap();
    assert_eq!(second, 0);
}

#[tokio::test]
async fn dispatch_ready_orbs_runs_multiple_in_parallel() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_worker_script(dir.path(), "ok.sh", "x");
    let wc = worker_config(&script);

    let base = dir.path().to_path_buf();
    let orb_store = OrbStore::new(base.join("orbs.jsonl"));
    let dep_store = DepStore::new(base.join("deps.jsonl"));
    for i in 0..3 {
        orb_store
            .append(&active_task_orb(&format!("orb-{i}")))
            .unwrap();
    }

    let ql = QueueLoop::new(orb_store.clone(), dep_store, base);
    let completed = ql.dispatch_ready_orbs(&wc, 3).await.unwrap();
    assert_eq!(completed, 3, "all 3 orbs should have dispatched");

    // Each orb should now have execution + result.
    for orb in orb_store.load_all().unwrap() {
        let _ = OrbId::from_raw(orb.id.as_str()); // sanity
        assert_eq!(orb.status, Some(OrbStatus::Done));
        assert!(orb.execution.is_some());
    }
}

#[tokio::test]
async fn dispatch_ready_orbs_ignores_pending_orbs() {
    // Pending orbs aren't yet Active — they shouldn't be dispatched
    // until the queue loop's existing tick promotes them.
    let dir = tempfile::tempdir().unwrap();
    let script = write_worker_script(dir.path(), "ok.sh", "x");
    let wc = worker_config(&script);

    let base = dir.path().to_path_buf();
    let orb_store = OrbStore::new(base.join("orbs.jsonl"));
    let dep_store = DepStore::new(base.join("deps.jsonl"));
    let orb = Orb::new("Pending", "x").with_type(OrbType::Task);
    orb_store.append(&orb).unwrap();

    let ql = QueueLoop::new(orb_store.clone(), dep_store, base);
    let completed = ql.dispatch_ready_orbs(&wc, 2).await.unwrap();
    assert_eq!(completed, 0);

    let reloaded = orb_store.load_by_id(&orb.id).unwrap().unwrap();
    assert!(
        reloaded.execution.is_none(),
        "Pending orbs should not get dispatched"
    );
}

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
use orbs::orb::{Orb, OrbPhase, OrbStatus, OrbType};
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

/// A mock Heddle worker that honors the runtime transcript destination supplied
/// by Orboros during `init`. This makes project log-home placement testable
/// without a real provider or Heddle runtime.
fn write_runtime_evidence_worker_script(dir: &Path, name: &str) -> PathBuf {
    let runtime_path = dir.join(format!("{name}.runtime-path"));
    let path = dir.join(name);
    let body = format!(
        r#"#!/bin/bash
RUNTIME_PATH='{runtime_path}'
while IFS= read -r line; do
  type=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['type'])" 2>/dev/null)
  id=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['id'])" 2>/dev/null)
  case "$type" in
    init)
      python3 -c "import json,os,sys; req=json.loads(sys.stdin.read()); path=req['config']['runtime']['transcript_path']; os.makedirs(os.path.dirname(path), exist_ok=True); open(path, 'w').write('mock transcript\\n'); open('{runtime_path}', 'w').write(path)" <<< "$line"
      echo "{{\"type\":\"init_ok\",\"id\":\"$id\",\"session_id\":\"s\",\"protocol_version\":\"0.3.0\"}}"
      ;;
    send)
      python3 -c "import json,sys; req=json.loads(sys.stdin.read()); path=open('{runtime_path}').read(); print(json.dumps({{'type':'result','id':req['id'],'status':'ok','response':'recorded','tool_calls_made':[],'iterations':1,'runtime':{{'mode':'isolated','transcript_path':path}}}}))" <<< "$line"
      ;;
    shutdown) echo "{{\"type\":\"shutdown_ok\",\"id\":\"$id\"}}"; exit 0 ;;
  esac
done
"#,
        runtime_path = runtime_path.display(),
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
    assert!(execution.system_prompt_hash.is_some());
}

#[tokio::test]
async fn project_log_homes_keep_worker_and_execution_evidence_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_runtime_evidence_worker_script(dir.path(), "runtime-evidence.sh");
    let worker = worker_config(&script);

    let mut projects = Vec::new();
    for name in ["alpha", "beta"] {
        let state_dir = dir.path().join(format!("{name}-state"));
        let log_home = dir.path().join(format!("{name}-logs"));
        fs::create_dir_all(&state_dir).unwrap();
        let orb_store = OrbStore::new(state_dir.join("orbs.jsonl"));
        let dep_store = DepStore::new(state_dir.join("deps.jsonl"));
        let orb = active_task_orb(name);
        orb_store.append(&orb).unwrap();
        let queue = QueueLoop::new(orb_store, dep_store, state_dir)
            .with_worker_evidence_dir(log_home.join("heddle"))
            .with_execution_log_path(log_home.join("executions.jsonl"));

        assert_eq!(queue.dispatch_ready_orbs(&worker, 1).await.unwrap(), 1);
        projects.push((orb.id.to_string(), log_home));
    }

    for (index, (orb_id, log_home)) in projects.iter().enumerate() {
        let records = orboros::execution::ExecutionStore::new(log_home.join("executions.jsonl"))
            .read_all()
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].orb_id, *orb_id);
        assert_eq!(records[0].attempts.len(), 1);
        let attempt = &records[0].attempts[0];
        let worker_id = attempt.worker_id.as_deref().unwrap();
        let runtime = attempt.runtime.as_ref().unwrap();
        let transcript = PathBuf::from(&runtime.transcript_path);
        assert!(transcript.starts_with(log_home.join("heddle")));
        assert!(transcript.ends_with(format!("worker-{worker_id}.jsonl")));
        assert_eq!(
            fs::read_to_string(&transcript).unwrap(),
            "mock transcript\n"
        );

        let (_, other_log_home) = &projects[1 - index];
        assert!(!transcript.starts_with(other_log_home));
        assert!(!other_log_home.join("heddle").join(orb_id).exists());
    }
}

#[tokio::test]
async fn dispatch_ready_orbs_injects_orb_context_into_user_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_echo_prompt_worker_script(dir.path(), "echo-prompt.sh");
    let wc = worker_config(&script);

    let base = dir.path().to_path_buf();
    let orb_store = OrbStore::new(base.join("orbs.jsonl"));
    let dep_store = DepStore::new(base.join("deps.jsonl"));

    let mut parent = Orb::new("Parent feature", "Parent spec").with_type(OrbType::Feature);
    parent.phase = Some(OrbPhase::Waiting);
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
async fn dispatch_recovers_one_terminal_retry_that_left_a_changed_child_workdir() {
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path().join("workdir");
    fs::create_dir_all(&workdir).unwrap();
    let counter = dir.path().join("attempts");
    let changed = workdir.join("partial.txt");
    let script = dir.path().join("recover-once.sh");
    fs::write(
        &script,
        format!(
            r##"#!/bin/bash
COUNTER='{counter}'
CHANGED='{changed}'
while IFS= read -r line; do
  type=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['type'])")
  id=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['id'])")
  case "$type" in
    init) echo "{{\"type\":\"init_ok\",\"id\":\"$id\",\"session_id\":\"s\",\"protocol_version\":\"0.4.0\"}}" ;;
    send)
      attempt=$(cat "$COUNTER" 2>/dev/null || echo 0)
      if [ "$attempt" = 0 ]; then
        echo partial > "$CHANGED"; echo 1 > "$COUNTER"
        echo "{{\"type\":\"result\",\"id\":\"$id\",\"status\":\"error\",\"error\":{{\"code\":\"loop_detected\",\"message\":\"loop\",\"retryable\":false}},\"tool_calls_made\":[],\"iterations\":1,\"failure\":{{\"code\":\"loop_detected\",\"termination_reason\":\"loop\",\"iterations\":1,\"tool_calls_made\":1}}}}"
      elif [ "$attempt" = 1 ]; then
        echo 2 > "$COUNTER"
        echo "{{\"type\":\"result\",\"id\":\"$id\",\"status\":\"error\",\"error\":{{\"code\":\"model_error\",\"message\":\"stream failed\",\"retryable\":false}},\"tool_calls_made\":[],\"iterations\":1}}"
      else
        echo "{{\"type\":\"result\",\"id\":\"$id\",\"status\":\"ok\",\"response\":\"recovered and verified\",\"tool_calls_made\":[],\"iterations\":1}}"
      fi
      ;;
    shutdown) echo "{{\"type\":\"shutdown_ok\",\"id\":\"$id\"}}"; exit 0 ;;
  esac
done
"##,
            counter = counter.display(),
            changed = changed.display(),
        ),
    )
    .unwrap();
    make_executable(&script);
    let mut wc = worker_config(&script);
    wc.cwd = Some(workdir);

    let base = dir.path().to_path_buf();
    let orb_store = OrbStore::new(base.join("orbs.jsonl"));
    let dep_store = DepStore::new(base.join("deps.jsonl"));
    let mut parent =
        Orb::new("Parent", "must pass the focused verification").with_type(OrbType::Feature);
    parent.phase = Some(OrbPhase::Waiting);
    let mut child = active_task_orb("Child");
    child.parent_id = Some(parent.id.clone());
    child.root_id = Some(parent.id.clone());
    orb_store.append(&parent).unwrap();
    orb_store.append(&child).unwrap();

    let ql = QueueLoop::new(orb_store.clone(), dep_store, base);
    assert_eq!(ql.dispatch_ready_orbs(&wc, 1).await.unwrap(), 1);
    let recovered = orb_store.load_by_id(&child.id).unwrap().unwrap();
    assert_eq!(recovered.status, Some(OrbStatus::Done));
    assert_eq!(recovered.result.as_deref(), Some("recovered and verified"));
    assert_eq!(fs::read_to_string(changed).unwrap(), "partial\n");
    assert_eq!(fs::read_to_string(counter).unwrap().trim(), "2");
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

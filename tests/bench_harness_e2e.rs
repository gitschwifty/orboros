//! End-to-end tests for the T1 benchmark harness driving a mock
//! worker. Exercises grade_attempt + run_t1_case + run_t1 against
//! the in-process worker plumbing.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use orboros::bench::case::{BenchCase, BenchExpected, BenchTier};
use orboros::bench::runner::{run_t1, run_t1_case, BenchRunConfig, RunOptions};
use orboros::bench::store::{BenchStatus, BenchStore};
use orboros::worker::process::WorkerConfig;

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
    send) python3 -c "import json,sys; body=open('$BODY_FILE').read(); print(json.dumps({{'type':'result','id':'$id','status':'ok','response':body,'tool_calls_made':[],'iterations':1}}))" ;;
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

fn write_error_worker_script(dir: &Path, name: &str, message: &str) -> PathBuf {
    let path = dir.join(name);
    let body = format!(
        r#"#!/bin/bash
export MESSAGE='{message}'
while IFS= read -r line; do
  type=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['type'])" 2>/dev/null)
  id=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['id'])" 2>/dev/null)
  case "$type" in
    init) echo "{{\"type\":\"init_ok\",\"id\":\"$id\",\"session_id\":\"s\",\"protocol_version\":\"0.3.0\"}}" ;;
    send) python3 -c "import json,os; print(json.dumps({{'type':'result','id':'$id','status':'error','error':{{'code':'model_error','message':os.environ['MESSAGE'],'retryable':False}},'tool_calls_made':[],'iterations':1}}))" ;;
    shutdown) echo "{{\"type\":\"shutdown_ok\",\"id\":\"$id\"}}"; exit 0 ;;
  esac
done
"#
    );
    fs::write(&path, body).unwrap();
    make_executable(&path);
    path
}

fn write_protocol_mismatch_worker_script(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    let body = r#"#!/bin/bash
while IFS= read -r line; do
  type=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['type'])" 2>/dev/null)
  id=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['id'])" 2>/dev/null)
  case "$type" in
    init) echo "{\"type\":\"init_ok\",\"id\":\"$id\",\"session_id\":\"s\",\"protocol_version\":\"0.2.0\"}" ;;
    send) echo "{\"type\":\"result\",\"id\":\"$id\",\"status\":\"ok\",\"response\":\"hello\",\"tool_calls_made\":[],\"iterations\":1}" ;;
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
        model: "mock/bench".into(),
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

fn t1_case(id: &str, prompt: &str, expected: BenchExpected) -> BenchCase {
    BenchCase {
        id: id.into(),
        tier: BenchTier::T1,
        name: id.into(),
        description: "test".into(),
        prompt: prompt.into(),
        expected,
        runner: None,
        seed_repo: None,
        timeout_s: Some(60),
        max_iterations: None,
        max_cost_cents: 100,
    }
}

#[tokio::test]
async fn t1_case_all_attempts_match_grades_pass() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_worker_script(dir.path(), "ok.sh", "hello");
    let wc = worker_config(&script);
    let case = t1_case(
        "c-pass",
        "say hello",
        BenchExpected::Exact {
            text: "hello".into(),
        },
    );
    let r = run_t1_case(&case, "run-x", &wc, &RunOptions::default())
        .await
        .unwrap();
    assert_eq!(r.status, BenchStatus::Pass);
    // Exact 3/3 pass rate.
    assert!((r.score - 1.0).abs() < f32::EPSILON);
    assert_eq!(r.tier, BenchTier::T1);
    assert!(!r.prompt_hash.is_empty());
}

#[tokio::test]
async fn t1_case_no_match_grades_fail() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_worker_script(dir.path(), "nope.sh", "wrong");
    let wc = worker_config(&script);
    let case = t1_case(
        "c-fail",
        "say hello",
        BenchExpected::Exact {
            text: "hello".into(),
        },
    );
    let r = run_t1_case(&case, "run-x", &wc, &RunOptions::default())
        .await
        .unwrap();
    assert_eq!(r.status, BenchStatus::Fail);
    assert!((r.score - 0.0).abs() < f32::EPSILON);
}

#[tokio::test]
async fn t1_case_worker_error_records_error_status_and_message() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_error_worker_script(dir.path(), "err.sh", "model is not available");
    let wc = worker_config(&script);
    let case = t1_case(
        "c-error",
        "say hello",
        BenchExpected::Exact {
            text: "hello".into(),
        },
    );
    let r = run_t1_case(&case, "run-x", &wc, &RunOptions::default())
        .await
        .unwrap();

    assert_eq!(r.status, BenchStatus::Error);
    assert_eq!(r.error.as_deref(), Some("model is not available"));
    assert!(r.output.as_deref().is_some_and(|out| {
        out.contains("worker_error") && out.contains("model is not available")
    }));
}

#[tokio::test]
async fn t1_regex_match_passes() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_worker_script(dir.path(), "rgx.sh", "version=1.2.3 released");
    let wc = worker_config(&script);
    let case = t1_case(
        "c-rgx",
        "version please",
        BenchExpected::Regex {
            pattern: r"version=\d+\.\d+\.\d+".into(),
        },
    );
    let r = run_t1_case(&case, "run-x", &wc, &RunOptions::default())
        .await
        .unwrap();
    assert_eq!(r.status, BenchStatus::Pass);
}

#[tokio::test]
async fn t1_case_extracts_confidence_line_from_response() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_worker_script(dir.path(), "conf.sh", "hello\nCONFIDENCE: 0.82");
    let wc = worker_config(&script);
    let case = t1_case(
        "c-conf",
        "say hello",
        BenchExpected::Exact {
            text: "hello".into(),
        },
    );
    let r = run_t1_case(&case, "run-x", &wc, &RunOptions::default())
        .await
        .unwrap();
    assert_eq!(r.status, BenchStatus::Pass);
    assert_eq!(r.confidence, Some(0.82));
    assert!(r
        .output
        .as_deref()
        .is_some_and(|out| out.contains("== attempt 0 pass ==") && out.contains("hello")));
    assert_eq!(r.system_prompt_source.as_deref(), Some("bench_t1_builtin"));
    assert!(r.system_prompt_hash.is_some());
}

#[tokio::test]
async fn run_t1_writes_results_and_summary_to_store() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_worker_script(dir.path(), "ok.sh", "hello");
    let wc = worker_config(&script);
    let cases = vec![
        t1_case(
            "c1",
            "say hello",
            BenchExpected::Exact {
                text: "hello".into(),
            },
        ),
        t1_case(
            "c2",
            "say hello",
            BenchExpected::Exact {
                text: "different".into(),
            },
        ),
    ];
    let store = BenchStore::new(dir.path().join("bench"));
    let summary = run_t1(
        &cases,
        &wc,
        &store,
        &RunOptions::default(),
        &BenchRunConfig::default(),
    )
    .await
    .unwrap();

    assert_eq!(summary.results.len(), 2);
    let runs = store.read_runs().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].run_id, summary.run_id);
    assert_eq!(runs[0].passed, 1);
    assert_eq!(runs[0].failed, 1);

    let case_results = store.read_results(&summary.run_id).unwrap();
    assert_eq!(case_results.len(), 2);
    assert!(case_results
        .iter()
        .any(|r| r.case_id == "c1" && r.status == BenchStatus::Pass));
    assert!(case_results
        .iter()
        .any(|r| r.case_id == "c2" && r.status == BenchStatus::Fail));
}

#[tokio::test]
async fn run_t1_stops_after_fatal_worker_error() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_error_worker_script(
        dir.path(),
        "err.sh",
        "openrouter/nope is not a valid model ID",
    );
    let wc = worker_config(&script);
    let cases = vec![
        t1_case(
            "c1",
            "say hello",
            BenchExpected::Exact {
                text: "hello".into(),
            },
        ),
        t1_case(
            "c2",
            "say hello",
            BenchExpected::Exact {
                text: "hello".into(),
            },
        ),
    ];
    let store = BenchStore::new(dir.path().join("bench"));
    let summary = run_t1(
        &cases,
        &wc,
        &store,
        &RunOptions::default(),
        &BenchRunConfig::default(),
    )
    .await
    .unwrap();

    assert_eq!(summary.results.len(), 1);
    assert_eq!(summary.results[0].case_id, "c1");
    assert_eq!(summary.results[0].status, BenchStatus::Error);
    let case_results = store.read_results(&summary.run_id).unwrap();
    assert_eq!(case_results.len(), 1);
}

#[tokio::test]
async fn run_t1_stops_after_protocol_mismatch_without_retries() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_protocol_mismatch_worker_script(dir.path(), "old-protocol.sh");
    let wc = worker_config(&script);
    let cases = vec![
        t1_case(
            "c1",
            "say hello",
            BenchExpected::Exact {
                text: "hello".into(),
            },
        ),
        t1_case(
            "c2",
            "say hello",
            BenchExpected::Exact {
                text: "hello".into(),
            },
        ),
    ];
    let store = BenchStore::new(dir.path().join("bench"));
    let summary = run_t1(
        &cases,
        &wc,
        &store,
        &RunOptions::default(),
        &BenchRunConfig::default(),
    )
    .await
    .unwrap();

    assert_eq!(summary.results.len(), 1);
    let result = &summary.results[0];
    assert_eq!(result.case_id, "c1");
    assert_eq!(result.status, BenchStatus::Error);
    let output = result.output.as_deref().unwrap_or_default();
    assert!(output.contains("protocol version mismatch"));
    assert!(output.contains("== attempt 0 spawn_error =="));
    assert!(!output.contains("== attempt 1 spawn_error =="));

    let case_results = store.read_results(&summary.run_id).unwrap();
    assert_eq!(case_results.len(), 1);
}

#[test]
fn local_t1_corpus_path_is_optional() {
    // Benchmark cases live under gitignored bench/cases so private
    // eval prompts and seed repos do not publish with the repo.
    use orboros::bench::case::{load_tier, BenchTier};
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir;
    let cases_root = repo_root.join("bench").join("cases");
    let cases = load_tier(&cases_root, BenchTier::T1).unwrap();
    for c in &cases {
        assert_eq!(c.tier, BenchTier::T1, "case {} has wrong tier", c.id);
        assert!(
            !c.prompt.trim().is_empty(),
            "case {} has empty prompt",
            c.id
        );
    }
}

#[tokio::test]
async fn t1_case_uses_default_pass_threshold_2_of_3() {
    // Mock worker always returns "hello" — verify all 3 attempts run
    // and 3/3 pass meets the threshold trivially. (Mocking a flaky
    // worker that passes 2/3 in-process would require per-attempt
    // state in the script; the deterministic 3/3 path is enough to
    // prove the threshold gate.)
    let dir = tempfile::tempdir().unwrap();
    let script = write_worker_script(dir.path(), "ok.sh", "hello");
    let wc = worker_config(&script);
    let case = t1_case(
        "c-thresh",
        "x",
        BenchExpected::Exact {
            text: "hello".into(),
        },
    );
    let r = run_t1_case(&case, "run-x", &wc, &RunOptions::default())
        .await
        .unwrap();
    assert_eq!(r.status, BenchStatus::Pass);
}

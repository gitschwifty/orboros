//! T2/T3 benchmark runner support.
//!
//! - **T2**: copy `seed_repo` (under `bench/fixtures/<name>/`) to a
//!   tempdir, dispatch one Orboros task orb against the copy, then
//!   evaluate the expectation (`TestsPass { command }` runs the
//!   command in the copied repo and passes iff exit 0).
//! - **T3**: spawn an orboros pipeline against a greenfield prompt,
//!   then ask a grader worker (typically a cheap fast model) to
//!   score the produced artifacts against the `Rubric { criteria }`.
//!
//! Both runners return [`BenchResult`] rows in the same shape T1
//! produces so the store + CLI surface stays uniform.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use chrono::Utc;
use orbs::dep_store::DepStore;
use orbs::orb::{Orb, OrbStatus, OrbType};
use orbs::orb_store::OrbStore;
use tracing::{debug, warn};

use crate::bench::case::{BenchCase, BenchExpected, BenchTier};
use crate::bench::runner::{prompt_hash, RunOptions};
use crate::bench::store::{BenchResult, BenchStatus};
use crate::queue_loop::QueueLoop;
use crate::worker::process::WorkerConfig;

const MAX_TEST_OUTPUT_CHARS: usize = 2_000;

/// Errors specific to the T2/T3 scaffolding. These bubble out of
/// the runner without ever marking a case as Pass — anything
/// unexpected becomes `BenchStatus::Error` with the message attached.
#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("seed repo `{0}` not found under bench/fixtures/")]
    SeedRepoMissing(String),
    #[error("expected `tests_pass.command` for T2 case `{0}`")]
    MissingTestsCommand(String),
    #[error("expected `rubric.criteria` for T3 case `{0}`")]
    MissingRubric(String),
    #[error("T2 dispatch did not complete case `{0}`")]
    DispatchIncomplete(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Copies a seed repo from `<fixtures_root>/<name>` into a tempdir.
/// Returns the destination path. Uses `cp -r` for simplicity — the
/// seed repos are intentionally small.
///
/// # Errors
///
/// Returns [`HarnessError::SeedRepoMissing`] when the named seed
/// doesn't exist, or [`HarnessError::Io`] for filesystem failures.
pub fn copy_seed_repo(
    fixtures_root: &Path,
    seed_name: &Path,
    dest: &Path,
) -> Result<PathBuf, HarnessError> {
    let src = fixtures_root.join(seed_name);
    if !src.exists() {
        return Err(HarnessError::SeedRepoMissing(
            seed_name.display().to_string(),
        ));
    }
    let dest_root = dest.join(seed_name.file_name().map_or_else(
        || std::ffi::OsString::from("seed"),
        std::ffi::OsStr::to_os_string,
    ));
    std::fs::create_dir_all(&dest_root)?;
    // Recursive copy. cp -a preserves modes; we use -R for portability
    // (BSD cp doesn't honor -a on macOS the same way).
    let status = Command::new("cp")
        .arg("-R")
        .arg(format!("{}/.", src.display()))
        .arg(&dest_root)
        .status()?;
    if !status.success() {
        return Err(HarnessError::Io(std::io::Error::other(format!(
            "cp -R failed: {status}"
        ))));
    }
    Ok(dest_root)
}

/// Runs the `tests_pass` command in `cwd`. Used as the final grader
/// step for T2.
///
/// # Errors
///
/// Returns [`HarnessError::Io`] when the command cannot be spawned.
pub fn evaluate_tests_pass(cwd: &Path, command: &str) -> Result<bool, HarnessError> {
    Ok(evaluate_tests_pass_output(cwd, command)?.passed)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TestsPassOutput {
    passed: bool,
    stdout: String,
    stderr: String,
}

fn evaluate_tests_pass_output(cwd: &Path, command: &str) -> Result<TestsPassOutput, HarnessError> {
    debug!(cwd = %cwd.display(), command, "evaluating tests_pass");
    let output = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .output()?;
    Ok(TestsPassOutput {
        passed: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Builds the grader prompt for a T3 rubric. Lists the criteria with
/// `[PASS]`/`[FAIL]` markers the grader is asked to fill in, and
/// includes the produced artifact for review.
#[must_use]
pub fn build_rubric_grader_prompt(criteria: &[String], artifact: &str) -> String {
    let mut prompt = String::from(
        "You are a benchmark grader. Score the candidate artifact against the rubric. \
For each criterion, respond with `[PASS]` or `[FAIL]` followed by a short reason. \
End with a single line `OVERALL: PASS` or `OVERALL: FAIL` — pass iff every \
criterion passes.\n\nRubric:\n",
    );
    for (i, c) in criteria.iter().enumerate() {
        let _ = writeln!(prompt, "{}. {c}", i + 1);
    }
    prompt.push_str("\nCandidate artifact:\n");
    prompt.push_str(artifact);
    prompt
}

/// Parses an `OVERALL: PASS` line out of the rubric grader's
/// response. Case-insensitive on the label, picks the *last*
/// matching line in case the grader produced multiple drafts.
#[must_use]
pub fn parse_rubric_verdict(grader_response: &str) -> Option<bool> {
    grader_response.lines().rev().find_map(|line| {
        let trimmed = line.trim();
        let lower = trimmed.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("overall:") {
            let v = rest.trim();
            if v == "pass" {
                Some(true)
            } else if v == "fail" {
                Some(false)
            } else {
                None
            }
        } else {
            None
        }
    })
}

/// Runs a T2 case against a copied seed repo.
///
/// The current T2 path creates a single task orb from the case prompt,
/// promotes it through the queue loop, dispatches it with `cwd` set to
/// the copied seed repo, then grades the mutated repo with the
/// case's `tests_pass.command`.
///
/// # Errors
///
/// Returns [`HarnessError`] when the case is misshapen, the seed repo
/// is missing, dispatch cannot complete, or the test command cannot be
/// executed.
#[allow(clippy::too_many_lines)]
pub async fn run_t2_case(
    case: &BenchCase,
    run_id: &str,
    fixtures_root: &Path,
    base_worker_config: &WorkerConfig,
    _opts: &RunOptions,
) -> Result<BenchResult, HarnessError> {
    let started = Instant::now();
    if case.tier != BenchTier::T2 {
        warn!(
            case = %case.id,
            tier = ?case.tier,
            "run_t2_case called on non-T2 case"
        );
    }
    let seed = case
        .seed_repo
        .as_deref()
        .ok_or_else(|| HarnessError::SeedRepoMissing("(none specified)".into()))?;
    let command = match &case.expected {
        BenchExpected::TestsPass { command } => command.clone(),
        _ => return Err(HarnessError::MissingTestsCommand(case.id.clone())),
    };

    let temp = TempWorkDir::new(&case.id)?;
    let workdir = copy_seed_repo(fixtures_root, seed, temp.path())?;
    let state_dir = workdir.join(".orbs");
    std::fs::create_dir_all(&state_dir)?;
    let orb_store = OrbStore::new(state_dir.join("orbs.jsonl"));
    let dep_store = DepStore::new(state_dir.join("deps.jsonl"));

    let orb = Orb::new(case.name.clone(), case.prompt.clone()).with_type(OrbType::Task);
    let orb_id = orb.id.clone();
    orb_store.append(&orb)?;

    let mut wc = base_worker_config.clone();
    wc.command = command_for_fixture_cwd(&wc.command)?;
    wc.cwd = Some(workdir.clone());
    let ql = QueueLoop::new(orb_store.clone(), dep_store, workdir.clone());
    ql.tick()?;
    let completed = ql.dispatch_ready_orbs(&wc, 1).await?;
    let updated = orb_store.load_by_id(&orb_id)?.ok_or_else(|| {
        HarnessError::Io(std::io::Error::other(format!(
            "orb {orb_id} disappeared during T2 dispatch"
        )))
    })?;
    if completed == 0 {
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        return Ok(BenchResult {
            case_id: case.id.clone(),
            run_id: run_id.into(),
            tier: BenchTier::T2,
            status: BenchStatus::Error,
            score: 0.0,
            latency_ms: elapsed_ms,
            cost_cents: 1,
            iterations: 0,
            worker_model: base_worker_config.model.clone(),
            prompt_hash: prompt_hash(&case.prompt),
            system_prompt_hash: updated
                .execution
                .as_ref()
                .and_then(|e| e.system_prompt_hash.clone()),
            system_prompt_source: updated
                .execution
                .as_ref()
                .and_then(|e| e.system_prompt_source.clone()),
            confidence: updated.confidence,
            output: t2_output(updated.result.as_ref(), None),
            error: Some(
                updated
                    .result
                    .unwrap_or_else(|| format!("T2 dispatch did not complete case `{}`", case.id)),
            ),
        });
    }

    let tests = evaluate_tests_pass_output(&workdir, &command)?;
    let status = if updated.status == Some(OrbStatus::Done) && tests.passed {
        BenchStatus::Pass
    } else {
        BenchStatus::Fail
    };
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let execution = updated.execution.as_ref();

    Ok(BenchResult {
        case_id: case.id.clone(),
        run_id: run_id.into(),
        tier: BenchTier::T2,
        status,
        score: if status == BenchStatus::Pass {
            1.0
        } else {
            0.0
        },
        latency_ms: elapsed_ms,
        cost_cents: 1,
        iterations: 1,
        worker_model: base_worker_config.model.clone(),
        prompt_hash: prompt_hash(&case.prompt),
        system_prompt_hash: execution.and_then(|e| e.system_prompt_hash.clone()),
        system_prompt_source: execution.and_then(|e| e.system_prompt_source.clone()),
        confidence: updated.confidence,
        output: t2_output(updated.result.as_ref(), Some(&tests)),
        error: if updated.status == Some(OrbStatus::Done) && tests.passed {
            None
        } else if !tests.passed {
            Some(format_tests_pass_error(&command, &tests))
        } else {
            updated.result
        },
    })
}

fn t2_output(worker_result: Option<&String>, tests: Option<&TestsPassOutput>) -> Option<String> {
    let mut out = String::new();
    if let Some(result) = worker_result {
        out.push_str("== worker result ==\n");
        out.push_str(result);
        if !result.ends_with('\n') {
            out.push('\n');
        }
    }
    if let Some(tests) = tests {
        out.push_str("== tests_pass stdout ==\n");
        out.push_str(&tests.stdout);
        if !tests.stdout.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("== tests_pass stderr ==\n");
        out.push_str(&tests.stderr);
        if !tests.stderr.ends_with('\n') {
            out.push('\n');
        }
    }
    (!out.is_empty()).then_some(out)
}

fn format_tests_pass_error(command: &str, output: &TestsPassOutput) -> String {
    let mut msg = format!("tests_pass command failed: {command}");
    let stdout = truncate_for_error(output.stdout.trim());
    let stderr = truncate_for_error(output.stderr.trim());
    if !stdout.is_empty() {
        msg.push_str("\nstdout:\n");
        msg.push_str(&stdout);
    }
    if !stderr.is_empty() {
        msg.push_str("\nstderr:\n");
        msg.push_str(&stderr);
    }
    msg
}

fn truncate_for_error(text: &str) -> String {
    let mut out: String = text.chars().take(MAX_TEST_OUTPUT_CHARS).collect();
    if text.chars().count() > MAX_TEST_OUTPUT_CHARS {
        out.push_str("\n...<truncated>");
    }
    out
}

fn command_for_fixture_cwd(command: &str) -> Result<String, HarnessError> {
    let path = Path::new(command);
    if path.is_absolute() || path.components().count() == 1 {
        return Ok(command.into());
    }
    Ok(std::env::current_dir()?.join(path).display().to_string())
}

struct TempWorkDir {
    path: PathBuf,
}

impl TempWorkDir {
    fn new(case_id: &str) -> Result<Self, HarnessError> {
        let path =
            std::env::temp_dir().join(format!("orboros-bench-{case_id}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempWorkDir {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.path) {
            tracing::warn!(
                path = %self.path.display(),
                error = %e,
                "failed to clean up T2 benchmark tempdir"
            );
        }
    }
}

/// Stub T3 runner. Same shape as the T2 stub.
///
/// # Errors
///
/// Currently only via misshapen expectation or I/O.
pub fn run_t3_case_stub(
    case: &BenchCase,
    run_id: &str,
    _opts: &RunOptions,
) -> Result<BenchResult, HarnessError> {
    let started = Instant::now();
    if case.tier != BenchTier::T3 {
        warn!(
            case = %case.id,
            tier = ?case.tier,
            "run_t3_case_stub called on non-T3 case"
        );
    }
    let _criteria = match &case.expected {
        BenchExpected::Rubric { criteria } => criteria.clone(),
        _ => return Err(HarnessError::MissingRubric(case.id.clone())),
    };
    let _ = Utc::now(); // scaffolding placeholder

    Ok(BenchResult {
        case_id: case.id.clone(),
        run_id: run_id.into(),
        tier: BenchTier::T3,
        status: BenchStatus::Error,
        score: 0.0,
        latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        cost_cents: 0,
        iterations: 0,
        worker_model: String::new(),
        prompt_hash: prompt_hash(&case.prompt),
        system_prompt_hash: None,
        system_prompt_source: None,
        confidence: None,
        output: None,
        error: Some(format!(
            "T3 runner is scaffolded but not yet wired to a greenfield pipeline + rubric grader (case {})",
            case.id
        )),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t2_case_with_seed(id: &str, seed: &str, command: &str) -> BenchCase {
        BenchCase {
            id: id.into(),
            tier: BenchTier::T2,
            name: id.into(),
            description: "test".into(),
            prompt: "p".into(),
            expected: BenchExpected::TestsPass {
                command: command.into(),
            },
            seed_repo: Some(PathBuf::from(seed)),
            timeout_s: 60,
            max_cost_cents: 100,
        }
    }

    fn worker_config(script: &Path) -> WorkerConfig {
        WorkerConfig {
            command: "bash".into(),
            args: vec![script.to_string_lossy().into()],
            cwd: None,
            env: vec![],
            model: "mock/t2".into(),
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

    fn write_editing_worker(dir: &Path) -> PathBuf {
        let path = dir.join("worker.sh");
        let body = r#"while IFS= read -r line; do
  type=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['type'])" 2>/dev/null)
  id=$(echo "$line" | python3 -c "import sys,json; print(json.loads(sys.stdin.read())['id'])" 2>/dev/null)
  case "$type" in
    init) echo "{\"type\":\"init_ok\",\"id\":\"$id\",\"session_id\":\"s\",\"protocol_version\":\"0.2.0\"}" ;;
    send) printf 'done\n' > result.txt; echo "{\"type\":\"result\",\"id\":\"$id\",\"status\":\"ok\",\"response\":\"edited\",\"tool_calls_made\":[],\"iterations\":1,\"confidence\":0.86}" ;;
    shutdown) echo "{\"type\":\"shutdown_ok\",\"id\":\"$id\"}"; exit 0 ;;
  esac
done
"#;
        std::fs::write(&path, body).unwrap();
        path
    }

    // ── copy_seed_repo ────────────────────────────────────────

    #[test]
    fn copy_seed_repo_copies_files_recursively() {
        let dir = tempfile::tempdir().unwrap();
        let src_root = dir.path().join("fixtures").join("small");
        std::fs::create_dir_all(src_root.join("inner")).unwrap();
        std::fs::write(src_root.join("README"), "hi").unwrap();
        std::fs::write(src_root.join("inner").join("a.txt"), "a").unwrap();

        let dest = dir.path().join("work");
        std::fs::create_dir_all(&dest).unwrap();
        let copied =
            copy_seed_repo(&dir.path().join("fixtures"), Path::new("small"), &dest).unwrap();

        assert!(copied.join("README").exists());
        assert!(copied.join("inner").join("a.txt").exists());
    }

    #[test]
    fn copy_seed_repo_missing_fixture_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("fixtures")).unwrap();
        let dest = dir.path().join("work");
        std::fs::create_dir_all(&dest).unwrap();
        let err =
            copy_seed_repo(&dir.path().join("fixtures"), Path::new("nope"), &dest).unwrap_err();
        assert!(matches!(err, HarnessError::SeedRepoMissing(_)));
    }

    // ── evaluate_tests_pass ───────────────────────────────────

    #[test]
    fn tests_pass_true_for_exit_zero() {
        let dir = tempfile::tempdir().unwrap();
        assert!(evaluate_tests_pass(dir.path(), "true").unwrap());
    }

    #[test]
    fn tests_pass_false_for_exit_nonzero() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!evaluate_tests_pass(dir.path(), "false").unwrap());
    }

    #[test]
    fn tests_pass_output_captures_stderr_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let output = evaluate_tests_pass_output(dir.path(), "echo nope >&2; exit 1").unwrap();
        assert!(!output.passed);
        assert!(output.stderr.contains("nope"));
    }

    // ── rubric grader prompt + parser ─────────────────────────

    #[test]
    fn rubric_prompt_lists_criteria_and_artifact() {
        let p =
            build_rubric_grader_prompt(&["compiles".into(), "has tests".into()], "fn main() {}");
        assert!(p.contains("benchmark grader"));
        assert!(p.contains("1. compiles"));
        assert!(p.contains("2. has tests"));
        assert!(p.contains("fn main()"));
    }

    #[test]
    fn rubric_parser_finds_overall_pass() {
        let r = "[PASS] criterion 1\n[PASS] criterion 2\nOVERALL: PASS";
        assert_eq!(parse_rubric_verdict(r), Some(true));
    }

    #[test]
    fn rubric_parser_finds_overall_fail() {
        let r = "[FAIL] criterion 1\nOVERALL: FAIL";
        assert_eq!(parse_rubric_verdict(r), Some(false));
    }

    #[test]
    fn rubric_parser_is_case_insensitive() {
        assert_eq!(parse_rubric_verdict("overall: pass"), Some(true));
        assert_eq!(parse_rubric_verdict("Overall: Fail"), Some(false));
    }

    #[test]
    fn rubric_parser_uses_last_overall_when_multiple() {
        let r = "OVERALL: FAIL\n(reviewing again)\nOVERALL: PASS";
        assert_eq!(parse_rubric_verdict(r), Some(true));
    }

    #[test]
    fn rubric_parser_returns_none_when_absent_or_garbled() {
        assert_eq!(parse_rubric_verdict("no verdict line here"), None);
        assert_eq!(parse_rubric_verdict("OVERALL: maybe"), None);
    }

    // ── T2 runner ─────────────────────────────────────────────

    #[tokio::test]
    async fn t2_runner_dispatches_worker_and_grades_seed_repo() {
        let dir = tempfile::tempdir().unwrap();
        let fixtures = dir.path().join("fixtures");
        std::fs::create_dir_all(fixtures.join("small")).unwrap();
        std::fs::write(fixtures.join("small").join("README"), "hi").unwrap();
        let script = write_editing_worker(dir.path());
        let wc = worker_config(&script);

        let case = t2_case_with_seed("t2-1", "small", "test \"$(cat result.txt)\" = done");
        let r = run_t2_case(&case, "run-x", &fixtures, &wc, &RunOptions::default())
            .await
            .unwrap();
        assert_eq!(r.status, BenchStatus::Pass);
        assert!((r.score - 1.0).abs() < f32::EPSILON);
        assert_eq!(r.tier, BenchTier::T2);
        assert_eq!(r.worker_model, "mock/t2");
        assert_eq!(r.confidence, Some(0.86));
        assert!(r.system_prompt_hash.is_some());
        let output = r.output.unwrap();
        assert!(output.contains("== worker result =="));
        assert!(output.contains("edited"));
        assert!(output.contains("== tests_pass stdout =="));
        assert!(output.contains("== tests_pass stderr =="));
    }

    #[tokio::test]
    async fn t2_runner_errors_when_seed_missing() {
        let dir = tempfile::tempdir().unwrap();
        let fixtures = dir.path().join("fixtures");
        std::fs::create_dir_all(&fixtures).unwrap();
        let script = write_editing_worker(dir.path());
        let wc = worker_config(&script);
        let case = t2_case_with_seed("t2-x", "nope", "true");
        let err = run_t2_case(&case, "run-x", &fixtures, &wc, &RunOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, HarnessError::SeedRepoMissing(_)));
    }

    #[tokio::test]
    async fn t2_runner_records_worker_failure_message() {
        let dir = tempfile::tempdir().unwrap();
        let fixtures = dir.path().join("fixtures");
        std::fs::create_dir_all(fixtures.join("small")).unwrap();
        std::fs::write(fixtures.join("small").join("README"), "hi").unwrap();
        let mut wc = worker_config(Path::new("unused"));
        wc.command = "definitely-not-an-orboros-worker".into();
        wc.args = vec![];

        let case = t2_case_with_seed("t2-fail", "small", "true");
        let r = run_t2_case(&case, "run-x", &fixtures, &wc, &RunOptions::default())
            .await
            .unwrap();
        assert_eq!(r.status, BenchStatus::Error);
        let err = r.error.unwrap();
        assert!(
            err.contains("worker spawn failed"),
            "expected worker failure details, got {err}"
        );
        assert!(r
            .output
            .as_deref()
            .is_some_and(|out| out.contains("worker spawn failed")));
    }

    #[test]
    fn t3_stub_returns_error_status_with_message() {
        let case = BenchCase {
            id: "t3-1".into(),
            tier: BenchTier::T3,
            name: "n".into(),
            description: "d".into(),
            prompt: "p".into(),
            expected: BenchExpected::Rubric {
                criteria: vec!["builds".into()],
            },
            seed_repo: None,
            timeout_s: 60,
            max_cost_cents: 100,
        };
        let r = run_t3_case_stub(&case, "run-x", &RunOptions::default()).unwrap();
        assert_eq!(r.status, BenchStatus::Error);
        assert!(r.error.is_some());
    }
}

//! T2/T3 benchmark runner scaffolding.
//!
//! Not wired into a real pipeline yet — these stubs prove out the
//! shape of the runner contract so that authoring the first T2 or T3
//! cases is straightforward when we get there. The harness expects:
//!
//! - **T2**: copy `seed_repo` (under `bench/fixtures/<name>/`) to a
//!   tempdir, run `orboros plan` + `orboros run` against the copy,
//!   then evaluate the expectation (`TestsPass { command }` runs the
//!   command in the tempdir and passes iff exit 0).
//! - **T3**: spawn an orboros pipeline against a greenfield prompt,
//!   then ask a grader worker (typically a cheap fast model) to
//!   score the produced artifacts against the `Rubric { criteria }`.
//!
//! Both runners return [`BenchResult`] rows in the same shape T1
//! produces so the store + CLI surface stays uniform.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use chrono::Utc;
use tracing::{debug, warn};

use crate::bench::case::{BenchCase, BenchExpected, BenchTier};
use crate::bench::runner::{prompt_hash, RunOptions};
use crate::bench::store::{BenchResult, BenchStatus};

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
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Copies a seed repo from `<fixtures_root>/<name>` into a tempdir.
/// Returns the destination path (the tempdir is kept alive by the
/// caller's `TempDir` handle). Uses `cp -r` for simplicity — the
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
    let dest_root = dest.join(
        seed_name
            .file_name()
            .map(std::ffi::OsStr::to_os_string)
            .unwrap_or_else(|| std::ffi::OsString::from("seed")),
    );
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
    debug!(cwd = %cwd.display(), command, "evaluating tests_pass");
    let status = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .status()?;
    Ok(status.success())
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
        prompt.push_str(&format!("{}. {c}\n", i + 1));
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

/// Stub T2 runner. Currently returns an Error result with a message
/// noting the harness isn't wired up to spawn the orboros pipeline
/// yet — the corpus authoring task (separate) will land alongside
/// the real pipeline integration. The function is here so the
/// dispatcher in `bench::cmd` can call it without conditional
/// imports.
///
/// # Errors
///
/// Currently only via I/O failure copying the seed repo.
pub fn run_t2_case_stub(
    case: &BenchCase,
    run_id: &str,
    fixtures_root: &Path,
    _opts: &RunOptions,
) -> Result<BenchResult, HarnessError> {
    let started = Instant::now();
    // Validate the case at least gestures at T2 shape.
    if case.tier != BenchTier::T2 {
        warn!(
            case = %case.id,
            tier = ?case.tier,
            "run_t2_case_stub called on non-T2 case"
        );
    }
    let seed = case
        .seed_repo
        .as_deref()
        .ok_or_else(|| HarnessError::SeedRepoMissing("(none specified)".into()))?;
    let _cmd = match &case.expected {
        BenchExpected::TestsPass { command } => command.clone(),
        _ => return Err(HarnessError::MissingTestsCommand(case.id.clone())),
    };

    // Verify the fixture exists so seed-repo authoring errors surface
    // even before the real runner is wired up. Actual copy + pipeline
    // invocation lands with the corpus authoring follow-up.
    let src = fixtures_root.join(seed);
    if !src.exists() {
        return Err(HarnessError::SeedRepoMissing(seed.display().to_string()));
    }

    Ok(BenchResult {
        case_id: case.id.clone(),
        run_id: run_id.into(),
        tier: BenchTier::T2,
        status: BenchStatus::Error,
        score: 0.0,
        latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        cost_cents: 0,
        iterations: 0,
        worker_model: String::new(),
        prompt_hash: prompt_hash(&case.prompt),
        confidence: None,
        error: Some(format!(
            "T2 runner is scaffolded but not yet wired to the orboros pipeline (case {})",
            case.id
        )),
    })
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
        confidence: None,
        error: Some(format!(
            "T3 runner is scaffolded but not yet wired to a greenfield pipeline + rubric grader (case {})",
            case.id
        )),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t2_case_with_seed(id: &str, seed: &str) -> BenchCase {
        BenchCase {
            id: id.into(),
            tier: BenchTier::T2,
            name: id.into(),
            description: "test".into(),
            prompt: "p".into(),
            expected: BenchExpected::TestsPass {
                command: "true".into(),
            },
            seed_repo: Some(PathBuf::from(seed)),
            timeout_s: 60,
            max_cost_cents: 100,
        }
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

    // ── stub runners ──────────────────────────────────────────

    #[test]
    fn t2_stub_returns_error_status_with_message() {
        let dir = tempfile::tempdir().unwrap();
        let fixtures = dir.path().join("fixtures");
        std::fs::create_dir_all(fixtures.join("small")).unwrap();
        std::fs::write(fixtures.join("small").join("README"), "hi").unwrap();

        let case = t2_case_with_seed("t2-1", "small");
        let r = run_t2_case_stub(&case, "run-x", &fixtures, &RunOptions::default()).unwrap();
        assert_eq!(r.status, BenchStatus::Error);
        assert!(r.error.is_some());
        assert_eq!(r.tier, BenchTier::T2);
    }

    #[test]
    fn t2_stub_errors_when_seed_missing() {
        let dir = tempfile::tempdir().unwrap();
        let fixtures = dir.path().join("fixtures");
        std::fs::create_dir_all(&fixtures).unwrap();
        let case = t2_case_with_seed("t2-x", "nope");
        let err = run_t2_case_stub(&case, "run-x", &fixtures, &RunOptions::default()).unwrap_err();
        assert!(matches!(err, HarnessError::SeedRepoMissing(_)));
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

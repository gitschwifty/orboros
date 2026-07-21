//! Append-only JSONL store for benchmark results.
//!
//! Layout under `.orbs/bench/`:
//!   - `runs.jsonl` — one [`BenchRun`] per line, the index of every
//!     run the harness has produced.
//!   - `results-<run_id>.jsonl` — one [`BenchResult`] per line for
//!     the case results within a run.
//!
//! The split keeps `runs.jsonl` small enough to scan for the CLI's
//! `bench list-runs` while letting individual case results scale
//! arbitrarily without bloating the index.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::bench::case::BenchTier;

/// Outcome of a single case execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchStatus {
    /// All N runs passed (or majority threshold met).
    Pass,
    /// Some passed and some failed, threshold not met.
    Fail,
    /// Harness couldn't complete the case (timeout, worker crash,
    /// budget cut, malformed expectation).
    Error,
    /// Case was skipped (e.g. tier filter or runtime gating).
    Skipped,
}

impl BenchStatus {
    #[must_use]
    pub fn is_pass(self) -> bool {
        matches!(self, BenchStatus::Pass)
    }
}

/// Per-case row written to `results-<run_id>.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchResult {
    pub case_id: String,
    pub run_id: String,
    pub tier: BenchTier,
    pub status: BenchStatus,
    /// Pass rate across N=3 (or however many) attempts, in `[0.0, 1.0]`.
    pub score: f32,
    pub latency_ms: u64,
    pub cost_cents: u32,
    pub iterations: u32,
    pub worker_model: String,
    /// SHA-256 of the prompt sent to the worker, hex-encoded —
    /// lets `bench compare` detect when the prompt changed between
    /// runs and warn before comparing.
    pub prompt_hash: String,
    /// SHA-256 of the resolved system prompt used by the worker.
    /// This separates benchmark case prompt drift from harness /
    /// role prompt drift.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_hash: Option<String>,
    /// Where the resolved system prompt came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_source: Option<String>,
    /// Worker-reported confidence, if any. Populated when the worker
    /// emits a CONFIDENCE: line or IPC field (task 57). Pairs with
    /// the calibration analysis (sub-task 59.7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    /// Captured benchmark output for later inspection. This is not
    /// printed in the default table; use the JSONL result file or
    /// `jq -r '.output'` when detailed worker/grader output is needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Free-form error message when `status == Error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Summary row written to `runs.jsonl` once per run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchRun {
    pub run_id: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub tier: Option<BenchTier>,
    /// Human-readable run variant label, e.g. `sonnet-baseline` or
    /// `kimi-candidate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    /// CLI/config selector used to choose the worker model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_selector: Option<String>,
    /// Catalog key when `model_selector` resolved through
    /// `[models.options.<key>]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_key: Option<String>,
    /// Resolved model string sent to the worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_model: Option<String>,
    /// Resolved model string intended for grader/reviewer roles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grader_model: Option<String>,
    /// Prompt variant label once prompt candidate loading is wired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_variant: Option<String>,
    /// Corpus root used for the run, for provenance when cases live
    /// in a sibling private repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cases_root: Option<String>,
    pub total: u32,
    pub passed: u32,
    pub failed: u32,
    pub errored: u32,
    pub skipped: u32,
    /// SHA-256 of the resolved harness config (model + prompt
    /// addendum + threshold + sampling rate, etc.) hex-encoded.
    /// Used by `bench compare` for warning on config drift.
    pub config_hash: String,
    /// Total cost across all cases in this run.
    pub total_cost_cents: u32,
}

/// JSONL store at `<bench_dir>/`. Operations are append-only on disk;
/// the type itself is stateless.
#[derive(Debug, Clone)]
pub struct BenchStore {
    bench_dir: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to serialize entry: {0}")]
    Encode(#[from] serde_json::Error),
}

impl BenchStore {
    /// Creates a store rooted at `bench_dir`. The directory is created
    /// on the first write — no error if it doesn't exist yet.
    #[must_use]
    pub fn new(bench_dir: impl Into<PathBuf>) -> Self {
        Self {
            bench_dir: bench_dir.into(),
        }
    }

    /// Path to the runs index file.
    #[must_use]
    pub fn runs_path(&self) -> PathBuf {
        self.bench_dir.join("runs.jsonl")
    }

    /// Path to the per-result file for a given run.
    #[must_use]
    pub fn results_path(&self, run_id: &str) -> PathBuf {
        self.bench_dir.join(format!("results-{run_id}.jsonl"))
    }

    /// Appends a result row to `results-<run_id>.jsonl`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on I/O or serialization failure.
    pub fn append_result(&self, result: &BenchResult) -> Result<(), StoreError> {
        ensure_dir(&self.bench_dir)?;
        let path = self.results_path(&result.run_id);
        append_jsonl(&path, result)
    }

    /// Appends a run summary row to `runs.jsonl`.
    ///
    /// # Errors
    ///
    /// As [`Self::append_result`].
    pub fn append_run(&self, run: &BenchRun) -> Result<(), StoreError> {
        ensure_dir(&self.bench_dir)?;
        let path = self.runs_path();
        append_jsonl(&path, run)
    }

    /// Reads all run summaries (oldest first). Skips malformed lines
    /// — old rows from a prior schema shouldn't crash the CLI.
    ///
    /// # Errors
    ///
    /// Returns I/O errors. A missing file yields `Ok(vec![])`.
    pub fn read_runs(&self) -> Result<Vec<BenchRun>, StoreError> {
        read_jsonl(&self.runs_path())
    }

    /// Reads all per-case results for one run.
    ///
    /// # Errors
    ///
    /// As [`Self::read_runs`].
    pub fn read_results(&self, run_id: &str) -> Result<Vec<BenchResult>, StoreError> {
        read_jsonl(&self.results_path(run_id))
    }
}

fn ensure_dir(dir: &Path) -> Result<(), StoreError> {
    std::fs::create_dir_all(dir).map_err(|e| StoreError::Io {
        path: dir.to_path_buf(),
        source: e,
    })
}

fn append_jsonl<T: Serialize>(path: &Path, value: &T) -> Result<(), StoreError> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| StoreError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
    file.write_all(line.as_bytes())
        .map_err(|e| StoreError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
    Ok(())
}

fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Vec<T>, StoreError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = std::fs::File::open(path).map_err(|e| StoreError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<T>(&line) {
            out.push(v);
        }
    }
    Ok(out)
}

/// Generates a fresh run id of the shape `bench-YYYYMMDDHHMMSS-<8 hex>`.
/// Used by the harness to label a new run before any results are written.
#[must_use]
pub fn new_run_id() -> String {
    let now = Utc::now();
    let suffix: u32 = rand::random();
    format!("bench-{}-{:08x}", now.format("%Y%m%d%H%M%S"), suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_result(run_id: &str, case_id: &str) -> BenchResult {
        BenchResult {
            case_id: case_id.into(),
            run_id: run_id.into(),
            tier: BenchTier::T1,
            status: BenchStatus::Pass,
            score: 1.0,
            latency_ms: 1234,
            cost_cents: 3,
            iterations: 1,
            worker_model: "mock/test".into(),
            prompt_hash: "deadbeef".into(),
            system_prompt_hash: Some("cafe".into()),
            system_prompt_source: Some("built_in".into()),
            confidence: Some(0.88),
            output: Some("details".into()),
            error: None,
        }
    }

    fn sample_run(run_id: &str) -> BenchRun {
        BenchRun {
            run_id: run_id.into(),
            started_at: Utc::now(),
            finished_at: Utc::now(),
            tier: Some(BenchTier::T1),
            variant: Some("baseline".into()),
            model_selector: Some("fast".into()),
            model_key: Some("fast".into()),
            worker_model: Some("mock/test".into()),
            grader_model: Some("mock/grader".into()),
            prompt_variant: None,
            cases_root: Some("bench/cases".into()),
            total: 3,
            passed: 2,
            failed: 1,
            errored: 0,
            skipped: 0,
            config_hash: "feedface".into(),
            total_cost_cents: 9,
        }
    }

    // ── id generation ─────────────────────────────────────────

    #[test]
    fn new_run_id_format() {
        let id = new_run_id();
        assert!(id.starts_with("bench-"), "got {id}");
        let parts: Vec<&str> = id.splitn(3, '-').collect();
        assert_eq!(parts.len(), 3, "expected 3 dash-separated parts: {id}");
        // Timestamp section is 14 chars (YYYYMMDDHHMMSS).
        assert_eq!(parts[1].len(), 14);
        assert_eq!(parts[2].len(), 8);
    }

    #[test]
    fn new_run_id_is_unique() {
        let a = new_run_id();
        let b = new_run_id();
        assert_ne!(a, b, "subsequent ids collided: {a}");
    }

    // ── append + read ─────────────────────────────────────────

    #[test]
    fn append_result_creates_dir_and_writes_line() {
        let dir = tempfile::tempdir().unwrap();
        let store = BenchStore::new(dir.path().join("bench"));
        let r = sample_result("run-1", "case-a");
        store.append_result(&r).unwrap();
        let read = store.read_results("run-1").unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0], r);
    }

    #[test]
    fn append_run_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = BenchStore::new(dir.path().join("bench"));
        let r = sample_run("run-x");
        store.append_run(&r).unwrap();
        let read = store.read_runs().unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0], r);
    }

    #[test]
    fn results_for_different_runs_are_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let store = BenchStore::new(dir.path().join("bench"));
        store
            .append_result(&sample_result("run-a", "case-1"))
            .unwrap();
        store
            .append_result(&sample_result("run-b", "case-1"))
            .unwrap();
        store
            .append_result(&sample_result("run-a", "case-2"))
            .unwrap();

        let a = store.read_results("run-a").unwrap();
        let b = store.read_results("run-b").unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(b.len(), 1);
        assert!(a.iter().all(|r| r.run_id == "run-a"));
    }

    #[test]
    fn read_runs_missing_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = BenchStore::new(dir.path().join("bench"));
        assert!(store.read_runs().unwrap().is_empty());
        assert!(store.read_results("nonexistent").unwrap().is_empty());
    }

    #[test]
    fn malformed_jsonl_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let store = BenchStore::new(dir.path().join("bench"));
        // Hand-write a malformed line, then append a valid one.
        std::fs::create_dir_all(dir.path().join("bench")).unwrap();
        std::fs::write(store.runs_path(), "{not valid}\n").unwrap();
        store.append_run(&sample_run("run-after-bad")).unwrap();
        let runs = store.read_runs().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].run_id, "run-after-bad");
    }

    // ── BenchStatus helpers ───────────────────────────────────

    #[test]
    fn bench_status_is_pass_only_for_pass() {
        assert!(BenchStatus::Pass.is_pass());
        assert!(!BenchStatus::Fail.is_pass());
        assert!(!BenchStatus::Error.is_pass());
        assert!(!BenchStatus::Skipped.is_pass());
    }
}

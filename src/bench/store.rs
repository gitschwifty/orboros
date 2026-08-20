//! Append-only JSONL store for benchmark results.
//!
//! Layout under the benchmark results directory:
//!   - `runs.jsonl` - one [`BenchRun`] per line, the index of every
//!     run the harness has produced.
//!   - `YYYY-MM-DD/<run_id>/run.json` - summary for one run.
//!   - `YYYY-MM-DD/<run_id>/results.jsonl` - one [`BenchResult`] per
//!     line for the case results within a run.
//!
//! The split keeps `runs.jsonl` small enough to scan for the CLI's
//! `bench list-runs` while keeping each run's artifacts in a
//! self-contained dated directory.

use std::collections::{BTreeSet, HashMap};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::bench::case::{BenchGrader, BenchResourceGuidance, BenchTaxonomy, BenchTier};
use crate::bench::prompts::PromptManifest;

/// Deterministic identity for the evaluated benchmark suite, deliberately
/// separate from source-commit and runtime/config provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchSuiteManifest {
    pub fingerprint: String,
    pub cases: Vec<BenchSuiteCase>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_manifest_sha256: Option<String>,
}

/// Content identities contributing to one selected case in a suite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchSuiteCase {
    pub id: String,
    pub selector: String,
    pub case_toml_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixture_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay_sha256: Option<String>,
    /// Classification snapshot from the case definition. This permits results
    /// repositories to group historical runs after the private corpus changes.
    #[serde(default, skip_serializing_if = "BenchTaxonomy::is_empty")]
    pub taxonomy: BenchTaxonomy,
    /// Rubric identity snapshot for task-specific AI grading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grader: Option<BenchGrader>,
}

/// A per-dispatch execution record retained at benchmark run scope.
///
/// The flattened execution fields make this useful after case artifacts have
/// been pruned; `case_id` identifies which benchmark case produced it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchDispatchRecord {
    pub case_id: String,
    #[serde(flatten)]
    pub execution: crate::execution::ExecutionRecord,
}

/// A prompt snapshot retained at benchmark run scope. Kept separate from
/// dispatch outcomes so callers can inspect comparable worker inputs after
/// artifacts and transcripts have been pruned.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchPromptRecord {
    pub case_id: String,
    #[serde(flatten)]
    pub prompt: crate::execution::PromptRecord,
}

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

/// Per-case row written to a run's `results.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchResult {
    pub case_id: String,
    pub run_id: String,
    pub tier: BenchTier,
    pub status: BenchStatus,
    /// Pass rate across N=3 (or however many) attempts, in `[0.0, 1.0]`.
    pub score: f32,
    /// Independent score for an optional case process contract. `None` means
    /// the case does not assess process behavior and must not affect process
    /// averages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_score: Option<f32>,
    /// Human-readable unmet process requirements. Retained in detailed JSONL
    /// and result views, but intentionally omitted from the summary table.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub process_annotations: Vec<String>,
    /// Exact non-failing resource guidance selected for this case at run
    /// startup. This snapshots evaluation semantics so historical displays do
    /// not change when the corpus's case TOML is later edited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_guidance: Option<BenchResourceGuidance>,
    /// Wall-clock elapsed time; retained as `latency_ms` for schema
    /// compatibility. This is not provider/model latency.
    pub latency_ms: u64,
    /// Provider/model latency reported by Heddle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_latency_ms: Option<u64>,
    /// Tool execution latency reported by Heddle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_latency_ms: Option<u64>,
    /// Heddle-reported total worker latency.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_latency_ms: Option<u64>,
    /// Actual provider cost in cents, when the worker reports it or
    /// the harness can price it accurately. `None` means unknown;
    /// benchmark code must not write placeholder estimates here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_cents: Option<u64>,
    /// Exact provider cost in micro-dollars. New results use this field for
    /// aggregation and display; `cost_cents` remains for compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_micros: Option<u64>,
    pub iterations: u32,
    /// Number of assistant/model turns reported by the worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_turns: Option<u32>,
    /// Number of tool calls reported by the worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    /// Provider-reported prompt cache read tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    /// Provider-reported prompt cache write tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
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

/// Summary row written to `runs.jsonl` and the run's `run.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchRun {
    pub run_id: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub tier: Option<BenchTier>,
    /// All tiers included in the run. Older summaries may only have `tier`;
    /// readers should fall back to `tier` when this is empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<BenchTier>,
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
    /// Selected prompt files, composition order, and resulting role hashes.
    /// Detailed inputs are also copied beneath the run artifact directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_manifest: Option<PromptManifest>,
    /// Evaluated suite identity and its content-addressed inputs. Historical
    /// runs predate this field and are intentionally non-comparable by suite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suite_manifest: Option<BenchSuiteManifest>,
    /// Corpus root used for the run, for provenance when cases live
    /// in a sibling private repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cases_root: Option<String>,
    /// Benchmark config file overlaid after normal Orboros config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bench_config_path: Option<String>,
    /// Git commit for the Orboros source used to run this benchmark.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orboros_commit: Option<String>,
    /// Git commit for the benchmark corpus repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bench_commit: Option<String>,
    /// Whether the Orboros worktree was dirty at benchmark startup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orboros_dirty: Option<bool>,
    /// Whether the benchmark corpus worktree was dirty at benchmark startup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bench_dirty: Option<bool>,
    pub total: u32,
    pub passed: u32,
    pub failed: u32,
    pub errored: u32,
    pub skipped: u32,
    /// SHA-256 of the resolved harness config (model + prompt
    /// addendum + threshold + sampling rate, etc.) hex-encoded.
    /// Used by `bench compare` for warning on config drift.
    pub config_hash: String,
    /// Total known cost across cases. `None` means no case reported
    /// actual cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cost_cents: Option<u64>,
    /// Exact aggregate provider cost in micro-dollars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cost_micros: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_turns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<u64>,
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

    /// Directory for one run's artifacts.
    #[must_use]
    pub fn run_dir(&self, run_id: &str) -> PathBuf {
        self.bench_dir.join(run_date_dir(run_id)).join(run_id)
    }

    /// Path to one run's summary copy.
    #[must_use]
    pub fn run_summary_path(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("run.json")
    }

    /// Path to the per-result file for a given run.
    #[must_use]
    pub fn results_path(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("results.jsonl")
    }

    /// Path to the durable per-dispatch benchmark evidence for one run.
    #[must_use]
    pub fn dispatches_path(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("dispatches.jsonl")
    }

    /// Path to durable per-dispatch prompt snapshots for one run.
    #[must_use]
    pub fn prompts_path(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("prompts.jsonl")
    }

    /// Directory containing retained compact orb state, grouped by case.
    #[must_use]
    pub fn case_orbs_dir(&self, run_id: &str, case_id: &str) -> PathBuf {
        self.run_dir(run_id)
            .join("orbs")
            .join(sanitize_path_component(case_id))
    }

    /// Directory for artifacts captured from one case within a run.
    #[must_use]
    pub fn case_artifact_dir(&self, run_id: &str, case_id: &str) -> PathBuf {
        self.run_dir(run_id)
            .join("artifacts")
            .join(sanitize_path_component(case_id))
    }

    /// Appends a result row to `<date>/<run_id>/results.jsonl`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on I/O or serialization failure.
    pub fn append_result(&self, result: &BenchResult) -> Result<(), StoreError> {
        ensure_dir(&self.run_dir(&result.run_id))?;
        let path = self.results_path(&result.run_id);
        append_jsonl(&path, result)
    }

    /// Copies a case's execution ledger into durable run-level evidence.
    pub fn append_dispatches(
        &self,
        run_id: &str,
        case_id: &str,
        records: &[crate::execution::ExecutionRecord],
    ) -> Result<(), StoreError> {
        ensure_dir(&self.run_dir(run_id))?;
        let path = self.dispatches_path(run_id);
        for execution in records {
            append_jsonl(
                &path,
                &BenchDispatchRecord {
                    case_id: case_id.into(),
                    execution: execution.clone(),
                },
            )?;
        }
        Ok(())
    }

    /// Copies prompt snapshots into durable run-level evidence, tagged by
    /// benchmark case so the workdir may be pruned later.
    pub fn append_prompts(
        &self,
        run_id: &str,
        case_id: &str,
        records: &[crate::execution::PromptRecord],
    ) -> Result<(), StoreError> {
        ensure_dir(&self.run_dir(run_id))?;
        let path = self.prompts_path(run_id);
        for prompt in records {
            append_jsonl(
                &path,
                &BenchPromptRecord {
                    case_id: case_id.into(),
                    prompt: prompt.clone(),
                },
            )?;
        }
        Ok(())
    }

    /// Retains the compact graph/lifecycle evidence from a case workdir.
    /// Dispatch telemetry is deliberately excluded because it is retained in
    /// the run-level `dispatches.jsonl` with a case identifier.
    pub fn retain_orb_state(
        &self,
        run_id: &str,
        case_id: &str,
        source_orbs_dir: &Path,
    ) -> Result<(), StoreError> {
        let destination = self.case_orbs_dir(run_id, case_id);
        for name in ["orbs.jsonl", "deps.jsonl"] {
            let source = source_orbs_dir.join(name);
            if source.exists() {
                ensure_dir(&destination)?;
                std::fs::copy(&source, destination.join(name)).map_err(|source_err| {
                    StoreError::Io {
                        path: source,
                        source: source_err,
                    }
                })?;
            }
        }
        Ok(())
    }

    /// Appends a run summary row to `runs.jsonl`.
    ///
    /// # Errors
    ///
    /// As [`Self::append_result`].
    pub fn append_run(&self, run: &BenchRun) -> Result<(), StoreError> {
        ensure_dir(&self.bench_dir)?;
        ensure_dir(&self.run_dir(&run.run_id))?;
        append_jsonl(&self.runs_path(), run)?;
        write_json(&self.run_summary_path(&run.run_id), run)
    }

    /// Reads all run summaries (oldest first). Skips malformed lines
    /// — old rows from a prior schema shouldn't crash the CLI.
    ///
    /// # Errors
    ///
    /// Returns I/O errors. A missing file yields `Ok(vec![])`.
    pub fn read_runs(&self) -> Result<Vec<BenchRun>, StoreError> {
        let mut runs: Vec<BenchRun> = dedupe_runs_last_wins(read_jsonl(&self.runs_path())?);
        let mut seen: BTreeSet<String> = runs.iter().map(|run| run.run_id.clone()).collect();
        for run in discover_run_summaries(&self.bench_dir)? {
            if seen.insert(run.run_id.clone()) {
                runs.push(run);
            }
        }
        runs.sort_by_key(|run| run.started_at);
        for run in &mut runs {
            normalize_legacy_variant(run);
        }
        Ok(runs)
    }

    /// Reads all per-case results for one run.
    ///
    /// # Errors
    ///
    /// As [`Self::read_runs`].
    pub fn read_results(&self, run_id: &str) -> Result<Vec<BenchResult>, StoreError> {
        read_jsonl(&self.results_path(run_id))
    }

    /// Reads durable per-dispatch telemetry for one run. Historical runs that
    /// predate the ledger simply return an empty collection.
    ///
    /// # Errors
    ///
    /// As [`Self::read_runs`].
    pub fn read_dispatches(&self, run_id: &str) -> Result<Vec<BenchDispatchRecord>, StoreError> {
        read_jsonl(&self.dispatches_path(run_id))
    }

    /// Reads durable prompt snapshots for one run.
    ///
    /// # Errors
    ///
    /// As [`Self::read_runs`].
    pub fn read_prompts(&self, run_id: &str) -> Result<Vec<BenchPromptRecord>, StoreError> {
        read_jsonl(&self.prompts_path(run_id))
    }
}

fn run_date_dir(run_id: &str) -> String {
    let Some(stamp) = run_id
        .strip_prefix("bench-")
        .and_then(|rest| rest.get(..14))
    else {
        return "unknown-date".into();
    };
    if stamp.len() == 14 && stamp.chars().all(|c| c.is_ascii_digit()) {
        format!("{}-{}-{}", &stamp[0..4], &stamp[4..6], &stamp[6..8])
    } else {
        "unknown-date".into()
    }
}

fn ensure_dir(dir: &Path) -> Result<(), StoreError> {
    std::fs::create_dir_all(dir).map_err(|e| StoreError::Io {
        path: dir.to_path_buf(),
        source: e,
    })
}

fn sanitize_path_component(raw: &str) -> String {
    raw.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
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

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), StoreError> {
    let body = serde_json::to_string_pretty(value)?;
    std::fs::write(path, body).map_err(|e| StoreError::Io {
        path: path.to_path_buf(),
        source: e,
    })
}

fn read_json_file<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>, StoreError> {
    if !path.exists() {
        return Ok(None);
    }
    let body = std::fs::read_to_string(path).map_err(|e| StoreError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    Ok(serde_json::from_str::<T>(&body).ok())
}

fn discover_run_summaries(bench_dir: &Path) -> Result<Vec<BenchRun>, StoreError> {
    if !bench_dir.exists() {
        return Ok(Vec::new());
    }
    let mut runs = Vec::new();
    let date_dirs = std::fs::read_dir(bench_dir).map_err(|e| StoreError::Io {
        path: bench_dir.to_path_buf(),
        source: e,
    })?;
    for date_entry in date_dirs.flatten() {
        let Ok(file_type) = date_entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let date_path = date_entry.path();
        let run_dirs = std::fs::read_dir(&date_path).map_err(|e| StoreError::Io {
            path: date_path.clone(),
            source: e,
        })?;
        for run_entry in run_dirs.flatten() {
            let Ok(file_type) = run_entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            if let Some(run) = read_json_file(&run_entry.path().join("run.json"))? {
                runs.push(run);
            }
        }
    }
    Ok(runs)
}

fn dedupe_runs_last_wins(runs: Vec<BenchRun>) -> Vec<BenchRun> {
    let mut deduped = Vec::with_capacity(runs.len());
    let mut positions = HashMap::new();
    for run in runs {
        if let Some(index) = positions.get(&run.run_id).copied() {
            deduped[index] = run;
        } else {
            positions.insert(run.run_id.clone(), deduped.len());
            deduped.push(run);
        }
    }
    deduped
}

/// Repairs labels written by older `just` recipes that passed shell quotes as
/// literal argument data. Persisted JSON stays readable without mutating
/// append-only history; new recipes write the normalized value directly.
fn normalize_legacy_variant(run: &mut BenchRun) {
    let Some(variant) = run.variant.as_deref() else {
        return;
    };
    if variant.len() >= 2 && variant.starts_with('"') && variant.ends_with('"') {
        run.variant = Some(variant[1..variant.len() - 1].into());
    }
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
    use crate::bench::prompts::{PromptInputFile, PromptManifest, PromptRoleManifest};
    use crate::execution::{PromptContextMetrics, PromptRecord};

    fn sample_result(run_id: &str, case_id: &str) -> BenchResult {
        BenchResult {
            case_id: case_id.into(),
            run_id: run_id.into(),
            tier: BenchTier::T1,
            status: BenchStatus::Pass,
            score: 1.0,
            process_score: None,
            process_annotations: Vec::new(),
            resource_guidance: None,
            latency_ms: 1234,
            model_latency_ms: Some(1000),
            tool_latency_ms: Some(200),
            total_latency_ms: Some(1200),
            cost_cents: Some(3),
            cost_micros: Some(25_000),
            iterations: 1,
            assistant_turns: Some(1),
            tool_calls: Some(0),
            prompt_tokens: Some(20),
            completion_tokens: Some(10),
            total_tokens: Some(30),
            cache_read_tokens: Some(4),
            cache_write_tokens: Some(2),
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
            tiers: vec![BenchTier::T1],
            variant: Some("baseline".into()),
            model_selector: Some("fast".into()),
            model_key: Some("fast".into()),
            worker_model: Some("mock/test".into()),
            grader_model: Some("mock/grader".into()),
            prompt_variant: Some("composable-v1".into()),
            prompt_manifest: Some(PromptManifest {
                prompt_set: "composable-v1".into(),
                composition: Some(PromptInputFile {
                    path: "composition.toml".into(),
                    sha256: "definition".into(),
                }),
                roles: vec![PromptRoleManifest {
                    role: "decompose".into(),
                    assembly: "composed".into(),
                    assembled_sha256: "assembled".into(),
                    fragments: vec![PromptInputFile {
                        path: "base/worker.md".into(),
                        sha256: "fragment".into(),
                    }],
                }],
            }),
            suite_manifest: None,
            cases_root: Some("bench/cases".into()),
            bench_config_path: None,
            orboros_commit: None,
            bench_commit: None,
            orboros_dirty: None,
            bench_dirty: None,
            total: 3,
            passed: 2,
            failed: 1,
            errored: 0,
            skipped: 0,
            config_hash: "feedface".into(),
            total_cost_cents: Some(9),
            total_cost_micros: Some(90_000),
            prompt_tokens: Some(60),
            completion_tokens: Some(30),
            total_tokens: Some(90),
            cache_read_tokens: Some(12),
            cache_write_tokens: Some(6),
            assistant_turns: Some(3),
            tool_calls: Some(2),
        }
    }

    const DATED_RUN_ID: &str = "bench-20260721200204-16b98c28";

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
        let r = sample_result(DATED_RUN_ID, "case-a");
        store.append_result(&r).unwrap();
        assert!(store
            .results_path(DATED_RUN_ID)
            .ends_with("2026-07-21/bench-20260721200204-16b98c28/results.jsonl"));
        let read = store.read_results(DATED_RUN_ID).unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0], r);
    }

    #[test]
    fn append_run_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = BenchStore::new(dir.path().join("bench"));
        let r = sample_run(DATED_RUN_ID);
        store.append_run(&r).unwrap();
        assert!(store.run_summary_path(DATED_RUN_ID).exists());
        let read = store.read_runs().unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0], r);
    }

    #[test]
    fn read_runs_normalizes_legacy_quoted_variant() {
        let dir = tempfile::tempdir().unwrap();
        let store = BenchStore::new(dir.path().join("bench"));
        let mut run = sample_run(DATED_RUN_ID);
        run.variant = Some("\"reliability-check\"".into());
        store.append_run(&run).unwrap();

        let read = store.read_runs().unwrap();
        assert_eq!(read[0].variant.as_deref(), Some("reliability-check"));
    }

    #[test]
    fn read_runs_uses_last_summary_for_duplicate_run_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = BenchStore::new(dir.path().join("bench"));
        let first = sample_run(DATED_RUN_ID);
        let mut updated = first.clone();
        updated.tier = None;
        updated.tiers = vec![BenchTier::T1, BenchTier::T2];
        updated.total = 14;
        updated.passed = 10;
        updated.failed = 1;
        updated.errored = 3;

        store.append_run(&first).unwrap();
        store.append_run(&updated).unwrap();

        let read = store.read_runs().unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0], updated);
    }

    #[test]
    fn read_runs_discovers_dated_run_summary_without_index() {
        let dir = tempfile::tempdir().unwrap();
        let store = BenchStore::new(dir.path().join("bench"));
        let r = sample_run(DATED_RUN_ID);
        std::fs::create_dir_all(store.run_dir(DATED_RUN_ID)).unwrap();
        write_json(&store.run_summary_path(DATED_RUN_ID), &r).unwrap();
        let read = store.read_runs().unwrap();
        assert_eq!(read, vec![r]);
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

    #[test]
    fn prompt_snapshots_round_trip_with_provider_input_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let store = BenchStore::new(dir.path().join("bench"));
        let prompt = PromptRecord {
            orb_id: "orb-1".into(),
            parent_id: None,
            dispatch_kind: "worker.execute".into(),
            dispatched_at: Utc::now(),
            system_prompt: "system".into(),
            user_prompt: "user".into(),
            system_prompt_hash: "system-hash".into(),
            user_prompt_hash: "user-hash".into(),
            input_tokens: Some(123),
            prompt_context: PromptContextMetrics {
                final_user_prompt_chars: 4,
                ..PromptContextMetrics::default()
            },
        };

        store
            .append_prompts(DATED_RUN_ID, "t2.001", std::slice::from_ref(&prompt))
            .unwrap();

        let retained = store.read_prompts(DATED_RUN_ID).unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].case_id, "t2.001");
        assert_eq!(retained[0].prompt.input_tokens, Some(123));
        assert_eq!(retained[0].prompt.user_prompt, prompt.user_prompt);
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

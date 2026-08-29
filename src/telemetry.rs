//! User-local, reconstructable project execution telemetry.
//!
//! The append-only ledgers are the durable audit trail. `summary.json` is a
//! replaceable projection for fast operator reads and can always be rebuilt.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::execution::ExecutionRecord;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptEvent {
    pub event: String,
    pub attempt_id: String,
    pub at: DateTime<Utc>,
    pub orb_id: String,
    pub dispatch_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_micros: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<u32>,
    #[serde(default)]
    pub retries: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunEvent {
    pub event: String,
    pub run_id: String,
    pub at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_secs: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregate: Option<RunAggregate>,
}

/// Durable end-of-run aggregate. Costs remain microdollars here; presentation
/// converts them only at the operator boundary.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RunAggregate {
    pub completed_dispatches: u64,
    pub failed_dispatches: u64,
    pub retries: u64,
    pub total_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_micros: u64,
    pub assistant_turns: u64,
    pub tool_calls: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct TelemetrySummary {
    pub updated_at: Option<DateTime<Utc>>,
    pub completed_dispatches: u64,
    pub failed_dispatches: u64,
    pub cancelled_dispatches: u64,
    pub retries: u64,
    pub total_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_micros: u64,
    pub assistant_turns: u64,
    pub tool_calls: u64,
    pub active_attempts: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct TelemetryStore {
    root: PathBuf,
}

impl TelemetryStore {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn attempts_path(&self) -> PathBuf {
        self.root.join("attempts.jsonl")
    }
    fn runs_path(&self) -> PathBuf {
        self.root.join("runs.jsonl")
    }
    fn summary_path(&self) -> PathBuf {
        self.root.join("summary.json")
    }

    fn with_lock<T>(&self, operation: impl FnOnce() -> std::io::Result<T>) -> std::io::Result<T> {
        std::fs::create_dir_all(&self.root)?;
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(self.root.join(".lock"))?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let result = operation();
        let _ = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
        result
    }

    fn append<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        serde_json::to_writer(&mut file, value).map_err(std::io::Error::other)?;
        file.write_all(b"\n")?;
        file.sync_data()
    }

    pub fn record_attempt_started(
        &self,
        orb_id: &str,
        dispatch_kind: &str,
    ) -> std::io::Result<String> {
        let attempt_id = uuid::Uuid::new_v4().to_string();
        let event = AttemptEvent {
            event: "started".into(),
            attempt_id: attempt_id.clone(),
            at: Utc::now(),
            orb_id: orb_id.into(),
            dispatch_kind: dispatch_kind.into(),
            status: None,
            total_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            cost_micros: None,
            assistant_turns: None,
            tool_calls: None,
            retries: 0,
        };
        self.with_lock(|| {
            Self::append(&self.attempts_path(), &event)?;
            self.write_summary_locked()
        })?;
        Ok(attempt_id)
    }

    pub fn record_attempt_finished(
        &self,
        attempt_id: &str,
        record: &ExecutionRecord,
    ) -> std::io::Result<()> {
        let event = AttemptEvent {
            event: "finished".into(),
            attempt_id: attempt_id.into(),
            at: record.completed_at,
            orb_id: record.orb_id.clone(),
            dispatch_kind: record.dispatch_kind.clone(),
            status: Some(record.status.clone()),
            total_tokens: record.total_tokens,
            cache_read_tokens: record.cache_read_tokens,
            cache_write_tokens: record.cache_write_tokens,
            cost_micros: record.cost_micros,
            assistant_turns: record.assistant_turns,
            tool_calls: record.tool_calls,
            retries: record.retries,
        };
        self.with_lock(|| {
            Self::append(&self.attempts_path(), &event)?;
            self.write_summary_locked()
        })
    }

    pub fn record_run_started(&self, run_id: &str) -> std::io::Result<()> {
        self.with_lock(|| {
            Self::append(
                &self.runs_path(),
                &RunEvent {
                    event: "started".into(),
                    run_id: run_id.into(),
                    at: Utc::now(),
                    elapsed_secs: None,
                    aggregate: None,
                },
            )
        })
    }

    pub fn record_run_finished(
        &self,
        run_id: &str,
        started: DateTime<Utc>,
        aggregate: RunAggregate,
    ) -> std::io::Result<()> {
        self.with_lock(|| {
            Self::append(
                &self.runs_path(),
                &RunEvent {
                    event: "finished".into(),
                    run_id: run_id.into(),
                    at: Utc::now(),
                    elapsed_secs: Some((Utc::now() - started).num_seconds()),
                    aggregate: Some(aggregate),
                },
            )
        })
    }

    pub fn rebuild_from_execution_records(
        &self,
        records: &[ExecutionRecord],
    ) -> std::io::Result<TelemetrySummary> {
        self.with_lock(|| {
            let replacement = self.root.join("attempts.rebuild.jsonl");
            let mut file = File::create(&replacement)?;
            for record in records {
                let event = AttemptEvent {
                    event: "finished".into(),
                    attempt_id: format!(
                        "rebuild:{}:{}",
                        record.orb_id,
                        record.completed_at.timestamp_micros()
                    ),
                    at: record.completed_at,
                    orb_id: record.orb_id.clone(),
                    dispatch_kind: record.dispatch_kind.clone(),
                    status: Some(record.status.clone()),
                    total_tokens: record.total_tokens,
                    cache_read_tokens: record.cache_read_tokens,
                    cache_write_tokens: record.cache_write_tokens,
                    cost_micros: record.cost_micros,
                    assistant_turns: record.assistant_turns,
                    tool_calls: record.tool_calls,
                    retries: record.retries,
                };
                serde_json::to_writer(&mut file, &event).map_err(std::io::Error::other)?;
                file.write_all(b"\n")?;
            }
            file.sync_all()?;
            std::fs::rename(replacement, self.attempts_path())?;
            self.write_summary_locked()
        })?;
        self.read_summary()
    }

    pub fn read_summary(&self) -> std::io::Result<TelemetrySummary> {
        match std::fs::read(self.summary_path()) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(TelemetrySummary::default())
            }
            Err(error) => Err(error),
        }
    }

    fn write_summary_locked(&self) -> std::io::Result<()> {
        let summary = summarize(&read_jsonl::<AttemptEvent>(&self.attempts_path()));
        let temporary = self.root.join("summary.json.tmp");
        let mut file = File::create(&temporary)?;
        serde_json::to_writer_pretty(&mut file, &summary).map_err(std::io::Error::other)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(temporary, self.summary_path())?;
        Ok(())
    }
}

fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &Path) -> Vec<T> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str(&line).ok())
        .collect()
}

fn summarize(events: &[AttemptEvent]) -> TelemetrySummary {
    let mut summary = TelemetrySummary::default();
    let mut active = BTreeSet::new();
    for event in events {
        summary.updated_at = Some(
            summary
                .updated_at
                .map_or(event.at, |current| current.max(event.at)),
        );
        if event.event == "started" {
            active.insert(event.attempt_id.clone());
            continue;
        }
        if event.event != "finished" {
            continue;
        }
        active.remove(&event.attempt_id);
        summary.total_tokens = summary
            .total_tokens
            .saturating_add(event.total_tokens.unwrap_or(0));
        summary.cache_read_tokens = summary
            .cache_read_tokens
            .saturating_add(event.cache_read_tokens.unwrap_or(0));
        summary.cache_write_tokens = summary
            .cache_write_tokens
            .saturating_add(event.cache_write_tokens.unwrap_or(0));
        summary.cost_micros = summary
            .cost_micros
            .saturating_add(event.cost_micros.unwrap_or(0));
        summary.assistant_turns = summary
            .assistant_turns
            .saturating_add(u64::from(event.assistant_turns.unwrap_or(0)));
        summary.tool_calls = summary
            .tool_calls
            .saturating_add(u64::from(event.tool_calls.unwrap_or(0)));
        summary.retries = summary.retries.saturating_add(u64::from(event.retries));
        match event.status.as_deref() {
            Some("done") => summary.completed_dispatches += 1,
            Some("cancelled" | "aborted") => summary.cancelled_dispatches += 1,
            _ => summary.failed_dispatches += 1,
        }
    }
    summary.active_attempts = active.into_iter().collect();
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn rebuild_creates_queryable_summary() {
        let dir = tempfile::tempdir().unwrap();
        let store = TelemetryStore::new(dir.path().join("telemetry"));
        let records = vec![ExecutionRecord {
            orb_id: "orb-1".into(),
            parent_id: None,
            dispatch_kind: "worker.execute".into(),
            tool_policy: None,
            tool_policy_source: None,
            allowed_tools: None,
            status: "done".into(),
            dispatched_at: Utc::now(),
            completed_at: Utc::now(),
            worker_model: None,
            model_latency_ms: None,
            tool_latency_ms: None,
            total_latency_ms: None,
            assistant_turns: Some(2),
            tool_calls: Some(5),
            prompt_tokens: None,
            completion_tokens: None,
            total_tokens: Some(12),
            cost_micros: Some(42),
            cache_read_tokens: Some(3),
            cache_write_tokens: None,
            retries: 1,
            prompt_context: None,
            decomposition_repair: None,
            phase_retry: None,
            terminal_retry: None,
            partial_artifact_recovery: None,
            refinement_round: None,
            refinement_attempt: None,
            attempts: Vec::new(),
        }];
        let summary = store.rebuild_from_execution_records(&records).unwrap();
        assert_eq!(summary.completed_dispatches, 1);
        assert_eq!(summary.total_tokens, 12);
        assert_eq!(summary.cost_micros, 42);
        assert_eq!(summary.assistant_turns, 2);
        assert_eq!(summary.tool_calls, 5);
    }
}

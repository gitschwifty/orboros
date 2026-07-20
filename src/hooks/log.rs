//! Append-only log of hook invocations.
//!
//! Lives at `<state_dir>/hooks.log.jsonl`. One JSON object per line —
//! a serialized `HookInvocation` plus the firing event name and
//! aborted/soft-failed status. Distinct from the orb audit log
//! (`audit_store::AuditStore`) because hook invocations carry
//! richer per-invocation data and may not be associated with an
//! orb at all (e.g. `on-queue-tick`).

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::hooks::runner::{FireOutcome, HookInvocation};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookLogEntry {
    /// The `HookInvocation` captured at fire time.
    pub invocation: HookInvocation,
    /// `"ok"`, `"soft_fail"`, or `"aborted"` — the aggregate outcome
    /// the caller saw. Recorded per-entry for easy filtering.
    pub outcome_label: String,
}

#[derive(Debug, Clone)]
pub struct HookLog {
    path: PathBuf,
}

impl HookLog {
    #[must_use]
    pub fn new(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join("hooks.log.jsonl"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one entry per invocation in the slice. The `outcome`
    /// is recorded on every entry — the caller produces one outcome
    /// per `fire()` call, not per invocation, so the same label
    /// applies to all invocations from that call.
    ///
    /// # Errors
    ///
    /// Returns an IO error if the file cannot be opened or written.
    pub fn append_batch(
        &self,
        invocations: &[HookInvocation],
        outcome: &FireOutcome,
    ) -> std::io::Result<()> {
        if invocations.is_empty() {
            return Ok(());
        }
        let label = outcome_label(outcome);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        for inv in invocations {
            let entry = HookLogEntry {
                invocation: inv.clone(),
                outcome_label: label.to_string(),
            };
            let mut line = serde_json::to_string(&entry)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            line.push('\n');
            file.write_all(line.as_bytes())?;
        }
        Ok(())
    }

    /// Reads all entries; skips malformed lines rather than failing.
    ///
    /// # Errors
    ///
    /// Returns an IO error if the file cannot be opened. Returns
    /// `Ok(vec![])` if the file does not exist yet.
    pub fn read_all(&self) -> std::io::Result<Vec<HookLogEntry>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let file = std::fs::File::open(&self.path)?;
        let reader = BufReader::new(file);
        let mut out = Vec::new();
        for line in reader.lines() {
            let Ok(line) = line else { continue };
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(entry) = serde_json::from_str::<HookLogEntry>(&line) {
                out.push(entry);
            }
        }
        Ok(out)
    }

    /// Reads entries filtered by orb id. Returns entries in source
    /// order (oldest first).
    ///
    /// # Errors
    ///
    /// As `read_all`.
    pub fn read_for_orb(&self, orb_id: &str) -> std::io::Result<Vec<HookLogEntry>> {
        Ok(self
            .read_all()?
            .into_iter()
            .filter(|e| e.invocation.orb_id.as_deref() == Some(orb_id))
            .collect())
    }
}

fn outcome_label(outcome: &FireOutcome) -> &'static str {
    match outcome {
        FireOutcome::Ok => "ok",
        FireOutcome::SoftFail { .. } => "soft_fail",
        FireOutcome::Aborted { .. } => "aborted",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn make_inv(name: &str, orb_id: Option<&str>, exit: Option<i32>) -> HookInvocation {
        HookInvocation {
            hook_name: name.into(),
            event: "post-worker-complete".into(),
            orb_id: orb_id.map(String::from),
            sync: true,
            started_at: Utc::now(),
            duration_ms: 0,
            exit_code: exit,
            timed_out: false,
            aborted: false,
            stdout_truncated: String::new(),
            stderr_truncated: String::new(),
            error: None,
        }
    }

    #[test]
    fn read_all_missing_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let log = HookLog::new(dir.path());
        assert!(log.read_all().unwrap().is_empty());
    }

    #[test]
    fn append_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let log = HookLog::new(dir.path());
        let inv = make_inv("h1", Some("orb-abc"), Some(0));
        log.append_batch(std::slice::from_ref(&inv), &FireOutcome::Ok)
            .unwrap();
        let entries = log.read_all().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].invocation.hook_name, "h1");
        assert_eq!(entries[0].outcome_label, "ok");
    }

    #[test]
    fn append_batch_writes_one_line_per_invocation() {
        let dir = tempfile::tempdir().unwrap();
        let log = HookLog::new(dir.path());
        let invs = vec![
            make_inv("a", Some("orb-1"), Some(0)),
            make_inv("b", Some("orb-1"), Some(1)),
        ];
        log.append_batch(
            &invs,
            &FireOutcome::SoftFail {
                hook_name: "b".into(),
                exit_code: 1,
            },
        )
        .unwrap();
        let entries = log.read_all().unwrap();
        assert_eq!(entries.len(), 2);
        // Both entries get the soft_fail label since it's per-fire-call.
        assert!(entries.iter().all(|e| e.outcome_label == "soft_fail"));
    }

    #[test]
    fn aborted_outcome_label_propagates() {
        let dir = tempfile::tempdir().unwrap();
        let log = HookLog::new(dir.path());
        let inv = make_inv("blocker", Some("orb-x"), Some(2));
        log.append_batch(
            &[inv],
            &FireOutcome::Aborted {
                hook_name: "blocker".into(),
                exit_code: 2,
            },
        )
        .unwrap();
        let entries = log.read_all().unwrap();
        assert_eq!(entries[0].outcome_label, "aborted");
    }

    #[test]
    fn read_for_orb_filters_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let log = HookLog::new(dir.path());
        log.append_batch(
            &[
                make_inv("a", Some("orb-1"), Some(0)),
                make_inv("b", Some("orb-2"), Some(0)),
                make_inv("c", Some("orb-1"), Some(0)),
                make_inv("d", None, Some(0)),
            ],
            &FireOutcome::Ok,
        )
        .unwrap();
        let entries = log.read_for_orb("orb-1").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].invocation.hook_name, "a");
        assert_eq!(entries[1].invocation.hook_name, "c");
    }

    #[test]
    fn append_empty_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let log = HookLog::new(dir.path());
        log.append_batch(&[], &FireOutcome::Ok).unwrap();
        assert!(!log.path.exists());
    }

    #[test]
    fn malformed_line_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let log = HookLog::new(dir.path());
        std::fs::write(&log.path, "{not-json}\n").unwrap();
        let inv = make_inv("ok", Some("orb-1"), Some(0));
        log.append_batch(&[inv], &FireOutcome::Ok).unwrap();
        let entries = log.read_all().unwrap();
        // Malformed line skipped; valid one preserved.
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].invocation.hook_name, "ok");
    }
}

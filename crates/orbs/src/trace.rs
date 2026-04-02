use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::task::TaskStatus;

/// Why an orchestration run ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminationReason {
    /// All subtasks completed successfully.
    Completed,
    /// Some subtasks failed but the run wasn't cancelled.
    PartialFailure,
    /// Task-level timeout fired.
    Timeout,
    /// Budget limit was exceeded.
    BudgetExceeded,
    /// Explicitly cancelled (token fired for other reasons).
    Cancelled,
}

/// A trace span representing the execution of a single subtask.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceSpan {
    /// Task ID in the store.
    pub task_id: Uuid,
    /// Title of the subtask.
    pub title: String,
    /// Execution order group.
    pub order: u32,
    /// Final status.
    pub status: TaskStatus,
    /// When the subtask was dispatched.
    pub dispatched_at: Option<DateTime<Utc>>,
    /// When the subtask completed.
    pub completed_at: Option<DateTime<Utc>>,
    /// Wall-clock duration (`completed_at` - `dispatched_at`) in ms.
    pub wall_clock_ms: Option<u64>,
    /// Model latency reported by the harness (ms).
    pub model_latency_ms: Option<u64>,
    /// Tool latency reported by the harness (ms).
    pub tool_latency_ms: Option<u64>,
    /// Total latency reported by the harness (ms).
    pub total_latency_ms: Option<u64>,
    /// Overhead: `wall_clock_ms` - `total_latency_ms` (IPC, scheduling, etc).
    pub overhead_ms: Option<i64>,
    /// Number of retries before this result.
    pub retries: u32,
    /// Gaps/anomalies detected for this span.
    pub gaps: Vec<TraceGap>,
}

/// A gap or anomaly detected in the trace data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceGap {
    /// Harness did not report latency fields.
    MissingHarnessLatency,
    /// Orchestrator timestamps (`dispatched_at` / `completed_at`) are missing.
    MissingTimestamps,
    /// Overhead is negative (harness reported more latency than wall clock).
    NegativeOverhead { overhead_ms: i64 },
    /// Gap between the end of one order group and the start of the next.
    InterGroupGap {
        from_order: u32,
        to_order: u32,
        gap_ms: u64,
    },
    /// `model_latency` + `tool_latency` != `total_latency` (> 10% delta).
    LatencyMismatch {
        model_ms: u64,
        tool_ms: u64,
        total_ms: u64,
    },
}

/// Timeline for an entire orchestration run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskTimeline {
    /// Parent task ID.
    pub parent_task_id: Uuid,
    /// Per-subtask spans, in the order they appear in the outcome.
    pub spans: Vec<TraceSpan>,
    /// Total wall-clock duration of the orchestration run (ms).
    pub total_wall_clock_ms: u64,
    /// Why the orchestration run ended.
    pub termination_reason: TerminationReason,
    /// Top-level gaps (e.g., inter-group gaps).
    pub gaps: Vec<TraceGap>,
}

/// Detects gaps between order groups (time between the latest completion in
/// one group and the earliest dispatch in the next).
pub fn detect_inter_group_gaps(spans: &[TraceSpan]) -> Vec<TraceGap> {
    use std::collections::BTreeMap;

    type GroupBounds = (Option<DateTime<Utc>>, Option<DateTime<Utc>>);
    let mut groups: BTreeMap<u32, GroupBounds> = BTreeMap::new();

    for span in spans {
        let entry = groups.entry(span.order).or_insert((None, None));
        // Track earliest dispatched_at and latest completed_at per group
        if let Some(d) = span.dispatched_at {
            entry.0 = Some(match entry.0 {
                Some(existing) if existing < d => existing,
                _ => d,
            });
        }
        if let Some(c) = span.completed_at {
            entry.1 = Some(match entry.1 {
                Some(existing) if existing > c => existing,
                _ => c,
            });
        }
    }

    let orders: Vec<u32> = groups.keys().copied().collect();
    let mut gaps = Vec::new();

    for window in orders.windows(2) {
        let from_order = window[0];
        let to_order = window[1];

        if let (Some((_, Some(prev_end))), Some((Some(next_start), _))) =
            (groups.get(&from_order), groups.get(&to_order))
        {
            let delta = *next_start - *prev_end;
            let gap_ms = delta.num_milliseconds();
            if gap_ms > 0 {
                gaps.push(TraceGap::InterGroupGap {
                    from_order,
                    to_order,
                    gap_ms: gap_ms.cast_unsigned(),
                });
            }
        }
    }

    gaps
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- TraceSpan / TraceGap serde round-trip ----

    #[test]
    fn trace_gap_serde_round_trip() {
        let gaps = vec![
            TraceGap::MissingHarnessLatency,
            TraceGap::MissingTimestamps,
            TraceGap::NegativeOverhead { overhead_ms: -50 },
            TraceGap::InterGroupGap {
                from_order: 0,
                to_order: 1,
                gap_ms: 100,
            },
            TraceGap::LatencyMismatch {
                model_ms: 100,
                tool_ms: 200,
                total_ms: 250,
            },
        ];

        for gap in &gaps {
            let json = serde_json::to_string(gap).unwrap();
            let parsed: TraceGap = serde_json::from_str(&json).unwrap();
            assert_eq!(&parsed, gap);
        }
    }

    #[test]
    fn trace_span_serde_round_trip() {
        let span = TraceSpan {
            task_id: Uuid::new_v4(),
            title: "Test span".into(),
            order: 0,
            status: TaskStatus::Done,
            dispatched_at: Some(Utc::now()),
            completed_at: Some(Utc::now()),
            wall_clock_ms: Some(500),
            model_latency_ms: Some(400),
            tool_latency_ms: Some(50),
            total_latency_ms: Some(450),
            overhead_ms: Some(50),
            retries: 0,
            gaps: vec![],
        };
        let json = serde_json::to_string(&span).unwrap();
        let parsed: TraceSpan = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.title, "Test span");
        assert_eq!(parsed.wall_clock_ms, Some(500));
    }

    #[test]
    fn task_timeline_serde_round_trip() {
        let timeline = TaskTimeline {
            parent_task_id: Uuid::new_v4(),
            spans: vec![],
            total_wall_clock_ms: 1000,
            termination_reason: TerminationReason::Completed,
            gaps: vec![],
        };
        let json = serde_json::to_string(&timeline).unwrap();
        let parsed: TaskTimeline = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.total_wall_clock_ms, 1000);
        assert_eq!(parsed.termination_reason, TerminationReason::Completed);
    }
}

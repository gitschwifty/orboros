use uuid::Uuid;

pub use orbs::trace::{
    detect_inter_group_gaps, TaskTimeline, TerminationReason, TraceGap, TraceSpan,
};

use crate::orchestrator::{OrchestrateOutcome, SubtaskResult};

/// Builds a `TaskTimeline` from a completed orchestration run.
pub fn build_timeline(parent_id: Uuid, outcome: &OrchestrateOutcome) -> TaskTimeline {
    let spans: Vec<TraceSpan> = outcome.subtask_results.iter().map(build_span).collect();

    let inter_group_gaps = detect_inter_group_gaps(&spans);

    TaskTimeline {
        parent_task_id: parent_id,
        spans,
        #[allow(clippy::cast_possible_truncation)]
        total_wall_clock_ms: outcome.elapsed.as_millis() as u64,
        termination_reason: outcome.termination_reason.clone(),
        gaps: inter_group_gaps,
    }
}

/// Converts a `SubtaskResult` into a `TraceSpan`, computing derived fields
/// and running per-span gap detection.
fn build_span(result: &SubtaskResult) -> TraceSpan {
    let wall_clock_ms = match (result.dispatched_at, result.completed_at) {
        (Some(start), Some(end)) => {
            let delta = end - start;
            Some(delta.num_milliseconds().max(0).cast_unsigned())
        }
        _ => None,
    };

    let overhead_ms = match (wall_clock_ms, result.total_latency_ms) {
        #[allow(clippy::cast_possible_wrap)]
        (Some(wall), Some(total)) => Some(wall as i64 - total as i64),
        _ => None,
    };

    let mut gaps = Vec::new();
    detect_span_gaps(result, overhead_ms, &mut gaps);

    TraceSpan {
        task_id: result.task_id,
        title: result.title.clone(),
        order: result.order,
        status: result.status,
        dispatched_at: result.dispatched_at,
        completed_at: result.completed_at,
        wall_clock_ms,
        model_latency_ms: result.model_latency_ms,
        tool_latency_ms: result.tool_latency_ms,
        total_latency_ms: result.total_latency_ms,
        overhead_ms,
        retries: result.retries,
        gaps,
    }
}

/// Detects per-span anomalies.
fn detect_span_gaps(result: &SubtaskResult, overhead_ms: Option<i64>, gaps: &mut Vec<TraceGap>) {
    // Missing harness latency
    if result.total_latency_ms.is_none()
        && result.model_latency_ms.is_none()
        && result.tool_latency_ms.is_none()
    {
        gaps.push(TraceGap::MissingHarnessLatency);
    }

    // Missing timestamps
    if result.dispatched_at.is_none() || result.completed_at.is_none() {
        gaps.push(TraceGap::MissingTimestamps);
    }

    // Negative overhead
    if let Some(oh) = overhead_ms {
        if oh < 0 {
            gaps.push(TraceGap::NegativeOverhead { overhead_ms: oh });
        }
    }

    // Latency mismatch: model + tool != total (> 10% delta)
    if let (Some(model), Some(tool), Some(total)) = (
        result.model_latency_ms,
        result.tool_latency_ms,
        result.total_latency_ms,
    ) {
        if total > 0 {
            let sum = model + tool;
            #[allow(clippy::cast_possible_wrap)]
            let diff = (sum as i64 - total as i64).unsigned_abs();
            // > 10% mismatch
            if diff * 10 > total {
                gaps.push(TraceGap::LatencyMismatch {
                    model_ms: model,
                    tool_ms: tool,
                    total_ms: total,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use chrono::{DateTime, Utc};
    use orbs::task::TaskStatus;
    use uuid::Uuid;

    use crate::ipc::types::Usage;

    fn make_subtask_result(title: &str, order: u32, status: TaskStatus) -> SubtaskResult {
        SubtaskResult {
            task_id: Uuid::new_v4(),
            title: title.into(),
            order,
            status,
            response: Some("test response".into()),
            usage: None,
            retries: 0,
            dispatched_at: None,
            completed_at: None,
            model_latency_ms: None,
            tool_latency_ms: None,
            total_latency_ms: None,
        }
    }

    fn make_timed_result(
        title: &str,
        order: u32,
        dispatched: DateTime<Utc>,
        completed: DateTime<Utc>,
        model_ms: Option<u64>,
        tool_ms: Option<u64>,
        total_ms: Option<u64>,
    ) -> SubtaskResult {
        SubtaskResult {
            task_id: Uuid::new_v4(),
            title: title.into(),
            order,
            status: TaskStatus::Done,
            response: Some("test response".into()),
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            }),
            retries: 0,
            dispatched_at: Some(dispatched),
            completed_at: Some(completed),
            model_latency_ms: model_ms,
            tool_latency_ms: tool_ms,
            total_latency_ms: total_ms,
        }
    }

    fn make_outcome(
        results: Vec<SubtaskResult>,
        reason: TerminationReason,
        elapsed: Duration,
    ) -> OrchestrateOutcome {
        OrchestrateOutcome {
            parent_status: TaskStatus::Done,
            subtask_results: results,
            aggregated_result: None,
            termination_reason: reason,
            total_usage: None,
            elapsed,
        }
    }

    // ---- build_timeline populates all fields ----

    #[test]
    fn build_timeline_populates_spans() {
        let now = Utc::now();
        let later = now + chrono::Duration::milliseconds(500);
        let results = vec![make_timed_result(
            "Step 1",
            0,
            now,
            later,
            Some(300),
            Some(100),
            Some(400),
        )];
        let outcome = make_outcome(
            results,
            TerminationReason::Completed,
            Duration::from_millis(500),
        );
        let parent_id = Uuid::new_v4();

        let timeline = build_timeline(parent_id, &outcome);

        assert_eq!(timeline.parent_task_id, parent_id);
        assert_eq!(timeline.spans.len(), 1);
        assert_eq!(timeline.total_wall_clock_ms, 500);
        assert_eq!(timeline.termination_reason, TerminationReason::Completed);

        let span = &timeline.spans[0];
        assert_eq!(span.title, "Step 1");
        assert_eq!(span.wall_clock_ms, Some(500));
        assert_eq!(span.model_latency_ms, Some(300));
        assert_eq!(span.tool_latency_ms, Some(100));
        assert_eq!(span.total_latency_ms, Some(400));
        assert_eq!(span.overhead_ms, Some(100)); // 500 - 400
        assert!(span.gaps.is_empty());
    }

    // ---- Gap detection: MissingHarnessLatency ----

    #[test]
    fn detects_missing_harness_latency() {
        let result = make_subtask_result("No latency", 0, TaskStatus::Done);
        let span = build_span(&result);
        assert!(span.gaps.contains(&TraceGap::MissingHarnessLatency));
    }

    #[test]
    fn no_missing_latency_when_total_present() {
        let now = Utc::now();
        let later = now + chrono::Duration::milliseconds(100);
        let mut result = make_subtask_result("Has latency", 0, TaskStatus::Done);
        result.dispatched_at = Some(now);
        result.completed_at = Some(later);
        result.total_latency_ms = Some(50);
        let span = build_span(&result);
        assert!(!span.gaps.contains(&TraceGap::MissingHarnessLatency));
    }

    // ---- Gap detection: MissingTimestamps ----

    #[test]
    fn detects_missing_timestamps() {
        let result = make_subtask_result("No timestamps", 0, TaskStatus::Done);
        let span = build_span(&result);
        assert!(span.gaps.contains(&TraceGap::MissingTimestamps));
    }

    #[test]
    fn no_missing_timestamps_when_both_present() {
        let now = Utc::now();
        let later = now + chrono::Duration::milliseconds(100);
        let mut result = make_subtask_result("Has timestamps", 0, TaskStatus::Done);
        result.dispatched_at = Some(now);
        result.completed_at = Some(later);
        let span = build_span(&result);
        assert!(!span.gaps.contains(&TraceGap::MissingTimestamps));
    }

    // ---- Gap detection: NegativeOverhead ----

    #[test]
    fn detects_negative_overhead() {
        let now = Utc::now();
        let later = now + chrono::Duration::milliseconds(100);
        // total_latency > wall_clock
        let result = make_timed_result("Negative OH", 0, now, later, None, None, Some(200));
        let span = build_span(&result);
        assert!(span
            .gaps
            .iter()
            .any(|g| matches!(g, TraceGap::NegativeOverhead { overhead_ms } if *overhead_ms < 0)));
    }

    #[test]
    fn no_negative_overhead_when_positive() {
        let now = Utc::now();
        let later = now + chrono::Duration::milliseconds(500);
        let result = make_timed_result("Positive OH", 0, now, later, None, None, Some(400));
        let span = build_span(&result);
        assert!(!span
            .gaps
            .iter()
            .any(|g| matches!(g, TraceGap::NegativeOverhead { .. })));
        assert_eq!(span.overhead_ms, Some(100));
    }

    // ---- Gap detection: LatencyMismatch ----

    #[test]
    fn detects_latency_mismatch() {
        let now = Utc::now();
        let later = now + chrono::Duration::milliseconds(500);
        // model(100) + tool(200) = 300, but total is 400 — delta 100/400 = 25% > 10%
        let result = make_timed_result("Mismatch", 0, now, later, Some(100), Some(200), Some(400));
        let span = build_span(&result);
        assert!(span
            .gaps
            .iter()
            .any(|g| matches!(g, TraceGap::LatencyMismatch { .. })));
    }

    #[test]
    fn no_latency_mismatch_when_close() {
        let now = Utc::now();
        let later = now + chrono::Duration::milliseconds(500);
        // model(200) + tool(100) = 300, total is 300 — exact match
        let result = make_timed_result("Match", 0, now, later, Some(200), Some(100), Some(300));
        let span = build_span(&result);
        assert!(!span
            .gaps
            .iter()
            .any(|g| matches!(g, TraceGap::LatencyMismatch { .. })));
    }

    // ---- Gap detection: InterGroupGap ----

    #[test]
    fn detects_inter_group_gap() {
        let t0 = Utc::now();
        let t1 = t0 + chrono::Duration::milliseconds(100);
        let t2 = t1 + chrono::Duration::milliseconds(50); // 50ms gap
        let t3 = t2 + chrono::Duration::milliseconds(100);

        let results = vec![
            make_timed_result("Group 0", 0, t0, t1, Some(80), Some(10), Some(90)),
            make_timed_result("Group 1", 1, t2, t3, Some(80), Some(10), Some(90)),
        ];
        let outcome = make_outcome(
            results,
            TerminationReason::Completed,
            Duration::from_millis(250),
        );
        let timeline = build_timeline(Uuid::new_v4(), &outcome);

        assert!(timeline.gaps.iter().any(|g| matches!(
            g,
            TraceGap::InterGroupGap { from_order: 0, to_order: 1, gap_ms } if *gap_ms == 50
        )));
    }

    #[test]
    fn no_inter_group_gap_same_order() {
        let t0 = Utc::now();
        let t1 = t0 + chrono::Duration::milliseconds(100);
        let t2 = t0 + chrono::Duration::milliseconds(10);
        let t3 = t0 + chrono::Duration::milliseconds(90);

        let results = vec![
            make_timed_result("A", 0, t0, t1, Some(80), Some(10), Some(90)),
            make_timed_result("B", 0, t2, t3, Some(70), Some(10), Some(80)),
        ];
        let outcome = make_outcome(
            results,
            TerminationReason::Completed,
            Duration::from_millis(100),
        );
        let timeline = build_timeline(Uuid::new_v4(), &outcome);

        assert!(timeline.gaps.is_empty());
    }

    // ---- Clean timeline: no false positives ----

    #[test]
    fn clean_timeline_has_no_gaps() {
        let now = Utc::now();
        let later = now + chrono::Duration::milliseconds(500);
        let results = vec![make_timed_result(
            "Clean",
            0,
            now,
            later,
            Some(200),
            Some(100),
            Some(300),
        )];
        let outcome = make_outcome(
            results,
            TerminationReason::Completed,
            Duration::from_millis(500),
        );
        let timeline = build_timeline(Uuid::new_v4(), &outcome);

        assert!(timeline.spans[0].gaps.is_empty(), "Expected no span gaps");
        assert!(timeline.gaps.is_empty(), "Expected no timeline gaps");
    }

    // ---- Edge cases ----

    #[test]
    fn handles_empty_subtask_list() {
        let outcome = make_outcome(
            vec![],
            TerminationReason::Completed,
            Duration::from_millis(10),
        );
        let timeline = build_timeline(Uuid::new_v4(), &outcome);

        assert!(timeline.spans.is_empty());
        assert!(timeline.gaps.is_empty());
        assert_eq!(timeline.total_wall_clock_ms, 10);
    }

    #[test]
    fn handles_single_subtask() {
        let now = Utc::now();
        let later = now + chrono::Duration::milliseconds(200);
        let results = vec![make_timed_result(
            "Solo",
            0,
            now,
            later,
            Some(100),
            Some(50),
            Some(150),
        )];
        let outcome = make_outcome(
            results,
            TerminationReason::Completed,
            Duration::from_millis(200),
        );
        let timeline = build_timeline(Uuid::new_v4(), &outcome);

        assert_eq!(timeline.spans.len(), 1);
        assert!(timeline.gaps.is_empty());
        assert_eq!(timeline.spans[0].overhead_ms, Some(50));
    }

    #[test]
    fn handles_cancelled_run() {
        let results = vec![make_subtask_result(
            "Cancelled step",
            0,
            TaskStatus::Cancelled,
        )];
        let outcome = make_outcome(
            results,
            TerminationReason::Cancelled,
            Duration::from_millis(100),
        );
        let timeline = build_timeline(Uuid::new_v4(), &outcome);

        assert_eq!(timeline.termination_reason, TerminationReason::Cancelled);
        assert_eq!(timeline.spans[0].status, TaskStatus::Cancelled);
    }

    #[test]
    fn handles_timed_out_run() {
        let now = Utc::now();
        let later = now + chrono::Duration::milliseconds(5000);
        let results = vec![make_timed_result(
            "Slow step",
            0,
            now,
            later,
            Some(4000),
            Some(500),
            Some(4500),
        )];
        let outcome = make_outcome(results, TerminationReason::Timeout, Duration::from_secs(5));
        let timeline = build_timeline(Uuid::new_v4(), &outcome);

        assert_eq!(timeline.termination_reason, TerminationReason::Timeout);
        assert_eq!(timeline.spans[0].wall_clock_ms, Some(5000));
    }

    // ---- Multi-group timeline ----

    #[test]
    fn multi_group_timeline() {
        let t0 = Utc::now();
        let t1 = t0 + chrono::Duration::milliseconds(100);
        let t2 = t1 + chrono::Duration::milliseconds(20);
        let t3 = t2 + chrono::Duration::milliseconds(150);

        let results = vec![
            make_timed_result("Phase 1a", 0, t0, t1, Some(80), Some(10), Some(90)),
            make_timed_result(
                "Phase 1b",
                0,
                t0 + chrono::Duration::milliseconds(5),
                t1 - chrono::Duration::milliseconds(5),
                Some(70),
                Some(10),
                Some(80),
            ),
            make_timed_result("Phase 2", 1, t2, t3, Some(120), Some(20), Some(140)),
        ];
        let outcome = make_outcome(
            results,
            TerminationReason::Completed,
            Duration::from_millis(270),
        );
        let timeline = build_timeline(Uuid::new_v4(), &outcome);

        assert_eq!(timeline.spans.len(), 3);
        assert!(timeline.gaps.iter().any(|g| matches!(
            g,
            TraceGap::InterGroupGap {
                from_order: 0,
                to_order: 1,
                ..
            }
        )));
    }

    #[test]
    fn wall_clock_computed_from_timestamps() {
        let now = Utc::now();
        let later = now + chrono::Duration::milliseconds(1234);
        let result = make_timed_result("Timed", 0, now, later, None, None, None);
        let span = build_span(&result);
        assert_eq!(span.wall_clock_ms, Some(1234));
    }
}

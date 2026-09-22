//! Bounded, content-free observations for one active worker send.

use std::time::Instant;

use crate::ipc::types::WorkerEvent;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_replaces_the_previous_snapshot_without_accumulating() {
        let mut live = LiveDispatch::new("worker", "session", "send");
        assert!(live.total_tokens.is_none());
        for tokens in [12, 20] {
            let event = serde_json::from_value(serde_json::json!({
                "event": "usage", "prompt_tokens": 10,
                "completion_tokens": tokens - 10, "total_tokens": tokens
            }))
            .unwrap();
            live.observe(&event);
        }
        assert_eq!(live.total_tokens, Some(20));
        assert_eq!(live.tool_calls, 0);
    }

    #[test]
    fn tool_end_counts_completion_without_retaining_output() {
        let mut live = LiveDispatch::new("worker", "session", "send");
        live.observe(&WorkerEvent::ToolEnd {
            name: "read_file".into(),
            result_preview: "private contents".into(),
        });
        assert_eq!(live.tool_calls, 1);
        assert_eq!(live.last_tool.as_deref(), Some("read_file"));
    }
}

pub(super) struct LiveDispatch {
    worker_id: String,
    session_id: String,
    send_id: String,
    started: Instant,
    last_activity: Instant,
    last_tool: Option<String>,
    tool_calls: u64,
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    total_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    cost_micros: Option<u64>,
    retryable_errors: u64,
}

impl LiveDispatch {
    pub(super) fn new(worker_id: &str, session_id: &str, send_id: &str) -> Self {
        let now = Instant::now();
        Self {
            worker_id: worker_id.into(),
            session_id: session_id.into(),
            send_id: send_id.into(),
            started: now,
            last_activity: now,
            last_tool: None,
            tool_calls: 0,
            prompt_tokens: None,
            completion_tokens: None,
            total_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            cost_micros: None,
            retryable_errors: 0,
        }
    }

    pub(super) fn observe(&mut self, event: &WorkerEvent) {
        self.last_activity = Instant::now();
        match event {
            WorkerEvent::ToolStart { name, .. } | WorkerEvent::ToolEnd { name, .. } => {
                self.last_tool = Some(name.chars().filter(|c| !c.is_control()).take(80).collect());
                if matches!(event, WorkerEvent::ToolEnd { .. }) {
                    self.tool_calls = self.tool_calls.saturating_add(1);
                }
            }
            WorkerEvent::Usage {
                prompt_tokens,
                completion_tokens,
                total_tokens,
                cached_tokens,
                cache_write_tokens,
                reasoning_tokens,
                cost_micros,
                ..
            } => {
                self.prompt_tokens = Some(*prompt_tokens);
                self.completion_tokens = Some(*completion_tokens);
                self.total_tokens = Some(*total_tokens);
                self.cache_read_tokens = *cached_tokens;
                self.cache_write_tokens = *cache_write_tokens;
                self.reasoning_tokens = *reasoning_tokens;
                self.cost_micros = *cost_micros;
            }
            WorkerEvent::Error {
                retryable: true, ..
            } => {
                self.retryable_errors = self.retryable_errors.saturating_add(1);
            }
            _ => {}
        }
    }

    pub(super) fn report(&self, state: &str) {
        tracing::info!(
            worker_id = %self.worker_id, session_id = %self.session_id, send_id = %self.send_id,
            live_state = state, elapsed_secs = self.started.elapsed().as_secs(),
            last_activity_age_secs = self.last_activity.elapsed().as_secs(),
            last_tool = self.last_tool.as_deref(), completed_tool_calls = self.tool_calls,
            provisional_prompt_tokens = self.prompt_tokens,
            provisional_completion_tokens = self.completion_tokens,
            provisional_total_tokens = self.total_tokens,
            provisional_cache_read_tokens = self.cache_read_tokens,
            provisional_cache_write_tokens = self.cache_write_tokens,
            provisional_reasoning_tokens = self.reasoning_tokens,
            provisional_cost_micros = self.cost_micros,
            retryable_errors_observed = self.retryable_errors,
            "worker live dispatch snapshot"
        );
    }
}

//! Long-lived conversational sessions on top of the existing worker IPC.
//!
//! `ConvoRuntime` owns one heddle subprocess per active session. Each user
//! turn drives `Worker::send_streaming`; the runtime maps incoming
//! `WorkerEvent`s to `SessionEvent`s, persists them to a `SessionStore`,
//! and forwards them to a per-turn `mpsc::Sender<SessionEvent>` so a UI
//! can render in real time.
//!
//! This module intentionally has no `tokio_util::sync::CancellationToken`
//! integration yet — Step 4 of the conversational interface adds Ctrl-C
//! semantics on top of the existing `Worker::send_cancellable` infra.

pub mod cli;
pub mod render;
pub mod sessions_cmd;

use std::collections::HashMap;

use chrono::Utc;
use orbs::session::{
    CloseReason, SessionEvent, SessionId, SessionInit, SessionUsage, ToolOutcome, TurnId,
};
use orbs::session_store::{SessionStore, SessionStoreError};
use tokio::sync::mpsc;

use crate::ipc::error::IpcError;
use crate::ipc::types::{ResultStatus, WorkerEvent};
use crate::worker::process::{SendOutcome, Worker, WorkerConfig};

/// Errors surfaced by the conversation runtime.
#[derive(Debug, thiserror::Error)]
pub enum ConvoError {
    #[error("session {0} is not active in this runtime")]
    SessionNotActive(SessionId),

    #[error("session {0} is already active in this runtime")]
    SessionAlreadyActive(SessionId),

    #[error("worker ipc error: {0}")]
    Ipc(#[from] IpcError),

    #[error("session store error: {0}")]
    Store(#[from] SessionStoreError),

    #[error("event drain task failed: {0}")]
    TaskJoin(String),
}

/// Outcome of a single user turn, returned alongside the streamed events.
#[derive(Debug, Clone)]
pub struct TurnSummary {
    pub turn_id: TurnId,
    pub response: Option<String>,
    pub usage: SessionUsage,
    pub tool_call_count: u32,
    pub status: TurnStatus,
}

/// Terminal state of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStatus {
    Ok,
    Cancelled,
    Error,
}

/// Conversation runtime — owns the live workers and the on-disk transcripts.
pub struct ConvoRuntime {
    store: SessionStore,
    workers: HashMap<SessionId, Worker>,
}

impl ConvoRuntime {
    pub fn new(store: SessionStore) -> Self {
        Self {
            store,
            workers: HashMap::new(),
        }
    }

    pub fn store(&self) -> &SessionStore {
        &self.store
    }

    /// Returns the ids of sessions currently backed by a live worker.
    pub fn active_session_ids(&self) -> impl Iterator<Item = &SessionId> {
        self.workers.keys()
    }

    /// Appends a session event outside the context of a worker turn.
    /// Used by slash commands (`/spawn`, `/await`) that record links and
    /// outcomes into the transcript without round-tripping through the
    /// LLM.
    ///
    /// # Errors
    ///
    /// `Store` if persistence fails (closed session, size cap, etc.).
    pub fn append_session_event(
        &self,
        session_id: &SessionId,
        event: &SessionEvent,
    ) -> Result<(), ConvoError> {
        self.store.append_event(session_id, event)?;
        Ok(())
    }

    /// Creates a new session: writes the init header to disk and spawns a
    /// worker against `worker_config`. The returned `SessionId` matches
    /// `init.id`.
    ///
    /// # Errors
    ///
    /// `Store` if persistence fails (e.g. duplicate id); `Ipc` if the
    /// worker spawn or init handshake fails. If spawn fails after the
    /// header has been written, the transcript is left in place with a
    /// `StatusChanged(Closed, WorkerCrash)` event appended so the partial
    /// session is observable.
    pub async fn start_session(
        &mut self,
        init: SessionInit,
        worker_config: WorkerConfig,
    ) -> Result<SessionId, ConvoError> {
        if self.workers.contains_key(&init.id) {
            return Err(ConvoError::SessionAlreadyActive(init.id));
        }

        self.store.create(&init)?;
        let id = init.id.clone();

        match Worker::spawn(&worker_config).await {
            Ok(worker) => {
                self.workers.insert(id.clone(), worker);
                Ok(id)
            }
            Err(ipc_err) => {
                let _ = self.store.close(
                    &id,
                    CloseReason::WorkerCrash {
                        detail: format!("spawn failed: {ipc_err}"),
                    },
                    Utc::now(),
                );
                Err(ConvoError::Ipc(ipc_err))
            }
        }
    }

    /// Drives one user turn. Persists the user message, then streams worker
    /// events through `event_tx` while also appending them to the
    /// transcript. Returns a `TurnSummary` when the worker emits its
    /// `Result`.
    ///
    /// # Errors
    ///
    /// `SessionNotActive` if no worker is attached; `Ipc` on worker IPC
    /// failure; `Store` on persistence failure (transcript size cap,
    /// closed session, etc.).
    #[tracing::instrument(
        name = "convo.send_turn",
        skip(self, message, event_tx),
        fields(session_id = %session_id, turn_id = tracing::field::Empty)
    )]
    pub async fn send_turn(
        &mut self,
        session_id: &SessionId,
        message: &str,
        event_tx: mpsc::Sender<SessionEvent>,
    ) -> Result<TurnSummary, ConvoError> {
        let worker = self
            .workers
            .get_mut(session_id)
            .ok_or_else(|| ConvoError::SessionNotActive(session_id.clone()))?;

        let turn_id = TurnId::new();
        let user_event = SessionEvent::UserMessage {
            turn_id: turn_id.clone(),
            content: message.to_string(),
            at: Utc::now(),
        };
        // Forward the user message to the subscriber too; the renderer may
        // want to echo it. Drop the channel silently if the receiver is
        // gone — persistence still happens.
        let _ = event_tx.send(user_event.clone()).await;
        self.store.append_event(session_id, &user_event)?;

        let (worker_tx, mut worker_rx) = mpsc::channel::<WorkerEvent>(64);
        let send_fut = worker.send_streaming(turn_id.as_str(), message, worker_tx);

        let store = self.store.clone();
        let session_id_owned = session_id.clone();
        let turn_id_owned = turn_id.clone();
        let event_tx_clone = event_tx.clone();
        let drain_task = tokio::spawn(async move {
            let mut response_buf = String::new();
            let mut tool_calls: u32 = 0;
            while let Some(worker_event) = worker_rx.recv().await {
                if let WorkerEvent::ContentDelta { text } = &worker_event {
                    response_buf.push_str(text);
                }
                if matches!(worker_event, WorkerEvent::ToolEnd { .. }) {
                    tool_calls = tool_calls.saturating_add(1);
                }
                let session_events = map_worker_event(&turn_id_owned, &worker_event);
                for ev in session_events {
                    if event_tx_clone.send(ev.clone()).await.is_err() {
                        // Receiver gone — stop forwarding but keep
                        // persisting so the transcript survives.
                    }
                    if let Err(e) = store.append_event(&session_id_owned, &ev) {
                        return Err(ConvoError::Store(e));
                    }
                }
            }
            Ok((response_buf, tool_calls))
        });

        let send_result = send_fut.await;
        let drain_result = drain_task
            .await
            .map_err(|join_err| ConvoError::TaskJoin(format!("drain task panicked: {join_err}")))?;

        let outcome = send_result?;
        let (streamed_response, streamed_tool_calls) = drain_result?;

        let status = turn_status_for(&outcome);
        let usage = outcome
            .usage
            .as_ref()
            .map(|u| SessionUsage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
                total_tokens: u.total_tokens,
                cost_micros: u.cost_micros,
                cost_currency: u.cost_currency.clone(),
                cached_tokens: u.cached_tokens,
                cache_write_tokens: u.cache_write_tokens,
                reasoning_tokens: u.reasoning_tokens,
            })
            .unwrap_or_default();

        // Final assistant message: prefer the worker's collated `response`
        // field over the stitched delta stream (deltas may have been pruned).
        let response = outcome
            .response
            .clone()
            .or((!streamed_response.is_empty()).then_some(streamed_response));

        self.finalize_turn(
            session_id,
            &turn_id,
            response.clone(),
            status,
            outcome.error.as_ref().map(|e| e.message.clone()),
            &event_tx,
        )
        .await?;

        let tool_call_count = u32::try_from(outcome.tool_calls_made.len())
            .unwrap_or(streamed_tool_calls)
            .max(streamed_tool_calls);

        Ok(TurnSummary {
            turn_id,
            response,
            usage,
            tool_call_count,
            status,
        })
    }

    async fn finalize_turn(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
        response: Option<String>,
        status: TurnStatus,
        error_message: Option<String>,
        event_tx: &mpsc::Sender<SessionEvent>,
    ) -> Result<(), ConvoError> {
        if let Some(text) = response {
            let final_event = SessionEvent::AssistantMessage {
                turn_id: turn_id.clone(),
                content: text,
                at: Utc::now(),
            };
            let _ = event_tx.send(final_event.clone()).await;
            self.store.append_event(session_id, &final_event)?;
        }

        if matches!(status, TurnStatus::Error) {
            if let Some(message) = error_message {
                let err_event = SessionEvent::Error {
                    turn_id: Some(turn_id.clone()),
                    message,
                    at: Utc::now(),
                };
                let _ = event_tx.send(err_event.clone()).await;
                self.store.append_event(session_id, &err_event)?;
            }
        }
        if matches!(status, TurnStatus::Cancelled) {
            let cancel_event = SessionEvent::Cancelled {
                turn_id: turn_id.clone(),
                at: Utc::now(),
            };
            let _ = event_tx.send(cancel_event.clone()).await;
            self.store.append_event(session_id, &cancel_event)?;
        }
        Ok(())
    }

    /// Closes a session: shuts down the worker (best-effort) and appends a
    /// `StatusChanged(Closed, reason)` event.
    ///
    /// # Errors
    ///
    /// `Store` if the close event cannot be appended. Worker shutdown
    /// failures are swallowed (we always reach `Closed` state on disk).
    pub async fn close_session(
        &mut self,
        session_id: &SessionId,
        reason: CloseReason,
    ) -> Result<(), ConvoError> {
        if let Some(worker) = self.workers.remove(session_id) {
            // Best-effort: don't fail the close on worker shutdown errors.
            let _ = worker.shutdown().await;
        }
        self.store.close(session_id, reason, Utc::now())?;
        Ok(())
    }

    /// Detaches the worker without changing transcript status — used by
    /// idle/handoff scenarios. The next `send_turn` will fail with
    /// `SessionNotActive`; the caller must call `start_session` again
    /// (re-hydrating from transcript) to resume.
    pub async fn detach_worker(&mut self, session_id: &SessionId) {
        if let Some(worker) = self.workers.remove(session_id) {
            let _ = worker.shutdown().await;
        }
    }

    /// Kills the current worker for `session_id` and spawns a fresh one
    /// against `worker_config`. The transcript stays active; a
    /// `SessionEvent::ContextReset` is appended with `reason`.
    ///
    /// Used by `/clear` (same `worker_config`, fresh heddle context) and
    /// `/model` (`worker_config.model` swapped, fresh context).
    ///
    /// # Errors
    ///
    /// `SessionNotActive` if no worker is attached to `session_id`;
    /// `Ipc` if the new worker fails to spawn; `Store` if appending the
    /// reset event fails. If the new worker fails to spawn, the old
    /// worker is already gone — caller should treat the session as
    /// detached and either retry or close it.
    #[tracing::instrument(
        name = "convo.restart_worker",
        skip(self, worker_config),
        fields(session_id = %session_id, reason = %reason)
    )]
    pub async fn restart_worker(
        &mut self,
        session_id: &SessionId,
        worker_config: WorkerConfig,
        reason: &str,
    ) -> Result<(), ConvoError> {
        if !self.workers.contains_key(session_id) {
            return Err(ConvoError::SessionNotActive(session_id.clone()));
        }

        // Persist the reset marker before killing the worker so the
        // transcript records the intent even if shutdown hangs.
        let event = SessionEvent::ContextReset {
            turn_id: TurnId::new(),
            reason: reason.to_string(),
            at: Utc::now(),
        };
        self.store.append_event(session_id, &event)?;

        if let Some(worker) = self.workers.remove(session_id) {
            let _ = worker.shutdown().await;
        }

        let new_worker = Worker::spawn(&worker_config).await?;
        self.workers.insert(session_id.clone(), new_worker);
        Ok(())
    }
}

fn turn_status_for(outcome: &SendOutcome) -> TurnStatus {
    match outcome.status {
        ResultStatus::Ok => TurnStatus::Ok,
        ResultStatus::Cancelled => TurnStatus::Cancelled,
        ResultStatus::Error => TurnStatus::Error,
    }
}

/// Translates a `WorkerEvent` from the IPC into the persistence-level
/// `SessionEvent`s that belong in a transcript. Operational events
/// (heartbeat, context handoff, permission flows) return an empty vec —
/// they are kept out of the transcript on purpose.
fn map_worker_event(turn_id: &TurnId, event: &WorkerEvent) -> Vec<SessionEvent> {
    match event {
        WorkerEvent::ContentDelta { text } => vec![SessionEvent::AssistantDelta {
            turn_id: turn_id.clone(),
            chunk: text.clone(),
        }],
        WorkerEvent::ToolStart { name, args } => vec![SessionEvent::ToolStart {
            turn_id: turn_id.clone(),
            name: name.clone(),
            args: args.clone(),
        }],
        WorkerEvent::ToolEnd {
            name,
            result_preview,
        } => vec![SessionEvent::ToolEnd {
            turn_id: turn_id.clone(),
            name: name.clone(),
            outcome: ToolOutcome::Ok {
                summary: result_preview.clone(),
            },
        }],
        WorkerEvent::Usage {
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cost_micros,
            cost_currency,
            cached_tokens,
            cache_write_tokens,
            reasoning_tokens,
            generation_id: _,
        } => vec![SessionEvent::Usage {
            turn_id: turn_id.clone(),
            usage: SessionUsage {
                prompt_tokens: *prompt_tokens,
                completion_tokens: *completion_tokens,
                total_tokens: *total_tokens,
                cost_micros: *cost_micros,
                cost_currency: cost_currency.clone(),
                cached_tokens: *cached_tokens,
                cache_write_tokens: *cache_write_tokens,
                reasoning_tokens: *reasoning_tokens,
            },
        }],
        WorkerEvent::Error { message, .. } => vec![SessionEvent::Error {
            turn_id: Some(turn_id.clone()),
            message: message.clone(),
            at: Utc::now(),
        }],
        // Operational / control-plane events — not part of the transcript.
        WorkerEvent::Heartbeat { .. }
        | WorkerEvent::PermissionRequest { .. }
        | WorkerEvent::PermissionDenied { .. }
        | WorkerEvent::PlanComplete { .. }
        | WorkerEvent::RoutedModel { .. }
        | WorkerEvent::ContextPrune { .. }
        | WorkerEvent::ContextCompact {}
        | WorkerEvent::ContextHandoff {} => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_content_delta_to_assistant_delta() {
        let turn = TurnId::from_raw("turn-1");
        let mapped = map_worker_event(&turn, &WorkerEvent::ContentDelta { text: "hi".into() });
        assert_eq!(mapped.len(), 1);
        assert!(matches!(
            &mapped[0],
            SessionEvent::AssistantDelta { chunk, .. } if chunk == "hi"
        ));
    }

    #[test]
    fn map_usage_to_session_usage() {
        let turn = TurnId::from_raw("turn-1");
        let mapped = map_worker_event(
            &turn,
            &WorkerEvent::Usage {
                prompt_tokens: 10,
                completion_tokens: 20,
                total_tokens: 30,
                cost_micros: Some(123),
                cost_currency: Some("USD".into()),
                cached_tokens: Some(4),
                cache_write_tokens: Some(5),
                reasoning_tokens: Some(6),
                generation_id: Some("gen-1".into()),
            },
        );
        match &mapped[0] {
            SessionEvent::Usage { usage, .. } => {
                assert_eq!(usage.prompt_tokens, 10);
                assert_eq!(usage.completion_tokens, 20);
                assert_eq!(usage.total_tokens, 30);
                assert_eq!(usage.cost_micros, Some(123));
                assert_eq!(usage.cost_currency.as_deref(), Some("USD"));
                assert_eq!(usage.cached_tokens, Some(4));
                assert_eq!(usage.cache_write_tokens, Some(5));
                assert_eq!(usage.reasoning_tokens, Some(6));
            }
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    #[test]
    fn map_tool_end_to_session_tool_end_ok() {
        let turn = TurnId::from_raw("turn-1");
        let mapped = map_worker_event(
            &turn,
            &WorkerEvent::ToolEnd {
                name: "bash".into(),
                result_preview: "exit 0".into(),
            },
        );
        match &mapped[0] {
            SessionEvent::ToolEnd { name, outcome, .. } => {
                assert_eq!(name, "bash");
                assert!(matches!(outcome, ToolOutcome::Ok { summary } if summary == "exit 0"));
            }
            other => panic!("expected ToolEnd, got {other:?}"),
        }
    }

    #[test]
    fn map_heartbeat_yields_no_session_events() {
        let turn = TurnId::from_raw("turn-1");
        assert!(map_worker_event(&turn, &WorkerEvent::Heartbeat { duration_ms: 1000 }).is_empty());
    }

    #[test]
    fn map_permission_request_yields_no_session_events() {
        let turn = TurnId::from_raw("turn-1");
        assert!(map_worker_event(
            &turn,
            &WorkerEvent::PermissionRequest {
                name: "bash".into(),
                reason: None,
            }
        )
        .is_empty());
    }

    #[test]
    fn turn_status_for_each_result_status() {
        let mut outcome = SendOutcome {
            id: "x".into(),
            status: ResultStatus::Ok,
            response: None,
            tool_calls_made: vec![],
            usage: None,
            iterations: 0,
            error: None,
            events: vec![],
            model_latency_ms: None,
            tool_latency_ms: None,
            total_latency_ms: None,
            confidence: None,
        };
        assert_eq!(turn_status_for(&outcome), TurnStatus::Ok);
        outcome.status = ResultStatus::Cancelled;
        assert_eq!(turn_status_for(&outcome), TurnStatus::Cancelled);
        outcome.status = ResultStatus::Error;
        assert_eq!(turn_status_for(&outcome), TurnStatus::Error);
    }
}

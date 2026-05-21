use std::fmt;
use std::ops::Add;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::id::OrbId;

/// Identifier for a long-lived conversation session.
///
/// Format: `session-<8 hex chars>`. Random (UUID-derived); not content-addressed
/// since sessions are not deduplicated by content.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    /// Generates a new random session id.
    pub fn new() -> Self {
        Self(format!("session-{}", short_uuid()))
    }

    /// Wraps a raw id string (for deserialization or tests).
    pub fn from_raw(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Returns the string representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Identifier for a single user turn within a session.
///
/// Format: `turn-<8 hex chars>`. Unique across sessions; carried on every event
/// that belongs to the turn (assistant message, tool calls, usage).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TurnId(String);

impl TurnId {
    pub fn new() -> Self {
        Self(format!("turn-{}", short_uuid()))
    }

    pub fn from_raw(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for TurnId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TurnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn short_uuid() -> String {
    Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(8)
        .collect()
}

/// Lifecycle state of a conversation session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Worker is alive and routing turns.
    Active,
    /// Worker has been detached after idle timeout; transcript intact, can rehydrate.
    Idle,
    /// Terminal state — no further events accepted.
    Closed,
}

/// Reason a session was closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CloseReason {
    /// User exited via `/exit` or Ctrl-D.
    UserExit,
    /// Worker crashed and could not be restarted.
    WorkerCrash { detail: String },
    /// Idle timeout exceeded the configured ceiling and session was finalized.
    IdleTimeout,
    /// Size cap exceeded; session can no longer be appended to.
    SizeCapExceeded,
    /// Catch-all for other reasons (admin close, test, etc.).
    Other { detail: String },
}

/// Token usage snapshot. Mirrors `orboros::ipc::Usage` shape; kept local to
/// the lib crate so persistence types don't depend on the IPC types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SessionUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

impl Add for SessionUsage {
    type Output = SessionUsage;

    fn add(self, other: SessionUsage) -> SessionUsage {
        SessionUsage {
            prompt_tokens: self.prompt_tokens.saturating_add(other.prompt_tokens),
            completion_tokens: self
                .completion_tokens
                .saturating_add(other.completion_tokens),
            total_tokens: self.total_tokens.saturating_add(other.total_tokens),
        }
    }
}

/// Outcome of a tool call invoked during a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolOutcome {
    Ok { summary: String },
    Err { message: String },
}

/// Header line of a session transcript. Written once at create time;
/// `SessionStore::load` returns it alongside the events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInit {
    pub id: SessionId,
    pub created_at: DateTime<Utc>,
    pub model: String,
    pub system_prompt: Option<String>,
    pub cwd: Option<String>,
    pub linked_orb: Option<OrbId>,
}

/// One event in a session transcript. JSONL line shape:
/// `{"type": "user_message", ...}` (internally tagged via serde).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    UserMessage {
        turn_id: TurnId,
        content: String,
        at: DateTime<Utc>,
    },
    AssistantDelta {
        turn_id: TurnId,
        chunk: String,
    },
    AssistantMessage {
        turn_id: TurnId,
        content: String,
        at: DateTime<Utc>,
    },
    ToolStart {
        turn_id: TurnId,
        name: String,
        args: serde_json::Value,
    },
    ToolEnd {
        turn_id: TurnId,
        name: String,
        outcome: ToolOutcome,
    },
    Usage {
        turn_id: TurnId,
        usage: SessionUsage,
    },
    OrbSpawned {
        turn_id: TurnId,
        orb_id: OrbId,
    },
    OrbResult {
        turn_id: TurnId,
        orb_id: OrbId,
        summary: String,
    },
    Error {
        turn_id: Option<TurnId>,
        message: String,
        at: DateTime<Utc>,
    },
    Cancelled {
        turn_id: TurnId,
        at: DateTime<Utc>,
    },
    /// The worker was restarted mid-session (e.g. `/clear` discarded the
    /// LLM context, `/model` switched to a new model). The transcript
    /// continues; subsequent turns run against a fresh worker.
    ContextReset {
        turn_id: TurnId,
        reason: String,
        at: DateTime<Utc>,
    },
    StatusChanged {
        to: SessionStatus,
        reason: Option<CloseReason>,
        at: DateTime<Utc>,
    },
}

/// In-memory snapshot of a session reconstructed from its transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub init: SessionInit,
    pub status: SessionStatus,
    pub turn_count: u32,
    pub total_usage: SessionUsage,
    pub closed_at: Option<DateTime<Utc>>,
    pub close_reason: Option<CloseReason>,
}

impl Session {
    /// Builds a fresh `Session` from an init header (no events yet).
    pub fn from_init(init: SessionInit) -> Self {
        Self {
            init,
            status: SessionStatus::Active,
            turn_count: 0,
            total_usage: SessionUsage::default(),
            closed_at: None,
            close_reason: None,
        }
    }

    /// Folds an event into the session snapshot. Idempotent on replay.
    pub fn apply(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::UserMessage { .. } => {
                self.turn_count = self.turn_count.saturating_add(1);
            }
            SessionEvent::Usage { usage, .. } => {
                self.total_usage = self.total_usage + *usage;
            }
            SessionEvent::StatusChanged { to, reason, at } => {
                self.status = *to;
                if matches!(to, SessionStatus::Closed) {
                    self.closed_at = Some(*at);
                    self.close_reason.clone_from(reason);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-05-21T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn session_id_is_unique_and_prefixed() {
        let a = SessionId::new();
        let b = SessionId::new();
        assert_ne!(a, b);
        assert!(a.as_str().starts_with("session-"));
        assert!(b.as_str().starts_with("session-"));
    }

    #[test]
    fn turn_id_is_unique_and_prefixed() {
        let a = TurnId::new();
        let b = TurnId::new();
        assert_ne!(a, b);
        assert!(a.as_str().starts_with("turn-"));
    }

    #[test]
    fn session_id_serde_is_transparent() {
        let id = SessionId::from_raw("session-abcd1234");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"session-abcd1234\"");
        let parsed: SessionId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn session_init_round_trips() {
        let init = SessionInit {
            id: SessionId::from_raw("session-aaaa1111"),
            created_at: now(),
            model: "openrouter/free".into(),
            system_prompt: Some("be helpful".into()),
            cwd: Some("/tmp/proj".into()),
            linked_orb: Some(OrbId::from_raw("orb-abc")),
        };
        let json = serde_json::to_string(&init).unwrap();
        let back: SessionInit = serde_json::from_str(&json).unwrap();
        assert_eq!(init, back);
    }

    #[test]
    fn session_event_user_message_tag() {
        let ev = SessionEvent::UserMessage {
            turn_id: TurnId::from_raw("turn-1"),
            content: "hello".into(),
            at: now(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"type\":\"user_message\""));
        let back: SessionEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn session_event_tool_end_with_outcome() {
        let ev = SessionEvent::ToolEnd {
            turn_id: TurnId::from_raw("turn-1"),
            name: "bash".into(),
            outcome: ToolOutcome::Ok {
                summary: "exit 0".into(),
            },
        };
        let json = serde_json::to_string(&ev).unwrap();
        let back: SessionEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn session_event_status_changed_serializes() {
        let ev = SessionEvent::StatusChanged {
            to: SessionStatus::Closed,
            reason: Some(CloseReason::UserExit),
            at: now(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"type\":\"status_changed\""));
        let back: SessionEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn session_event_context_reset_round_trips() {
        let ev = SessionEvent::ContextReset {
            turn_id: TurnId::from_raw("turn-1"),
            reason: "clear".into(),
            at: now(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"type\":\"context_reset\""));
        let back: SessionEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn close_reason_variants_round_trip() {
        for reason in [
            CloseReason::UserExit,
            CloseReason::IdleTimeout,
            CloseReason::SizeCapExceeded,
            CloseReason::WorkerCrash {
                detail: "EPIPE".into(),
            },
            CloseReason::Other {
                detail: "test".into(),
            },
        ] {
            let json = serde_json::to_string(&reason).unwrap();
            let back: CloseReason = serde_json::from_str(&json).unwrap();
            assert_eq!(reason, back);
        }
    }

    #[test]
    fn session_apply_user_message_increments_turn_count() {
        let init = SessionInit {
            id: SessionId::new(),
            created_at: now(),
            model: "m".into(),
            system_prompt: None,
            cwd: None,
            linked_orb: None,
        };
        let mut s = Session::from_init(init);
        assert_eq!(s.turn_count, 0);
        s.apply(&SessionEvent::UserMessage {
            turn_id: TurnId::new(),
            content: "hi".into(),
            at: now(),
        });
        assert_eq!(s.turn_count, 1);
    }

    #[test]
    fn session_usage_addition_via_add_operator() {
        let a = SessionUsage {
            prompt_tokens: 1,
            completion_tokens: 2,
            total_tokens: 3,
        };
        let b = SessionUsage {
            prompt_tokens: 10,
            completion_tokens: 20,
            total_tokens: 30,
        };
        let sum = a + b;
        assert_eq!(sum.prompt_tokens, 11);
        assert_eq!(sum.completion_tokens, 22);
        assert_eq!(sum.total_tokens, 33);
    }

    #[test]
    fn session_apply_usage_accumulates() {
        let init = SessionInit {
            id: SessionId::new(),
            created_at: now(),
            model: "m".into(),
            system_prompt: None,
            cwd: None,
            linked_orb: None,
        };
        let mut s = Session::from_init(init);
        s.apply(&SessionEvent::Usage {
            turn_id: TurnId::new(),
            usage: SessionUsage {
                prompt_tokens: 10,
                completion_tokens: 20,
                total_tokens: 30,
            },
        });
        s.apply(&SessionEvent::Usage {
            turn_id: TurnId::new(),
            usage: SessionUsage {
                prompt_tokens: 5,
                completion_tokens: 7,
                total_tokens: 12,
            },
        });
        assert_eq!(s.total_usage.prompt_tokens, 15);
        assert_eq!(s.total_usage.completion_tokens, 27);
        assert_eq!(s.total_usage.total_tokens, 42);
    }

    #[test]
    fn session_apply_status_changed_to_closed_records_close_metadata() {
        let init = SessionInit {
            id: SessionId::new(),
            created_at: now(),
            model: "m".into(),
            system_prompt: None,
            cwd: None,
            linked_orb: None,
        };
        let mut s = Session::from_init(init);
        let at = now();
        s.apply(&SessionEvent::StatusChanged {
            to: SessionStatus::Closed,
            reason: Some(CloseReason::UserExit),
            at,
        });
        assert_eq!(s.status, SessionStatus::Closed);
        assert_eq!(s.closed_at, Some(at));
        assert_eq!(s.close_reason, Some(CloseReason::UserExit));
    }

    #[test]
    fn session_apply_status_changed_to_idle_does_not_record_close_metadata() {
        let init = SessionInit {
            id: SessionId::new(),
            created_at: now(),
            model: "m".into(),
            system_prompt: None,
            cwd: None,
            linked_orb: None,
        };
        let mut s = Session::from_init(init);
        s.apply(&SessionEvent::StatusChanged {
            to: SessionStatus::Idle,
            reason: None,
            at: now(),
        });
        assert_eq!(s.status, SessionStatus::Idle);
        assert!(s.closed_at.is_none());
    }

    #[test]
    fn session_usage_add_saturates() {
        let big = SessionUsage {
            prompt_tokens: u32::MAX,
            completion_tokens: 0,
            total_tokens: 0,
        };
        let one = SessionUsage {
            prompt_tokens: 1,
            completion_tokens: 0,
            total_tokens: 0,
        };
        assert_eq!((big + one).prompt_tokens, u32::MAX);
    }
}

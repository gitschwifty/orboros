use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::id::OrbId;
use crate::orb::{OrbPhase, OrbStatus};

/// Types of audit events that can be logged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    Created,
    StatusChanged,
    PhaseChanged,
    PriorityChanged,
    Assigned,
    Commented,
    DepAdded,
    DepRemoved,
    Deferred,
    Undeferred,
    Tombstoned,
}

/// A single audit event recording a mutation on an orb.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub id: Uuid,
    pub orb_id: OrbId,
    pub event_type: EventType,
    pub timestamp: DateTime<Utc>,
    pub actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

impl AuditEvent {
    /// Creates a new audit event with a generated ID and current timestamp.
    pub fn new(
        orb_id: OrbId,
        event_type: EventType,
        actor: impl Into<String>,
        details: Option<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            orb_id,
            event_type,
            timestamp: Utc::now(),
            actor: actor.into(),
            details,
        }
    }
}

/// A comment attached to an orb.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    pub id: Uuid,
    pub orb_id: OrbId,
    pub author: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

impl Comment {
    /// Creates a new comment with a generated ID and current timestamp.
    pub fn new(orb_id: OrbId, author: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            orb_id,
            author: author.into(),
            body: body.into(),
            created_at: Utc::now(),
        }
    }
}

// ── Auto-logging helper functions ────────────────────────────────────

/// Logs a creation event.
///
/// # Errors
///
/// Returns an IO error if writing to the audit store fails.
pub fn log_creation(
    store: &crate::audit_store::AuditStore,
    orb_id: &OrbId,
    actor: &str,
) -> std::io::Result<()> {
    let event = AuditEvent::new(orb_id.clone(), EventType::Created, actor, None);
    store.log_event(&event)
}

/// Logs a status change event.
///
/// # Errors
///
/// Returns an IO error if writing to the audit store fails.
pub fn log_status_change(
    store: &crate::audit_store::AuditStore,
    orb_id: &OrbId,
    old_status: OrbStatus,
    new_status: OrbStatus,
    actor: &str,
) -> std::io::Result<()> {
    let details = format!(
        "{} -> {}",
        serde_json::to_value(old_status).unwrap_or_default(),
        serde_json::to_value(new_status).unwrap_or_default()
    );
    let event = AuditEvent::new(
        orb_id.clone(),
        EventType::StatusChanged,
        actor,
        Some(details),
    );
    store.log_event(&event)
}

/// Logs a phase change event.
///
/// # Errors
///
/// Returns an IO error if writing to the audit store fails.
pub fn log_phase_change(
    store: &crate::audit_store::AuditStore,
    orb_id: &OrbId,
    old_phase: OrbPhase,
    new_phase: OrbPhase,
    actor: &str,
) -> std::io::Result<()> {
    let details = format!(
        "{} -> {}",
        serde_json::to_value(old_phase).unwrap_or_default(),
        serde_json::to_value(new_phase).unwrap_or_default()
    );
    let event = AuditEvent::new(
        orb_id.clone(),
        EventType::PhaseChanged,
        actor,
        Some(details),
    );
    store.log_event(&event)
}

/// Logs a priority change event.
///
/// # Errors
///
/// Returns an IO error if writing to the audit store fails.
pub fn log_priority_change(
    store: &crate::audit_store::AuditStore,
    orb_id: &OrbId,
    old_priority: u8,
    new_priority: u8,
    actor: &str,
) -> std::io::Result<()> {
    let details = format!("{old_priority} -> {new_priority}");
    let event = AuditEvent::new(
        orb_id.clone(),
        EventType::PriorityChanged,
        actor,
        Some(details),
    );
    store.log_event(&event)
}

/// Logs an assignment event.
///
/// # Errors
///
/// Returns an IO error if writing to the audit store fails.
pub fn log_assigned(
    store: &crate::audit_store::AuditStore,
    orb_id: &OrbId,
    assignee: &str,
    actor: &str,
) -> std::io::Result<()> {
    let event = AuditEvent::new(
        orb_id.clone(),
        EventType::Assigned,
        actor,
        Some(format!("assigned to {assignee}")),
    );
    store.log_event(&event)
}

/// Logs a dependency-added event.
///
/// # Errors
///
/// Returns an IO error if writing to the audit store fails.
pub fn log_dep_added(
    store: &crate::audit_store::AuditStore,
    orb_id: &OrbId,
    dep_id: &OrbId,
    actor: &str,
) -> std::io::Result<()> {
    let event = AuditEvent::new(
        orb_id.clone(),
        EventType::DepAdded,
        actor,
        Some(format!("dependency: {dep_id}")),
    );
    store.log_event(&event)
}

/// Logs a dependency-removed event.
///
/// # Errors
///
/// Returns an IO error if writing to the audit store fails.
pub fn log_dep_removed(
    store: &crate::audit_store::AuditStore,
    orb_id: &OrbId,
    dep_id: &OrbId,
    actor: &str,
) -> std::io::Result<()> {
    let event = AuditEvent::new(
        orb_id.clone(),
        EventType::DepRemoved,
        actor,
        Some(format!("removed dependency: {dep_id}")),
    );
    store.log_event(&event)
}

/// Logs a deferred event.
///
/// # Errors
///
/// Returns an IO error if writing to the audit store fails.
pub fn log_deferred(
    store: &crate::audit_store::AuditStore,
    orb_id: &OrbId,
    actor: &str,
) -> std::io::Result<()> {
    let event = AuditEvent::new(orb_id.clone(), EventType::Deferred, actor, None);
    store.log_event(&event)
}

/// Logs an undeferred event.
///
/// # Errors
///
/// Returns an IO error if writing to the audit store fails.
pub fn log_undeferred(
    store: &crate::audit_store::AuditStore,
    orb_id: &OrbId,
    actor: &str,
) -> std::io::Result<()> {
    let event = AuditEvent::new(orb_id.clone(), EventType::Undeferred, actor, None);
    store.log_event(&event)
}

/// Logs a tombstoned event.
///
/// # Errors
///
/// Returns an IO error if writing to the audit store fails.
pub fn log_tombstoned(
    store: &crate::audit_store::AuditStore,
    orb_id: &OrbId,
    reason: Option<&str>,
    actor: &str,
) -> std::io::Result<()> {
    let event = AuditEvent::new(
        orb_id.clone(),
        EventType::Tombstoned,
        actor,
        reason.map(String::from),
    );
    store.log_event(&event)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_event_new_sets_fields() {
        let orb_id = OrbId::from_raw("orb-abc");
        let event = AuditEvent::new(
            orb_id.clone(),
            EventType::Created,
            "alice",
            Some("initial creation".into()),
        );
        assert_eq!(event.orb_id, orb_id);
        assert_eq!(event.event_type, EventType::Created);
        assert_eq!(event.actor, "alice");
        assert_eq!(event.details.as_deref(), Some("initial creation"));
    }

    #[test]
    fn audit_event_serde_round_trip() {
        let orb_id = OrbId::from_raw("orb-xyz");
        let event = AuditEvent::new(orb_id, EventType::StatusChanged, "bob", None);
        let json = serde_json::to_string(&event).unwrap();
        let parsed: AuditEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, event.id);
        assert_eq!(parsed.orb_id, event.orb_id);
        assert_eq!(parsed.event_type, EventType::StatusChanged);
        assert_eq!(parsed.actor, "bob");
        assert!(parsed.details.is_none());
    }

    #[test]
    fn event_type_serde_all_variants() {
        let variants = vec![
            EventType::Created,
            EventType::StatusChanged,
            EventType::PhaseChanged,
            EventType::PriorityChanged,
            EventType::Assigned,
            EventType::Commented,
            EventType::DepAdded,
            EventType::DepRemoved,
            EventType::Deferred,
            EventType::Undeferred,
            EventType::Tombstoned,
        ];
        for variant in variants {
            let json = serde_json::to_string(&variant).unwrap();
            let parsed: EventType = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn event_type_snake_case_serialization() {
        assert_eq!(
            serde_json::to_string(&EventType::StatusChanged).unwrap(),
            "\"status_changed\""
        );
        assert_eq!(
            serde_json::to_string(&EventType::DepAdded).unwrap(),
            "\"dep_added\""
        );
    }

    #[test]
    fn comment_new_sets_fields() {
        let orb_id = OrbId::from_raw("orb-abc");
        let comment = Comment::new(orb_id.clone(), "alice", "This looks good");
        assert_eq!(comment.orb_id, orb_id);
        assert_eq!(comment.author, "alice");
        assert_eq!(comment.body, "This looks good");
    }

    #[test]
    fn comment_serde_round_trip() {
        let comment = Comment::new(OrbId::from_raw("orb-xyz"), "bob", "Needs work");
        let json = serde_json::to_string(&comment).unwrap();
        let parsed: Comment = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, comment.id);
        assert_eq!(parsed.orb_id, comment.orb_id);
        assert_eq!(parsed.author, "bob");
        assert_eq!(parsed.body, "Needs work");
    }

    #[test]
    fn audit_event_without_details_omits_field() {
        let event = AuditEvent::new(
            OrbId::from_raw("orb-abc"),
            EventType::Created,
            "system",
            None,
        );
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("details"));
    }

    // ── Auto-logging function tests ──────────────────────────────────

    #[test]
    fn log_creation_writes_event() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::audit_store::AuditStore::new(dir.path().join("events.jsonl"));
        let orb_id = OrbId::from_raw("orb-test");

        log_creation(&store, &orb_id, "alice").unwrap();

        let events = store.events_for_orb(&orb_id).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::Created);
        assert_eq!(events[0].actor, "alice");
    }

    #[test]
    fn log_status_change_includes_details() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::audit_store::AuditStore::new(dir.path().join("events.jsonl"));
        let orb_id = OrbId::from_raw("orb-test");

        log_status_change(
            &store,
            &orb_id,
            OrbStatus::Pending,
            OrbStatus::Active,
            "bob",
        )
        .unwrap();

        let events = store.events_for_orb(&orb_id).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::StatusChanged);
        let details = events[0].details.as_deref().unwrap();
        assert!(details.contains("pending"));
        assert!(details.contains("active"));
    }

    #[test]
    fn log_phase_change_includes_details() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::audit_store::AuditStore::new(dir.path().join("events.jsonl"));
        let orb_id = OrbId::from_raw("orb-test");

        log_phase_change(
            &store,
            &orb_id,
            OrbPhase::Pending,
            OrbPhase::Speccing,
            "carol",
        )
        .unwrap();

        let events = store.events_for_orb(&orb_id).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::PhaseChanged);
    }

    #[test]
    fn log_priority_change_includes_values() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::audit_store::AuditStore::new(dir.path().join("events.jsonl"));
        let orb_id = OrbId::from_raw("orb-test");

        log_priority_change(&store, &orb_id, 3, 1, "dave").unwrap();

        let events = store.events_for_orb(&orb_id).unwrap();
        assert_eq!(events[0].event_type, EventType::PriorityChanged);
        assert_eq!(events[0].details.as_deref(), Some("3 -> 1"));
    }

    #[test]
    fn log_assigned_includes_assignee() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::audit_store::AuditStore::new(dir.path().join("events.jsonl"));
        let orb_id = OrbId::from_raw("orb-test");

        log_assigned(&store, &orb_id, "eve", "admin").unwrap();

        let events = store.events_for_orb(&orb_id).unwrap();
        assert_eq!(events[0].event_type, EventType::Assigned);
        assert!(events[0].details.as_deref().unwrap().contains("eve"));
    }

    #[test]
    fn log_dep_added_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::audit_store::AuditStore::new(dir.path().join("events.jsonl"));
        let orb_id = OrbId::from_raw("orb-a");
        let dep_id = OrbId::from_raw("orb-b");

        log_dep_added(&store, &orb_id, &dep_id, "system").unwrap();
        log_dep_removed(&store, &orb_id, &dep_id, "system").unwrap();

        let events = store.events_for_orb(&orb_id).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, EventType::DepAdded);
        assert_eq!(events[1].event_type, EventType::DepRemoved);
    }

    #[test]
    fn log_deferred_and_undeferred() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::audit_store::AuditStore::new(dir.path().join("events.jsonl"));
        let orb_id = OrbId::from_raw("orb-test");

        log_deferred(&store, &orb_id, "alice").unwrap();
        log_undeferred(&store, &orb_id, "alice").unwrap();

        let events = store.events_for_orb(&orb_id).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, EventType::Deferred);
        assert_eq!(events[1].event_type, EventType::Undeferred);
    }

    #[test]
    fn log_tombstoned_with_reason() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::audit_store::AuditStore::new(dir.path().join("events.jsonl"));
        let orb_id = OrbId::from_raw("orb-test");

        log_tombstoned(&store, &orb_id, Some("duplicate"), "admin").unwrap();

        let events = store.events_for_orb(&orb_id).unwrap();
        assert_eq!(events[0].event_type, EventType::Tombstoned);
        assert_eq!(events[0].details.as_deref(), Some("duplicate"));
    }

    #[test]
    fn log_tombstoned_without_reason() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::audit_store::AuditStore::new(dir.path().join("events.jsonl"));
        let orb_id = OrbId::from_raw("orb-test");

        log_tombstoned(&store, &orb_id, None, "admin").unwrap();

        let events = store.events_for_orb(&orb_id).unwrap();
        assert!(events[0].details.is_none());
    }
}

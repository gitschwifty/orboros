use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::id::{self, OrbId};
use crate::task::TaskStatus;

/// Type classification for an Orb.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrbType {
    Epic,
    Feature,
    Task,
    Bug,
    Chore,
    Docs,
    Custom(String),
}

impl OrbType {
    /// Returns true if this type uses the phase lifecycle (epic/feature).
    pub fn uses_phase(&self) -> bool {
        matches!(self, Self::Epic | Self::Feature)
    }

    /// Returns the serde-compatible string for content hashing.
    pub fn as_hash_str(&self) -> &str {
        match self {
            Self::Epic => "epic",
            Self::Feature => "feature",
            Self::Task => "task",
            Self::Bug => "bug",
            Self::Chore => "chore",
            Self::Docs => "docs",
            Self::Custom(s) => s.as_str(),
        }
    }
}

/// Status lifecycle for tasks, bugs, chores, docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrbStatus {
    Draft,
    Pending,
    Active,
    Review,
    Done,
    Failed,
    Cancelled,
    Deferred,
    Tombstone,
}

/// Phase lifecycle for epics and features.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrbPhase {
    Draft,
    Pending,
    Speccing,
    Decomposing,
    Refining,
    Review,
    Waiting,
    Executing,
    Reevaluating,
    Done,
    Failed,
    Cancelled,
    Deferred,
    Tombstone,
}

/// Difficulty estimate for an orb.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Difficulty {
    Trivial,
    Easy,
    Medium,
    Hard,
    Unknown,
}

/// Priority display names.
pub fn priority_name(priority: u8) -> &'static str {
    match priority {
        1 => "Critical",
        2 => "High",
        3 => "Medium",
        4 => "Low",
        5 => "Backlog",
        _ => "Unknown",
    }
}

/// Execution metadata for a completed orb.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecutionMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dispatched_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u32>,
    #[serde(default)]
    pub retries: u32,
}

/// The core Orb struct — replaces the former `Task`.
///
/// All new fields default to `None`/empty so existing Task JSONL
/// can be deserialized as Orb without breaking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Orb {
    // ── Identity ──────────────────────────────────────────────
    /// Content-addressed ID (e.g. "orb-k4f" or "orb-k4f.1").
    pub id: OrbId,

    /// Content hash for change detection (excludes timestamps/metadata).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,

    // ── Core fields ──────────────────────────────────────────
    pub title: String,
    pub description: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance_criteria: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scope: Vec<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,

    // ── Type & lifecycle ─────────────────────────────────────
    #[serde(default = "default_orb_type")]
    pub orb_type: OrbType,

    /// Status lifecycle (tasks, bugs, chores, docs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<OrbStatus>,

    /// Phase lifecycle (epics, features).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<OrbPhase>,

    // ── Priority & estimation ────────────────────────────────
    /// Priority 1 (Critical) to 5 (Backlog).
    #[serde(default = "default_priority")]
    pub priority: u8,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_minutes: Option<u32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub difficulty: Option<Difficulty>,

    // ── Hierarchy ────────────────────────────────────────────
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<OrbId>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_id: Option<OrbId>,

    // ── Timestamps ───────────────────────────────────────────
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<DateTime<Utc>>,

    // ── Tombstone ────────────────────────────────────────────
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<DateTime<Utc>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_reason: Option<String>,

    // ── Execution ────────────────────────────────────────────
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<ExecutionMeta>,

    /// Final response/result text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,

    // ── HITL ─────────────────────────────────────────────────
    #[serde(default)]
    pub requires_approval: bool,

    // ── External ─────────────────────────────────────────────
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_ref: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_model: Option<String>,

    // ── Legacy compatibility ─────────────────────────────────
    /// Legacy UUID-based ID for backwards compat with Task JSONL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_id: Option<uuid::Uuid>,

    /// Legacy `worker_model` (moved to `execution.worker_model`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_model: Option<String>,
}

fn default_orb_type() -> OrbType {
    OrbType::Task
}

fn default_priority() -> u8 {
    3
}

impl Orb {
    /// Creates a new pending Orb of type Task.
    pub fn new(title: impl Into<String>, description: impl Into<String>) -> Self {
        let now = Utc::now();
        let title = title.into();
        let description = description.into();

        let id = OrbId::generate(
            &title,
            &description,
            "system",
            now.timestamp_nanos_opt()
                .map_or(0, |n| u128::from(n.cast_unsigned())),
            &std::collections::HashSet::new(),
        );

        Self {
            id,
            content_hash: None,
            title,
            description,
            design: None,
            acceptance_criteria: None,
            scope: vec![],
            labels: vec![],
            orb_type: OrbType::Task,
            status: Some(OrbStatus::Pending),
            phase: None,
            priority: 3,
            estimated_minutes: None,
            difficulty: None,
            parent_id: None,
            root_id: None,
            created_at: now,
            updated_at: now,
            closed_at: None,
            deleted_at: None,
            delete_reason: None,
            execution: None,
            result: None,
            requires_approval: false,
            external_ref: None,
            preferred_model: None,
            legacy_id: None,
            worker_model: None,
        }
    }

    /// Creates a new Orb with a specific type, setting the appropriate lifecycle field.
    #[must_use]
    pub fn with_type(mut self, orb_type: OrbType) -> Self {
        if orb_type.uses_phase() {
            self.status = None;
            self.phase = Some(OrbPhase::Pending);
        } else {
            self.status = Some(OrbStatus::Pending);
            self.phase = None;
        }
        self.orb_type = orb_type;
        self
    }

    /// Sets the priority (clamped to 1-5).
    #[must_use]
    pub fn with_priority(mut self, priority: u8) -> Self {
        self.priority = priority.clamp(1, 5);
        self
    }

    /// Sets the parent ID and root ID.
    #[must_use]
    pub fn with_parent(mut self, parent_id: OrbId, root_id: Option<OrbId>) -> Self {
        self.root_id = root_id.or_else(|| Some(parent_id.clone()));
        self.parent_id = Some(parent_id);
        self
    }

    /// Computes and sets the content hash.
    pub fn update_content_hash(&mut self) {
        self.content_hash = Some(id::content_hash(
            &self.title,
            &self.description,
            self.design.as_deref(),
            self.acceptance_criteria.as_deref(),
            self.orb_type.as_hash_str(),
            &self.scope,
            self.priority,
        ));
    }

    /// Returns the effective status, mapping from either status or phase.
    /// This provides backwards compatibility with code expecting `TaskStatus`.
    pub fn effective_status(&self) -> TaskStatus {
        if let Some(status) = self.status {
            match status {
                OrbStatus::Draft | OrbStatus::Pending | OrbStatus::Deferred => TaskStatus::Pending,
                OrbStatus::Active => TaskStatus::Active,
                OrbStatus::Review => TaskStatus::Review,
                OrbStatus::Done => TaskStatus::Done,
                OrbStatus::Failed => TaskStatus::Failed,
                OrbStatus::Cancelled | OrbStatus::Tombstone => TaskStatus::Cancelled,
            }
        } else if let Some(phase) = self.phase {
            match phase {
                OrbPhase::Draft | OrbPhase::Pending | OrbPhase::Deferred | OrbPhase::Waiting => {
                    TaskStatus::Pending
                }
                OrbPhase::Speccing
                | OrbPhase::Decomposing
                | OrbPhase::Refining
                | OrbPhase::Executing
                | OrbPhase::Reevaluating => TaskStatus::Active,
                OrbPhase::Review => TaskStatus::Review,
                OrbPhase::Done => TaskStatus::Done,
                OrbPhase::Failed => TaskStatus::Failed,
                OrbPhase::Cancelled | OrbPhase::Tombstone => TaskStatus::Cancelled,
            }
        } else {
            TaskStatus::Pending
        }
    }

    /// Returns true if this orb is tombstoned (soft-deleted).
    pub fn is_tombstoned(&self) -> bool {
        self.deleted_at.is_some()
            || self.status == Some(OrbStatus::Tombstone)
            || self.phase == Some(OrbPhase::Tombstone)
    }

    /// Returns true if the orb can be deferred from its current state.
    pub fn can_defer(&self) -> bool {
        if let Some(status) = self.status {
            matches!(status, OrbStatus::Pending | OrbStatus::Draft)
        } else if let Some(phase) = self.phase {
            matches!(
                phase,
                OrbPhase::Pending | OrbPhase::Waiting | OrbPhase::Draft
            )
        } else {
            false
        }
    }

    /// Defers this orb. Returns false if deferral is not allowed.
    pub fn defer(&mut self) -> bool {
        if !self.can_defer() {
            return false;
        }
        if self.status.is_some() {
            self.status = Some(OrbStatus::Deferred);
        } else {
            self.phase = Some(OrbPhase::Deferred);
        }
        self.updated_at = Utc::now();
        true
    }

    /// Undefers this orb, restoring to the appropriate default state.
    pub fn undefer(&mut self) {
        if self.status == Some(OrbStatus::Deferred) {
            self.status = Some(OrbStatus::Pending);
        } else if self.phase == Some(OrbPhase::Deferred) {
            // Default: if has parent_id (has been decomposed), go to waiting; else pending
            if self.parent_id.is_some() {
                self.phase = Some(OrbPhase::Waiting);
            } else {
                self.phase = Some(OrbPhase::Pending);
            }
        }
        self.updated_at = Utc::now();
    }

    /// Soft-deletes (tombstones) this orb.
    pub fn tombstone(&mut self, reason: Option<String>) {
        let now = Utc::now();
        self.deleted_at = Some(now);
        self.delete_reason = reason;
        if self.status.is_some() {
            self.status = Some(OrbStatus::Tombstone);
        } else {
            self.phase = Some(OrbPhase::Tombstone);
        }
        self.updated_at = now;
    }

    /// Transitions status (for task/bug/chore/docs types).
    pub fn set_status(&mut self, new_status: OrbStatus) {
        self.status = Some(new_status);
        self.updated_at = Utc::now();
        if matches!(
            new_status,
            OrbStatus::Done | OrbStatus::Failed | OrbStatus::Cancelled
        ) {
            self.closed_at = Some(self.updated_at);
        }
    }

    /// Transitions phase (for epic/feature types).
    pub fn set_phase(&mut self, new_phase: OrbPhase) {
        self.phase = Some(new_phase);
        self.updated_at = Utc::now();
        if matches!(
            new_phase,
            OrbPhase::Done | OrbPhase::Failed | OrbPhase::Cancelled
        ) {
            self.closed_at = Some(self.updated_at);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_orb_is_pending_task() {
        let orb = Orb::new("Test orb", "Do something");
        assert_eq!(orb.status, Some(OrbStatus::Pending));
        assert_eq!(orb.orb_type, OrbType::Task);
        assert_eq!(orb.priority, 3);
        assert!(orb.result.is_none());
        assert!(orb.parent_id.is_none());
        assert!(orb.id.as_str().starts_with("orb-"));
    }

    #[test]
    fn with_type_epic_uses_phase() {
        let orb = Orb::new("Epic", "big thing").with_type(OrbType::Epic);
        assert_eq!(orb.orb_type, OrbType::Epic);
        assert_eq!(orb.phase, Some(OrbPhase::Pending));
        assert_eq!(orb.status, None);
    }

    #[test]
    fn priority_clamped() {
        let orb = Orb::new("Test", "test").with_priority(0);
        assert_eq!(orb.priority, 1);
        let orb = Orb::new("Test", "test").with_priority(10);
        assert_eq!(orb.priority, 5);
    }

    #[test]
    fn priority_display_names() {
        assert_eq!(priority_name(1), "Critical");
        assert_eq!(priority_name(2), "High");
        assert_eq!(priority_name(3), "Medium");
        assert_eq!(priority_name(4), "Low");
        assert_eq!(priority_name(5), "Backlog");
    }

    #[test]
    fn effective_status_maps_correctly() {
        let mut orb = Orb::new("Test", "test");
        assert_eq!(orb.effective_status(), TaskStatus::Pending);

        orb.set_status(OrbStatus::Active);
        assert_eq!(orb.effective_status(), TaskStatus::Active);

        orb.set_status(OrbStatus::Done);
        assert_eq!(orb.effective_status(), TaskStatus::Done);
    }

    #[test]
    fn effective_status_for_phase_types() {
        let mut orb = Orb::new("Epic", "big").with_type(OrbType::Epic);
        assert_eq!(orb.effective_status(), TaskStatus::Pending);

        orb.set_phase(OrbPhase::Speccing);
        assert_eq!(orb.effective_status(), TaskStatus::Active);

        orb.set_phase(OrbPhase::Waiting);
        assert_eq!(orb.effective_status(), TaskStatus::Pending);

        orb.set_phase(OrbPhase::Done);
        assert_eq!(orb.effective_status(), TaskStatus::Done);
    }

    #[test]
    fn defer_from_pending() {
        let mut orb = Orb::new("Test", "test");
        assert!(orb.can_defer());
        assert!(orb.defer());
        assert_eq!(orb.status, Some(OrbStatus::Deferred));
    }

    #[test]
    fn defer_from_active_fails() {
        let mut orb = Orb::new("Test", "test");
        orb.set_status(OrbStatus::Active);
        assert!(!orb.can_defer());
        assert!(!orb.defer());
        assert_eq!(orb.status, Some(OrbStatus::Active));
    }

    #[test]
    fn defer_epic_from_waiting() {
        let mut orb = Orb::new("Epic", "big").with_type(OrbType::Epic);
        orb.set_phase(OrbPhase::Waiting);
        assert!(orb.can_defer());
        assert!(orb.defer());
        assert_eq!(orb.phase, Some(OrbPhase::Deferred));
    }

    #[test]
    fn undefer_restores_pending() {
        let mut orb = Orb::new("Test", "test");
        orb.defer();
        orb.undefer();
        assert_eq!(orb.status, Some(OrbStatus::Pending));
    }

    #[test]
    fn undefer_epic_with_parent_restores_waiting() {
        let mut orb = Orb::new("Feature", "sub").with_type(OrbType::Feature);
        orb.parent_id = Some(OrbId::from_raw("orb-parent"));
        orb.set_phase(OrbPhase::Waiting);
        orb.defer();
        orb.undefer();
        assert_eq!(orb.phase, Some(OrbPhase::Waiting));
    }

    #[test]
    fn tombstone_sets_deleted_at() {
        let mut orb = Orb::new("Test", "test");
        assert!(!orb.is_tombstoned());
        orb.tombstone(Some("duplicate".into()));
        assert!(orb.is_tombstoned());
        assert!(orb.deleted_at.is_some());
        assert_eq!(orb.delete_reason.as_deref(), Some("duplicate"));
        assert_eq!(orb.status, Some(OrbStatus::Tombstone));
    }

    #[test]
    fn closed_at_set_on_terminal_status() {
        let mut orb = Orb::new("Test", "test");
        assert!(orb.closed_at.is_none());
        orb.set_status(OrbStatus::Done);
        assert!(orb.closed_at.is_some());
    }

    #[test]
    fn content_hash_computed() {
        let mut orb = Orb::new("Test", "description");
        orb.update_content_hash();
        assert!(orb.content_hash.is_some());

        let hash1 = orb.content_hash.clone();
        orb.description = "changed".into();
        orb.update_content_hash();
        assert_ne!(orb.content_hash, hash1);
    }

    #[test]
    fn content_hash_stable_on_metadata_change() {
        let mut orb = Orb::new("Test", "description");
        orb.update_content_hash();
        let hash1 = orb.content_hash.clone();

        // Metadata change — should NOT affect content hash
        orb.updated_at = Utc::now();
        orb.update_content_hash();
        assert_eq!(orb.content_hash, hash1);
    }

    #[test]
    fn serde_round_trip_full_orb() {
        let mut orb = Orb::new("Review auth", "Check error handling");
        orb.labels = vec!["security".into()];
        orb.scope = vec!["auth".into(), "jwt".into()];
        orb.design = Some("Use standard JWT validation".into());
        orb.execution = Some(ExecutionMeta {
            prompt_tokens: Some(100),
            completion_tokens: Some(50),
            total_tokens: Some(150),
            ..Default::default()
        });
        orb.update_content_hash();

        let json = serde_json::to_string(&orb).unwrap();
        let parsed: Orb = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, orb.id);
        assert_eq!(parsed.title, orb.title);
        assert_eq!(parsed.labels, orb.labels);
        assert_eq!(parsed.scope, orb.scope);
        assert_eq!(parsed.content_hash, orb.content_hash);
        assert_eq!(parsed.execution.as_ref().unwrap().prompt_tokens, Some(100));
    }

    #[test]
    fn backwards_compat_legacy_task_json() {
        // Simulate existing Task JSONL format
        let legacy_json = r#"{
            "id": "orb-legacy",
            "title": "Old task",
            "description": "From before the orb schema",
            "priority": 2,
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z"
        }"#;

        let orb: Orb = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(orb.title, "Old task");
        assert_eq!(orb.priority, 2);
        assert_eq!(orb.orb_type, OrbType::Task); // default
        assert!(orb.status.is_none()); // not in legacy JSON
        assert!(orb.scope.is_empty()); // default
    }

    #[test]
    fn with_parent_sets_root_id() {
        let parent_id = OrbId::from_raw("orb-parent");
        let orb = Orb::new("Child", "sub task").with_parent(parent_id.clone(), None);
        assert_eq!(orb.parent_id, Some(parent_id.clone()));
        assert_eq!(orb.root_id, Some(parent_id));
    }

    #[test]
    fn with_parent_preserves_explicit_root() {
        let parent_id = OrbId::from_raw("orb-parent");
        let root_id = OrbId::from_raw("orb-root");
        let orb =
            Orb::new("Child", "sub task").with_parent(parent_id.clone(), Some(root_id.clone()));
        assert_eq!(orb.parent_id, Some(parent_id));
        assert_eq!(orb.root_id, Some(root_id));
    }

    #[test]
    fn orb_type_serde() {
        let json = serde_json::to_string(&OrbType::Epic).unwrap();
        assert_eq!(json, "\"epic\"");

        let custom = OrbType::Custom("research".into());
        let json = serde_json::to_string(&custom).unwrap();
        let parsed: OrbType = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, custom);
    }

    #[test]
    fn difficulty_serde() {
        let json = serde_json::to_string(&Difficulty::Hard).unwrap();
        assert_eq!(json, "\"hard\"");
        let parsed: Difficulty = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, Difficulty::Hard);
    }

    #[test]
    fn execution_meta_serde() {
        let meta = ExecutionMeta {
            worker_model: Some("claude-3".into()),
            prompt_tokens: Some(100),
            completion_tokens: Some(50),
            total_tokens: Some(150),
            retries: 2,
            ..Default::default()
        };
        let json = serde_json::to_string(&meta).unwrap();
        let parsed: ExecutionMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.prompt_tokens, Some(100));
        assert_eq!(parsed.retries, 2);
    }
}

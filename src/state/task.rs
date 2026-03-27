use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Status of a task in the orchestrator pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Waiting to be picked up by a worker.
    Pending,
    /// Currently being executed by a worker.
    Active,
    /// Completed, awaiting human review.
    Review,
    /// Successfully completed.
    Done,
    /// Failed during execution.
    Failed,
    /// Cancelled before completion.
    Cancelled,
}

/// A task managed by the orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: Uuid,
    pub title: String,
    pub description: String,
    pub status: TaskStatus,
    /// Priority 1 (highest) to 5 (lowest).
    pub priority: u8,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Final response text from the worker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// Parent task ID for subtask relationships.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<Uuid>,
    /// Model used by the worker for this task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_model: Option<String>,
}

impl Task {
    /// Creates a new pending task with the given title and description.
    pub fn new(title: impl Into<String>, description: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            title: title.into(),
            description: description.into(),
            status: TaskStatus::Pending,
            priority: 3,
            created_at: now,
            updated_at: now,
            result: None,
            parent_id: None,
            worker_model: None,
        }
    }

    /// Sets the priority (clamped to 1-5).
    #[must_use]
    pub fn with_priority(mut self, priority: u8) -> Self {
        self.priority = priority.clamp(1, 5);
        self
    }

    /// Sets the parent task ID.
    #[must_use]
    pub fn with_parent(mut self, parent_id: Uuid) -> Self {
        self.parent_id = Some(parent_id);
        self
    }

    /// Transitions the task to a new status, updating the timestamp.
    pub fn transition(&mut self, new_status: TaskStatus) {
        self.status = new_status;
        self.updated_at = Utc::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_task_is_pending() {
        let task = Task::new("Test task", "Do something");
        assert_eq!(task.status, TaskStatus::Pending);
        assert_eq!(task.priority, 3);
        assert!(task.result.is_none());
        assert!(task.parent_id.is_none());
    }

    #[test]
    fn priority_clamped() {
        let task = Task::new("Test", "test").with_priority(0);
        assert_eq!(task.priority, 1);
        let task = Task::new("Test", "test").with_priority(10);
        assert_eq!(task.priority, 5);
    }

    #[test]
    fn transition_updates_status_and_timestamp() {
        let mut task = Task::new("Test", "test");
        let original_updated = task.updated_at;
        // Small sleep to ensure timestamp changes
        std::thread::sleep(std::time::Duration::from_millis(10));
        task.transition(TaskStatus::Active);
        assert_eq!(task.status, TaskStatus::Active);
        assert!(task.updated_at >= original_updated);
    }

    #[test]
    fn round_trip_serialize() {
        let task = Task::new("Review auth", "Check error handling in auth module").with_priority(2);
        let json = serde_json::to_string(&task).unwrap();
        let parsed: Task = serde_json::from_str(&json).unwrap();
        assert_eq!(task.id, parsed.id);
        assert_eq!(task.title, parsed.title);
        assert_eq!(task.status, parsed.status);
        assert_eq!(task.priority, parsed.priority);
    }

    #[test]
    fn status_serializes_as_snake_case() {
        let json = serde_json::to_string(&TaskStatus::Pending).unwrap();
        assert_eq!(json, "\"pending\"");
        let json = serde_json::to_string(&TaskStatus::Active).unwrap();
        assert_eq!(json, "\"active\"");
        let json = serde_json::to_string(&TaskStatus::Review).unwrap();
        assert_eq!(json, "\"review\"");
        let json = serde_json::to_string(&TaskStatus::Cancelled).unwrap();
        assert_eq!(json, "\"cancelled\"");
    }

    #[test]
    fn subtask_with_parent() {
        let parent = Task::new("Parent", "parent task");
        let child = Task::new("Child", "child task").with_parent(parent.id);
        assert_eq!(child.parent_id, Some(parent.id));
    }
}

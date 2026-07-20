//! Identifiers used as `tracing` span fields to correlate log events
//! across the worker → orchestrator → queue boundary.
//!
//! The newtypes don't enforce anything beyond being distinct compile-time
//! types — they're carried as `%`-formatted fields on instrumented spans
//! so a filter like `RUST_LOG=orboros=debug` plus a tracing subscriber
//! with field rendering produces correlated output.
//!
//! Example:
//! ```ignore
//! use orboros::tracing_ctx::{WorkerId, PipelineRunId};
//!
//! #[tracing::instrument(skip(self), fields(worker_id = %worker_id))]
//! async fn dispatch(&self, worker_id: WorkerId) { /* ... */ }
//! ```

use std::fmt;

use uuid::Uuid;

/// Identifies one orboros pipeline run — a single invocation of
/// `orchestrate`, `plan`, `queue_loop::tick`, etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PipelineRunId(Uuid);

impl PipelineRunId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    #[must_use]
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl Default for PipelineRunId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for PipelineRunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "run-{}", self.short())
    }
}

impl PipelineRunId {
    fn short(&self) -> String {
        self.0
            .simple()
            .to_string()
            .chars()
            .take(8)
            .collect::<String>()
    }
}

/// Identifies one spawned worker subprocess. Held alongside `Worker` and
/// emitted on every span that touches the worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkerId(Uuid);

impl WorkerId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    #[must_use]
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl Default for WorkerId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for WorkerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "worker-{}",
            self.0
                .simple()
                .to_string()
                .chars()
                .take(8)
                .collect::<String>()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_run_id_is_unique() {
        let a = PipelineRunId::new();
        let b = PipelineRunId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn pipeline_run_id_display_prefix() {
        let id = PipelineRunId::new();
        let s = format!("{id}");
        assert!(s.starts_with("run-"));
        assert_eq!(s.len(), "run-".len() + 8);
    }

    #[test]
    fn worker_id_display_prefix() {
        let id = WorkerId::new();
        let s = format!("{id}");
        assert!(s.starts_with("worker-"));
        assert_eq!(s.len(), "worker-".len() + 8);
    }

    #[test]
    fn worker_id_is_unique() {
        let a = WorkerId::new();
        let b = WorkerId::new();
        assert_ne!(a, b);
    }
}

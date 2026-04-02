use std::io::Write;
use std::process::Command;

use crate::config::NotificationConfig;

// ---------------------------------------------------------------------------
// Notification types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationType {
    PipelineComplete,
    ReviewNeeded,
    TaskFailed,
    BudgetExceeded,
    DaemonStarted,
}

impl NotificationType {
    /// Default title prefix for each notification type.
    pub fn prefix(&self) -> &'static str {
        match self {
            Self::PipelineComplete => "Pipeline Complete:",
            Self::ReviewNeeded => "Review Needed:",
            Self::TaskFailed => "Task Failed:",
            Self::BudgetExceeded => "Budget Exceeded:",
            Self::DaemonStarted => "Daemon Started:",
        }
    }
}

// ---------------------------------------------------------------------------
// NotifyConfig — runtime config derived from OrbConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyConfig {
    pub enabled: bool,
    pub desktop: bool,
    pub sound: bool,
}

impl NotifyConfig {
    /// Build a `NotifyConfig` from the persisted `NotificationConfig`.
    /// `sound` defaults to the same value as `enabled`.
    pub fn from_notification_config(cfg: &NotificationConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            desktop: cfg.desktop_enabled,
            sound: cfg.enabled,
        }
    }
}

impl Default for NotifyConfig {
    fn default() -> Self {
        let nc = NotificationConfig::default();
        Self::from_notification_config(&nc)
    }
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// Format a human-readable notification string.
pub fn format_notification(ntype: &NotificationType, title: &str, message: &str) -> String {
    format!("[{}] {} — {}", ntype.prefix(), title, message)
}

// ---------------------------------------------------------------------------
// Sending
// ---------------------------------------------------------------------------

/// Send a notification according to the provided config.
///
/// - When `config.enabled`, writes a terminal bell (`\x07`) to stderr.
/// - When `config.desktop`, invokes `osascript` to show a macOS notification.
///
/// Errors from the desktop notification command are silently ignored (best-effort).
pub fn notify(ntype: &NotificationType, title: &str, message: &str, config: &NotifyConfig) {
    if !config.enabled {
        return;
    }

    // Terminal bell
    let _ = std::io::stderr().write_all(b"\x07");
    let _ = std::io::stderr().flush();

    // macOS desktop notification
    if config.desktop {
        let full_title = format!("{} {}", ntype.prefix(), title);
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            message.replace('\"', "\\\""),
            full_title.replace('\"', "\\\""),
        );
        let _ = Command::new("osascript").arg("-e").arg(&script).output();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- NotificationType prefix ---

    #[test]
    fn pipeline_complete_prefix() {
        assert_eq!(
            NotificationType::PipelineComplete.prefix(),
            "Pipeline Complete:"
        );
    }

    #[test]
    fn review_needed_prefix() {
        assert_eq!(NotificationType::ReviewNeeded.prefix(), "Review Needed:");
    }

    #[test]
    fn task_failed_prefix() {
        assert_eq!(NotificationType::TaskFailed.prefix(), "Task Failed:");
    }

    #[test]
    fn budget_exceeded_prefix() {
        assert_eq!(
            NotificationType::BudgetExceeded.prefix(),
            "Budget Exceeded:"
        );
    }

    #[test]
    fn daemon_started_prefix() {
        assert_eq!(NotificationType::DaemonStarted.prefix(), "Daemon Started:");
    }

    // --- NotifyConfig defaults ---

    #[test]
    fn default_config_enabled_no_desktop() {
        let cfg = NotifyConfig::default();
        assert!(cfg.enabled);
        assert!(!cfg.desktop);
        assert!(cfg.sound);
    }

    #[test]
    fn from_notification_config_enabled() {
        let nc = NotificationConfig {
            enabled: true,
            desktop_enabled: true,
        };
        let cfg = NotifyConfig::from_notification_config(&nc);
        assert!(cfg.enabled);
        assert!(cfg.desktop);
        assert!(cfg.sound);
    }

    #[test]
    fn from_notification_config_disabled() {
        let nc = NotificationConfig {
            enabled: false,
            desktop_enabled: false,
        };
        let cfg = NotifyConfig::from_notification_config(&nc);
        assert!(!cfg.enabled);
        assert!(!cfg.desktop);
        assert!(!cfg.sound);
    }

    // --- format_notification for all 5 types ---

    #[test]
    fn format_pipeline_complete() {
        let s = format_notification(
            &NotificationType::PipelineComplete,
            "build-42",
            "All tasks finished",
        );
        assert_eq!(s, "[Pipeline Complete:] build-42 — All tasks finished");
    }

    #[test]
    fn format_review_needed() {
        let s = format_notification(
            &NotificationType::ReviewNeeded,
            "PR #7",
            "Awaiting approval",
        );
        assert_eq!(s, "[Review Needed:] PR #7 — Awaiting approval");
    }

    #[test]
    fn format_task_failed() {
        let s = format_notification(&NotificationType::TaskFailed, "lint", "Exit code 1");
        assert_eq!(s, "[Task Failed:] lint — Exit code 1");
    }

    #[test]
    fn format_budget_exceeded() {
        let s = format_notification(
            &NotificationType::BudgetExceeded,
            "project-x",
            "$50 over limit",
        );
        assert_eq!(s, "[Budget Exceeded:] project-x — $50 over limit");
    }

    #[test]
    fn format_daemon_started() {
        let s = format_notification(
            &NotificationType::DaemonStarted,
            "orboros",
            "Listening on port 9090",
        );
        assert_eq!(s, "[Daemon Started:] orboros — Listening on port 9090");
    }

    // --- notification string content ---

    #[test]
    fn format_includes_prefix_title_and_message() {
        let s = format_notification(&NotificationType::TaskFailed, "my-task", "crashed");
        assert!(s.contains("Task Failed:"));
        assert!(s.contains("my-task"));
        assert!(s.contains("crashed"));
    }

    #[test]
    fn format_empty_title_and_message() {
        let s = format_notification(&NotificationType::DaemonStarted, "", "");
        assert_eq!(s, "[Daemon Started:]  — ");
    }
}

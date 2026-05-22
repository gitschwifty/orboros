//! Second-opinion reviewer types.
//!
//! Models the output of a reviewer worker (task 58). The reviewer takes a
//! completed orb and its result and emits a [`ReviewReport`] containing a
//! [`ReviewVerdict`]. On `Revise`, the verdict's [`ReviseScope`] tells the
//! pipeline whether to re-execute the same plan (`Execution`) or re-plan
//! the orb (`Decomposition`).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::id::OrbId;

/// Where the orb should re-enter the pipeline on a `Revise` verdict.
///
/// `Execution` means the plan was sound but something in the run
/// itself went wrong (wrong tool call, sloppy reasoning, etc.) — the
/// orb returns to `Active` and re-executes with the critique in
/// context. `Decomposition` means the plan itself was wrong — the orb
/// returns to `Refining` to re-plan with the critique attached.
///
/// Default to `Execution` when the reviewer's intent is ambiguous;
/// re-executing once is cheaper than re-planning unnecessarily.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviseScope {
    Execution,
    Decomposition,
}

impl Default for ReviseScope {
    fn default() -> Self {
        Self::Execution
    }
}

/// The reviewer's verdict on a completed orb.
///
/// Externally tagged so the JSON is unambiguous when nested inside a
/// `ReviewReport.verdict` field:
///   - `Accept` → `"accept"`
///   - `Reject` → `"reject"`
///   - `Revise { scope: Execution }` → `{"revise": {"scope": "execution"}}`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    /// Result meets the bar; orb stays `Done`.
    Accept,
    /// Result is unrecoverable from this attempt; orb transitions to
    /// `Failed`. Use when re-running won't help (e.g., misunderstood
    /// requirement, hallucinated result, fundamental wrong path).
    Reject,
    /// Result is salvageable; orb re-enters the pipeline at the
    /// scope indicated.
    Revise {
        #[serde(default)]
        scope: ReviseScope,
    },
}

impl ReviewVerdict {
    /// Convenience: did the reviewer accept the result as-is?
    #[must_use]
    pub fn is_accept(&self) -> bool {
        matches!(self, ReviewVerdict::Accept)
    }

    /// Convenience: did the reviewer reject the result terminally?
    #[must_use]
    pub fn is_reject(&self) -> bool {
        matches!(self, ReviewVerdict::Reject)
    }

    /// Convenience: did the reviewer ask for a revise (any scope)?
    #[must_use]
    pub fn is_revise(&self) -> bool {
        matches!(self, ReviewVerdict::Revise { .. })
    }
}

/// The full reviewer report stored on an orb.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewReport {
    /// The verdict (including revise scope when applicable).
    pub verdict: ReviewVerdict,
    /// Reviewer's free-form critique of the result.
    pub critique: String,
    /// Optional concrete suggestions for what to change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_changes: Option<String>,
    /// Model identifier used by the reviewer worker.
    pub reviewer_model: String,
    /// When the review completed.
    pub reviewed_at: DateTime<Utc>,
    /// Orb id of the reviewer's own worker invocation, when the
    /// reviewer ran as its own orb (otherwise None).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_orb_id: Option<OrbId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_round_trips_through_serde() {
        let report = ReviewReport {
            verdict: ReviewVerdict::Accept,
            critique: "Looks correct.".into(),
            suggested_changes: None,
            reviewer_model: "anthropic/claude-sonnet-4-6".into(),
            reviewed_at: Utc::now(),
            reviewer_orb_id: None,
        };
        let json = serde_json::to_string(&report).unwrap();
        // External tag: Accept inside the `verdict` field is "accept".
        assert!(json.contains("\"verdict\":\"accept\""));
        let parsed: ReviewReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, report);
    }

    #[test]
    fn reject_round_trips_through_serde() {
        let report = ReviewReport {
            verdict: ReviewVerdict::Reject,
            critique: "Off-topic.".into(),
            suggested_changes: None,
            reviewer_model: "m".into(),
            reviewed_at: Utc::now(),
            reviewer_orb_id: None,
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"verdict\":\"reject\""));
        let parsed: ReviewReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, report);
    }

    #[test]
    fn revise_round_trips_with_scope() {
        let report = ReviewReport {
            verdict: ReviewVerdict::Revise {
                scope: ReviseScope::Decomposition,
            },
            critique: "Plan missed step X.".into(),
            suggested_changes: Some("Add step X before Y.".into()),
            reviewer_model: "m".into(),
            reviewed_at: Utc::now(),
            reviewer_orb_id: None,
        };
        let json = serde_json::to_string(&report).unwrap();
        // External tag: Revise becomes `{"revise":{"scope":"..."}}`
        // wrapped inside the parent's verdict field.
        assert!(json.contains("\"verdict\":{\"revise\":{"));
        assert!(json.contains("\"scope\":\"decomposition\""));
        let parsed: ReviewReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, report);
    }

    #[test]
    fn revise_defaults_scope_to_execution_when_absent() {
        // Reviewer's prompt template suggests `{"revise": {}}` for the
        // "no opinion on scope" case — default kicks in.
        let json = r#"{
            "verdict": {"revise": {}},
            "critique": "Bad output but plan was fine.",
            "reviewer_model": "m",
            "reviewed_at": "2026-05-22T00:00:00Z"
        }"#;
        let parsed: ReviewReport = serde_json::from_str(json).unwrap();
        assert_eq!(
            parsed.verdict,
            ReviewVerdict::Revise {
                scope: ReviseScope::Execution,
            }
        );
    }

    #[test]
    fn verdict_classifiers() {
        assert!(ReviewVerdict::Accept.is_accept());
        assert!(!ReviewVerdict::Accept.is_revise());
        assert!(ReviewVerdict::Reject.is_reject());
        assert!(!ReviewVerdict::Reject.is_accept());
        assert!(ReviewVerdict::Revise {
            scope: ReviseScope::Execution
        }
        .is_revise());
        assert!(!ReviewVerdict::Revise {
            scope: ReviseScope::Execution
        }
        .is_accept());
    }

    #[test]
    fn revise_scope_default_is_execution() {
        assert_eq!(ReviseScope::default(), ReviseScope::Execution);
    }
}

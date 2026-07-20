//! Hook event taxonomy. Mirrors the table in
//! `private/design/lifecycle-hooks.md` §2.

use std::fmt;
use std::str::FromStr;

use orbs::orb::OrbPhase;
use serde::{Deserialize, Serialize};

/// Lifecycle events that can fire hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HookEvent {
    OnOrbCreate,
    OnSubmit,
    PreWorkerSpawn,
    PostWorkerSpawn,
    OnReviewNeeded,
    PostWorkerComplete,
    PostWorkerFail,
    OnReviewApprove,
    OnReviewRevise,
    OnReviewReject,
    /// Automated second-opinion reviewer (task 58) produced a verdict.
    /// Distinct from the HITL `OnReviewApprove`/`Reject`/`Revise`
    /// events which fire on human decisions.
    OnReviewSecondOpinion,
    OnCancel,
    OnDelete,
    OnUndefer,
    /// `pre-phase-transition(<phase>)`. Per-phase variant.
    #[serde(rename = "pre-phase-transition")]
    PrePhaseTransition(OrbPhase),
    /// `post-phase-transition(<phase>)`.
    #[serde(rename = "post-phase-transition")]
    PostPhaseTransition(OrbPhase),
    OnDepChanged,
    OnDepResolved,
    OnEscalate,
    OnPipelineStart,
    OnPipelineEnd,
    OnQueueTick,
}

impl HookEvent {
    /// True for events that should default to `sync = true` (gating
    /// hooks that the caller waits on).
    #[must_use]
    pub fn is_pre_event(self) -> bool {
        matches!(
            self,
            HookEvent::PreWorkerSpawn | HookEvent::PrePhaseTransition(_)
        )
    }

    /// Default `timeout_ms` for this event class.
    #[must_use]
    pub fn default_timeout_ms(self) -> u64 {
        if self.is_pre_event() {
            5_000
        } else {
            30_000
        }
    }
}

impl fmt::Display for HookEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HookEvent::OnOrbCreate => f.write_str("on-orb-create"),
            HookEvent::OnSubmit => f.write_str("on-submit"),
            HookEvent::PreWorkerSpawn => f.write_str("pre-worker-spawn"),
            HookEvent::PostWorkerSpawn => f.write_str("post-worker-spawn"),
            HookEvent::OnReviewNeeded => f.write_str("on-review-needed"),
            HookEvent::PostWorkerComplete => f.write_str("post-worker-complete"),
            HookEvent::PostWorkerFail => f.write_str("post-worker-fail"),
            HookEvent::OnReviewApprove => f.write_str("on-review-approve"),
            HookEvent::OnReviewRevise => f.write_str("on-review-revise"),
            HookEvent::OnReviewReject => f.write_str("on-review-reject"),
            HookEvent::OnReviewSecondOpinion => f.write_str("on-review-second-opinion"),
            HookEvent::OnCancel => f.write_str("on-cancel"),
            HookEvent::OnDelete => f.write_str("on-delete"),
            HookEvent::OnUndefer => f.write_str("on-undefer"),
            HookEvent::PrePhaseTransition(p) => {
                write!(f, "pre-phase-transition({})", phase_token(*p))
            }
            HookEvent::PostPhaseTransition(p) => {
                write!(f, "post-phase-transition({})", phase_token(*p))
            }
            HookEvent::OnDepChanged => f.write_str("on-dep-changed"),
            HookEvent::OnDepResolved => f.write_str("on-dep-resolved"),
            HookEvent::OnEscalate => f.write_str("on-escalate"),
            HookEvent::OnPipelineStart => f.write_str("on-pipeline-start"),
            HookEvent::OnPipelineEnd => f.write_str("on-pipeline-end"),
            HookEvent::OnQueueTick => f.write_str("on-queue-tick"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HookEventParseError {
    #[error("unknown hook event: {0}")]
    Unknown(String),
    #[error("expected '(phase)' suffix for {event}, got: {got}")]
    MissingPhase { event: &'static str, got: String },
    #[error("unknown phase '{0}' for phase-transition event")]
    UnknownPhase(String),
}

impl FromStr for HookEvent {
    type Err = HookEventParseError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let s = raw.trim();
        if let Some(rest) = s.strip_prefix("pre-phase-transition") {
            return parse_phase_event(rest, "pre-phase-transition")
                .map(HookEvent::PrePhaseTransition);
        }
        if let Some(rest) = s.strip_prefix("post-phase-transition") {
            return parse_phase_event(rest, "post-phase-transition")
                .map(HookEvent::PostPhaseTransition);
        }
        Ok(match s {
            "on-orb-create" => HookEvent::OnOrbCreate,
            "on-submit" => HookEvent::OnSubmit,
            "pre-worker-spawn" => HookEvent::PreWorkerSpawn,
            "post-worker-spawn" => HookEvent::PostWorkerSpawn,
            "on-review-needed" => HookEvent::OnReviewNeeded,
            "post-worker-complete" => HookEvent::PostWorkerComplete,
            "post-worker-fail" => HookEvent::PostWorkerFail,
            "on-review-approve" => HookEvent::OnReviewApprove,
            "on-review-revise" => HookEvent::OnReviewRevise,
            "on-review-reject" => HookEvent::OnReviewReject,
            "on-review-second-opinion" => HookEvent::OnReviewSecondOpinion,
            "on-cancel" => HookEvent::OnCancel,
            "on-delete" => HookEvent::OnDelete,
            "on-undefer" => HookEvent::OnUndefer,
            "on-dep-changed" => HookEvent::OnDepChanged,
            "on-dep-resolved" => HookEvent::OnDepResolved,
            "on-escalate" => HookEvent::OnEscalate,
            "on-pipeline-start" => HookEvent::OnPipelineStart,
            "on-pipeline-end" => HookEvent::OnPipelineEnd,
            "on-queue-tick" => HookEvent::OnQueueTick,
            other => return Err(HookEventParseError::Unknown(other.to_string())),
        })
    }
}

fn parse_phase_event(rest: &str, event: &'static str) -> Result<OrbPhase, HookEventParseError> {
    let trimmed = rest.trim();
    let inner = trimmed
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .map(str::trim)
        .ok_or_else(|| HookEventParseError::MissingPhase {
            event,
            got: rest.to_string(),
        })?;
    parse_phase(inner)
}

fn parse_phase(s: &str) -> Result<OrbPhase, HookEventParseError> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "draft" => OrbPhase::Draft,
        "pending" => OrbPhase::Pending,
        "speccing" => OrbPhase::Speccing,
        "decomposing" => OrbPhase::Decomposing,
        "refining" => OrbPhase::Refining,
        "review" => OrbPhase::Review,
        "waiting" => OrbPhase::Waiting,
        "executing" => OrbPhase::Executing,
        "reevaluating" => OrbPhase::Reevaluating,
        "done" => OrbPhase::Done,
        "failed" => OrbPhase::Failed,
        "cancelled" => OrbPhase::Cancelled,
        "deferred" => OrbPhase::Deferred,
        "tombstone" => OrbPhase::Tombstone,
        other => return Err(HookEventParseError::UnknownPhase(other.to_string())),
    })
}

fn phase_token(phase: OrbPhase) -> &'static str {
    match phase {
        OrbPhase::Draft => "draft",
        OrbPhase::Pending => "pending",
        OrbPhase::Speccing => "speccing",
        OrbPhase::Decomposing => "decomposing",
        OrbPhase::Refining => "refining",
        OrbPhase::Review => "review",
        OrbPhase::Waiting => "waiting",
        OrbPhase::Executing => "executing",
        OrbPhase::Reevaluating => "reevaluating",
        OrbPhase::Done => "done",
        OrbPhase::Failed => "failed",
        OrbPhase::Cancelled => "cancelled",
        OrbPhase::Deferred => "deferred",
        OrbPhase::Tombstone => "tombstone",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_events() {
        assert_eq!(
            "on-orb-create".parse::<HookEvent>().unwrap(),
            HookEvent::OnOrbCreate
        );
        assert_eq!(
            "pre-worker-spawn".parse::<HookEvent>().unwrap(),
            HookEvent::PreWorkerSpawn
        );
        assert_eq!(
            "post-worker-complete".parse::<HookEvent>().unwrap(),
            HookEvent::PostWorkerComplete
        );
        assert_eq!(
            "on-queue-tick".parse::<HookEvent>().unwrap(),
            HookEvent::OnQueueTick
        );
    }

    #[test]
    fn parse_phase_events() {
        assert_eq!(
            "pre-phase-transition(refining)"
                .parse::<HookEvent>()
                .unwrap(),
            HookEvent::PrePhaseTransition(OrbPhase::Refining)
        );
        assert_eq!(
            "post-phase-transition(done)".parse::<HookEvent>().unwrap(),
            HookEvent::PostPhaseTransition(OrbPhase::Done)
        );
    }

    #[test]
    fn parse_phase_event_is_case_insensitive_on_phase_name() {
        assert_eq!(
            "pre-phase-transition(REFINING)"
                .parse::<HookEvent>()
                .unwrap(),
            HookEvent::PrePhaseTransition(OrbPhase::Refining)
        );
    }

    #[test]
    fn parse_phase_event_missing_phase_errors() {
        let err = "pre-phase-transition".parse::<HookEvent>().unwrap_err();
        assert!(matches!(err, HookEventParseError::MissingPhase { .. }));
    }

    #[test]
    fn parse_phase_event_unknown_phase_errors() {
        let err = "pre-phase-transition(nope)"
            .parse::<HookEvent>()
            .unwrap_err();
        assert!(matches!(err, HookEventParseError::UnknownPhase(_)));
    }

    #[test]
    fn parse_on_review_second_opinion() {
        let ev = "on-review-second-opinion".parse::<HookEvent>().unwrap();
        assert_eq!(ev, HookEvent::OnReviewSecondOpinion);
        // Round-trips through Display.
        assert_eq!(ev.to_string(), "on-review-second-opinion");
    }

    #[test]
    fn parse_unknown_event_errors() {
        let err = "made-up-event".parse::<HookEvent>().unwrap_err();
        assert!(matches!(err, HookEventParseError::Unknown(_)));
    }

    #[test]
    fn display_round_trips_for_simple_events() {
        for ev in [
            HookEvent::OnOrbCreate,
            HookEvent::PreWorkerSpawn,
            HookEvent::PostWorkerComplete,
            HookEvent::OnPipelineStart,
        ] {
            let s = ev.to_string();
            assert_eq!(s.parse::<HookEvent>().unwrap(), ev);
        }
    }

    #[test]
    fn display_round_trips_for_phase_events() {
        let ev = HookEvent::PrePhaseTransition(OrbPhase::Refining);
        assert_eq!(ev.to_string(), "pre-phase-transition(refining)");
        assert_eq!(ev.to_string().parse::<HookEvent>().unwrap(), ev);
    }

    #[test]
    fn is_pre_event_classification() {
        assert!(HookEvent::PreWorkerSpawn.is_pre_event());
        assert!(HookEvent::PrePhaseTransition(OrbPhase::Refining).is_pre_event());
        assert!(!HookEvent::PostWorkerComplete.is_pre_event());
        assert!(!HookEvent::OnOrbCreate.is_pre_event());
        assert!(!HookEvent::OnQueueTick.is_pre_event());
    }

    #[test]
    fn default_timeout_pre_vs_other() {
        assert_eq!(HookEvent::PreWorkerSpawn.default_timeout_ms(), 5_000);
        assert_eq!(HookEvent::PostWorkerComplete.default_timeout_ms(), 30_000);
    }
}

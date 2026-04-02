use orbs::orb::{Orb, OrbPhase};

use crate::config::OrbConfig;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The type of review checkpoint in the phase lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointType {
    /// After refinement, before execution (Refining -> Review -> Waiting).
    PostRefinement,
    /// After execution, before marking done (Executing -> Review -> Done).
    PostCompletion,
}

/// A human decision on a review checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewDecision {
    /// Advance to the next phase.
    Approve,
    /// Mark as failed.
    Reject { reason: String },
    /// Send back for rework.
    RequestChanges { feedback: String },
}

/// A pending review checkpoint for an orb.
#[derive(Debug, Clone)]
pub struct ReviewCheckpoint {
    pub orb_id: orbs::id::OrbId,
    pub checkpoint_type: CheckpointType,
    pub required: bool,
}

// ---------------------------------------------------------------------------
// Logic
// ---------------------------------------------------------------------------

/// Returns true if the orb requires human review, based on the orb's own
/// `requires_approval` flag and the config-level default.
pub fn needs_review(orb: &Orb, config: &OrbConfig) -> bool {
    orb.requires_approval || config.review.requires_approval_by_default
}

/// Transitions an orb into the Review phase.
///
/// Sets `orb.phase` to `Review`. The `checkpoint_type` is informational
/// for the caller but does not affect the orb state directly (the orb
/// only knows it is in Review).
pub fn enter_review(orb: &mut Orb, _checkpoint_type: CheckpointType) {
    orb.set_phase(OrbPhase::Review);
}

/// Applies a review decision, transitioning the orb to the appropriate phase.
///
/// - `Approve`:
///   - `PostRefinement` -> Waiting
///   - `PostCompletion` -> Done
/// - `Reject` -> Failed
/// - `RequestChanges`:
///   - `PostRefinement` -> Refining
///   - `PostCompletion` -> Executing
pub fn apply_decision(orb: &mut Orb, decision: &ReviewDecision, checkpoint_type: CheckpointType) {
    match decision {
        ReviewDecision::Approve => match checkpoint_type {
            CheckpointType::PostRefinement => orb.set_phase(OrbPhase::Waiting),
            CheckpointType::PostCompletion => orb.set_phase(OrbPhase::Done),
        },
        ReviewDecision::Reject { .. } => {
            orb.set_phase(OrbPhase::Failed);
        }
        ReviewDecision::RequestChanges { .. } => match checkpoint_type {
            CheckpointType::PostRefinement => orb.set_phase(OrbPhase::Refining),
            CheckpointType::PostCompletion => orb.set_phase(OrbPhase::Executing),
        },
    }
}

/// Placeholder confidence check. Returns true if the orb has both a `result`
/// and `acceptance_criteria` set. Will be replaced by LLM-based scoring later.
pub fn confidence_check(orb: &Orb) -> bool {
    orb.result.is_some() && orb.acceptance_criteria.is_some()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use orbs::orb::OrbType;

    use super::*;

    fn feature_orb(title: &str) -> Orb {
        Orb::new(title, "test description").with_type(OrbType::Feature)
    }

    fn default_config() -> OrbConfig {
        OrbConfig::default()
    }

    fn approval_config() -> OrbConfig {
        OrbConfig {
            review: crate::config::ReviewConfig {
                requires_approval_by_default: true,
                review_on_completion: true,
            },
            ..Default::default()
        }
    }

    // ── needs_review ─────────────────────────────────────────

    #[test]
    fn needs_review_false_by_default() {
        let orb = feature_orb("Auth");
        let config = default_config();
        assert!(!needs_review(&orb, &config));
    }

    #[test]
    fn needs_review_true_when_orb_requires_approval() {
        let mut orb = feature_orb("Auth");
        orb.requires_approval = true;
        let config = default_config();
        assert!(needs_review(&orb, &config));
    }

    #[test]
    fn needs_review_true_when_config_requires_approval() {
        let orb = feature_orb("Auth");
        let config = approval_config();
        assert!(needs_review(&orb, &config));
    }

    #[test]
    fn needs_review_true_when_both_require() {
        let mut orb = feature_orb("Auth");
        orb.requires_approval = true;
        let config = approval_config();
        assert!(needs_review(&orb, &config));
    }

    // ── enter_review ─────────────────────────────────────────

    #[test]
    fn enter_review_sets_phase_to_review() {
        let mut orb = feature_orb("Auth");
        orb.set_phase(OrbPhase::Refining);

        enter_review(&mut orb, CheckpointType::PostRefinement);
        assert_eq!(orb.phase, Some(OrbPhase::Review));
    }

    #[test]
    fn enter_review_post_completion() {
        let mut orb = feature_orb("Auth");
        orb.set_phase(OrbPhase::Executing);

        enter_review(&mut orb, CheckpointType::PostCompletion);
        assert_eq!(orb.phase, Some(OrbPhase::Review));
    }

    // ── apply_decision: approve ──────────────────────────────

    #[test]
    fn approve_post_refinement_transitions_to_waiting() {
        let mut orb = feature_orb("Auth");
        orb.set_phase(OrbPhase::Review);

        apply_decision(
            &mut orb,
            &ReviewDecision::Approve,
            CheckpointType::PostRefinement,
        );
        assert_eq!(orb.phase, Some(OrbPhase::Waiting));
    }

    #[test]
    fn approve_post_completion_transitions_to_done() {
        let mut orb = feature_orb("Auth");
        orb.set_phase(OrbPhase::Review);

        apply_decision(
            &mut orb,
            &ReviewDecision::Approve,
            CheckpointType::PostCompletion,
        );
        assert_eq!(orb.phase, Some(OrbPhase::Done));
    }

    // ── apply_decision: reject ───────────────────────────────

    #[test]
    fn reject_post_refinement_transitions_to_failed() {
        let mut orb = feature_orb("Auth");
        orb.set_phase(OrbPhase::Review);

        let decision = ReviewDecision::Reject {
            reason: "design flawed".into(),
        };
        apply_decision(&mut orb, &decision, CheckpointType::PostRefinement);
        assert_eq!(orb.phase, Some(OrbPhase::Failed));
    }

    #[test]
    fn reject_post_completion_transitions_to_failed() {
        let mut orb = feature_orb("Auth");
        orb.set_phase(OrbPhase::Review);

        let decision = ReviewDecision::Reject {
            reason: "output unusable".into(),
        };
        apply_decision(&mut orb, &decision, CheckpointType::PostCompletion);
        assert_eq!(orb.phase, Some(OrbPhase::Failed));
    }

    // ── apply_decision: request changes ──────────────────────

    #[test]
    fn request_changes_post_refinement_returns_to_refining() {
        let mut orb = feature_orb("Auth");
        orb.set_phase(OrbPhase::Review);

        let decision = ReviewDecision::RequestChanges {
            feedback: "needs more detail on error handling".into(),
        };
        apply_decision(&mut orb, &decision, CheckpointType::PostRefinement);
        assert_eq!(orb.phase, Some(OrbPhase::Refining));
    }

    #[test]
    fn request_changes_post_completion_returns_to_executing() {
        let mut orb = feature_orb("Auth");
        orb.set_phase(OrbPhase::Review);

        let decision = ReviewDecision::RequestChanges {
            feedback: "missing edge case handling".into(),
        };
        apply_decision(&mut orb, &decision, CheckpointType::PostCompletion);
        assert_eq!(orb.phase, Some(OrbPhase::Executing));
    }

    // ── confidence_check ─────────────────────────────────────

    #[test]
    fn confidence_check_false_when_no_result() {
        let mut orb = feature_orb("Auth");
        orb.acceptance_criteria = Some("must handle 401".into());
        assert!(!confidence_check(&orb));
    }

    #[test]
    fn confidence_check_false_when_no_acceptance_criteria() {
        let mut orb = feature_orb("Auth");
        orb.result = Some("implemented auth".into());
        assert!(!confidence_check(&orb));
    }

    #[test]
    fn confidence_check_true_when_both_present() {
        let mut orb = feature_orb("Auth");
        orb.result = Some("implemented auth".into());
        orb.acceptance_criteria = Some("must handle 401".into());
        assert!(confidence_check(&orb));
    }

    #[test]
    fn confidence_check_false_when_neither_present() {
        let orb = feature_orb("Auth");
        assert!(!confidence_check(&orb));
    }

    // ── review checkpoint struct ─────────────────────────────

    #[test]
    fn review_checkpoint_construction() {
        let orb = feature_orb("Auth");
        let checkpoint = ReviewCheckpoint {
            orb_id: orb.id.clone(),
            checkpoint_type: CheckpointType::PostRefinement,
            required: true,
        };
        assert_eq!(checkpoint.checkpoint_type, CheckpointType::PostRefinement);
        assert!(checkpoint.required);
    }

    // ── end-to-end flow ──────────────────────────────────────

    #[test]
    fn full_post_refinement_review_flow() {
        let mut orb = feature_orb("Auth");
        orb.requires_approval = true;
        orb.set_phase(OrbPhase::Refining);

        let config = default_config();
        assert!(needs_review(&orb, &config));

        // Enter review
        enter_review(&mut orb, CheckpointType::PostRefinement);
        assert_eq!(orb.phase, Some(OrbPhase::Review));

        // Request changes -> back to refining
        let changes = ReviewDecision::RequestChanges {
            feedback: "add error handling".into(),
        };
        apply_decision(&mut orb, &changes, CheckpointType::PostRefinement);
        assert_eq!(orb.phase, Some(OrbPhase::Refining));

        // Re-enter review
        enter_review(&mut orb, CheckpointType::PostRefinement);
        assert_eq!(orb.phase, Some(OrbPhase::Review));

        // Approve -> waiting
        apply_decision(
            &mut orb,
            &ReviewDecision::Approve,
            CheckpointType::PostRefinement,
        );
        assert_eq!(orb.phase, Some(OrbPhase::Waiting));
    }

    #[test]
    fn full_post_completion_review_flow() {
        let mut orb = feature_orb("Auth");
        orb.set_phase(OrbPhase::Executing);
        orb.result = Some("auth implemented".into());
        orb.acceptance_criteria = Some("handles 401".into());

        let config = approval_config();
        assert!(needs_review(&orb, &config));
        assert!(confidence_check(&orb));

        // Enter review
        enter_review(&mut orb, CheckpointType::PostCompletion);
        assert_eq!(orb.phase, Some(OrbPhase::Review));

        // Approve -> done
        apply_decision(
            &mut orb,
            &ReviewDecision::Approve,
            CheckpointType::PostCompletion,
        );
        assert_eq!(orb.phase, Some(OrbPhase::Done));
        assert!(orb.closed_at.is_some());
    }
}

//! Trigger decision for the second-opinion reviewer.
//!
//! Pure function over (orb, config, RNG) — decides whether the
//! automated reviewer should run after an orb hits `Done`. Kept
//! separate from the reviewer's worker invocation so the decision
//! is easy to test deterministically.

use orbs::orb::Orb;
use rand::Rng;

use crate::config::{SecondOpinionConfig, SecondOpinionMode};

/// Decides whether to dispatch the second-opinion reviewer for `orb`.
///
/// Caller is expected to feed a seeded RNG in tests so the
/// `Sampling` mode is deterministic. Production code can pass
/// `rand::thread_rng()`.
///
/// Modes:
/// - `Off`: never.
/// - `Always`: always.
/// - `Confidence`: orb has a confidence value strictly less than
///   `cfg.confidence_threshold`. Orbs without a confidence value are
///   *not* reviewed under this mode (no signal to decide on — they
///   should fall through to manual review).
/// - `Sampling`: random fraction equal to `cfg.sampling_rate`.
pub fn should_review<R: Rng + ?Sized>(orb: &Orb, cfg: &SecondOpinionConfig, rng: &mut R) -> bool {
    match cfg.mode {
        SecondOpinionMode::Off => false,
        SecondOpinionMode::Always => true,
        SecondOpinionMode::Confidence => {
            orb.confidence.is_some_and(|c| c < cfg.confidence_threshold)
        }
        SecondOpinionMode::Sampling => {
            if cfg.sampling_rate <= 0.0 {
                return false;
            }
            if cfg.sampling_rate >= 1.0 {
                return true;
            }
            rng.gen::<f32>() < cfg.sampling_rate
        }
    }
}

/// Persists the reviewer's report on the orb without changing its
/// status. Sets `review_report` and, when the verdict is `Revise`,
/// copies the critique into `review_critique` so downstream prompt
/// builders can surface it.
///
/// The actual lifecycle re-entry (Done → Active for re-execution,
/// or Done → Refining for re-decomposition) is deferred to a
/// separate follow-up that relaxes the terminal-state invariant
/// and adds a revision counter to prevent infinite loops.
pub fn apply_review_outcome(orb: &mut Orb, report: orbs::review::ReviewReport) {
    use orbs::review::ReviewVerdict;
    if matches!(report.verdict, ReviewVerdict::Revise { .. }) && !report.critique.is_empty() {
        orb.review_critique = Some(report.critique.clone());
    }
    orb.review_report = Some(report);
}

#[cfg(test)]
mod tests {
    use super::*;
    use orbs::orb::OrbType;
    use orbs::review::{ReviewReport, ReviewVerdict, ReviseScope};
    use rand::{rngs::StdRng, SeedableRng};

    fn done_orb_with_confidence(c: Option<f32>) -> Orb {
        let mut o = Orb::new("t", "d").with_type(OrbType::Task);
        o.confidence = c;
        o
    }

    fn cfg(mode: SecondOpinionMode) -> SecondOpinionConfig {
        SecondOpinionConfig {
            mode,
            confidence_threshold: 0.7,
            sampling_rate: 0.5,
            reviewer_model: None,
        }
    }

    fn rng() -> StdRng {
        StdRng::seed_from_u64(42)
    }

    // ── trigger decision ──────────────────────────────────────

    #[test]
    fn mode_off_never_triggers() {
        let o = done_orb_with_confidence(Some(0.1));
        assert!(!should_review(&o, &cfg(SecondOpinionMode::Off), &mut rng()));
    }

    #[test]
    fn mode_always_always_triggers() {
        let o = done_orb_with_confidence(None);
        assert!(should_review(
            &o,
            &cfg(SecondOpinionMode::Always),
            &mut rng()
        ));
    }

    #[test]
    fn confidence_mode_triggers_when_below_threshold() {
        let o = done_orb_with_confidence(Some(0.5));
        assert!(should_review(
            &o,
            &cfg(SecondOpinionMode::Confidence),
            &mut rng()
        ));
    }

    #[test]
    fn confidence_mode_skips_when_at_or_above_threshold() {
        let o = done_orb_with_confidence(Some(0.7));
        assert!(!should_review(
            &o,
            &cfg(SecondOpinionMode::Confidence),
            &mut rng()
        ));
        let o2 = done_orb_with_confidence(Some(0.9));
        assert!(!should_review(
            &o2,
            &cfg(SecondOpinionMode::Confidence),
            &mut rng()
        ));
    }

    #[test]
    fn confidence_mode_skips_when_no_confidence_recorded() {
        let o = done_orb_with_confidence(None);
        assert!(
            !should_review(&o, &cfg(SecondOpinionMode::Confidence), &mut rng()),
            "no signal means no automated review (falls through to manual)"
        );
    }

    #[test]
    fn sampling_mode_zero_rate_never_triggers() {
        let o = done_orb_with_confidence(None);
        let mut c = cfg(SecondOpinionMode::Sampling);
        c.sampling_rate = 0.0;
        assert!(!should_review(&o, &c, &mut rng()));
    }

    #[test]
    fn sampling_mode_full_rate_always_triggers() {
        let o = done_orb_with_confidence(None);
        let mut c = cfg(SecondOpinionMode::Sampling);
        c.sampling_rate = 1.0;
        assert!(should_review(&o, &c, &mut rng()));
    }

    #[test]
    fn sampling_mode_is_deterministic_with_seeded_rng() {
        let o = done_orb_with_confidence(None);
        let c = cfg(SecondOpinionMode::Sampling);
        // Same seed → same answer over many calls.
        let mut r1 = StdRng::seed_from_u64(7);
        let mut r2 = StdRng::seed_from_u64(7);
        for _ in 0..20 {
            assert_eq!(
                should_review(&o, &c, &mut r1),
                should_review(&o, &c, &mut r2)
            );
        }
    }

    #[test]
    fn sampling_mode_approximates_target_rate_over_many_draws() {
        let o = done_orb_with_confidence(None);
        let mut c = cfg(SecondOpinionMode::Sampling);
        c.sampling_rate = 0.25;
        let mut r = StdRng::seed_from_u64(100);
        let hits = (0..2000).filter(|_| should_review(&o, &c, &mut r)).count();
        let observed = hits as f64 / 2000.0;
        assert!(
            (observed - 0.25).abs() < 0.05,
            "sampling rate ~0.25; observed {observed}",
        );
    }

    // ── apply_review_outcome ──────────────────────────────────

    fn make_report(verdict: ReviewVerdict, critique: &str) -> ReviewReport {
        ReviewReport {
            verdict,
            critique: critique.into(),
            suggested_changes: None,
            reviewer_model: "m".into(),
            reviewed_at: chrono::Utc::now(),
            reviewer_orb_id: None,
        }
    }

    #[test]
    fn apply_accept_sets_report_without_critique_copy() {
        let mut o = done_orb_with_confidence(None);
        apply_review_outcome(&mut o, make_report(ReviewVerdict::Accept, "looks good"));
        assert!(o.review_report.is_some());
        // Critique field is only populated on Revise — Accept shouldn't
        // leak any "guidance" into the orb's prompt context.
        assert!(o.review_critique.is_none());
    }

    #[test]
    fn apply_reject_sets_report_without_critique_copy() {
        let mut o = done_orb_with_confidence(None);
        apply_review_outcome(&mut o, make_report(ReviewVerdict::Reject, "unrecoverable"));
        assert!(o.review_report.is_some());
        assert!(o.review_critique.is_none());
    }

    #[test]
    fn apply_revise_execution_copies_critique() {
        let mut o = done_orb_with_confidence(None);
        apply_review_outcome(
            &mut o,
            make_report(
                ReviewVerdict::Revise {
                    scope: ReviseScope::Execution,
                },
                "wrong tool call sequence",
            ),
        );
        assert!(o.review_report.is_some());
        assert_eq!(
            o.review_critique.as_deref(),
            Some("wrong tool call sequence")
        );
    }

    #[test]
    fn apply_revise_decomposition_copies_critique() {
        let mut o = done_orb_with_confidence(None);
        apply_review_outcome(
            &mut o,
            make_report(
                ReviewVerdict::Revise {
                    scope: ReviseScope::Decomposition,
                },
                "missing migration step",
            ),
        );
        assert_eq!(o.review_critique.as_deref(), Some("missing migration step"));
    }

    #[test]
    fn apply_revise_with_empty_critique_does_not_set_field() {
        let mut o = done_orb_with_confidence(None);
        apply_review_outcome(
            &mut o,
            make_report(
                ReviewVerdict::Revise {
                    scope: ReviseScope::Execution,
                },
                "",
            ),
        );
        assert!(o.review_report.is_some());
        assert!(
            o.review_critique.is_none(),
            "empty critique shouldn't write a misleading hint"
        );
    }

    #[test]
    fn apply_does_not_change_orb_status() {
        let mut o = done_orb_with_confidence(None);
        let prior_status = o.status;
        apply_review_outcome(
            &mut o,
            make_report(
                ReviewVerdict::Revise {
                    scope: ReviseScope::Execution,
                },
                "x",
            ),
        );
        assert_eq!(o.status, prior_status, "lifecycle re-entry is deferred");
    }
}

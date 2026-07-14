//! Trigger decision for the second-opinion reviewer.
//!
//! Pure function over (orb, config, RNG) — decides whether the
//! automated reviewer should run after an orb hits `Done`. Kept
//! separate from the reviewer's worker invocation so the decision
//! is easy to test deterministically.

use orbs::orb::Orb;
use rand::Rng;

use crate::config::{SecondOpinionConfig, SecondOpinionMode};
use crate::hooks::{event::HookEvent, runner::FireCtx, sink::HookSink};

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

/// Persists the reviewer's report on the orb and, for `Revise`
/// verdicts, re-enters the pipeline via `Orb::try_begin_revision`.
/// The critique is copied into `review_critique` so the next
/// worker's prompt context surfaces it.
///
/// Re-entry routing:
/// - `Revise { Execution }` → `Orb::try_begin_revision(Execution)`.
///   Task orbs return to `Active`; phase orbs return to `Executing`.
/// - `Revise { Decomposition }` → `try_begin_revision(Decomposition)`.
///   Task orbs return to `Active` (no Refining phase exists for
///   them); phase orbs return to `Refining`.
///
/// When the orb has already hit `MAX_REVISIONS`, the verdict is
/// still recorded but the orb stays at `Done` and an `on-escalate`
/// hook fires so external systems can surface it for human review.
pub fn apply_review_outcome(
    orb: &mut Orb,
    report: orbs::review::ReviewReport,
    hooks: Option<&HookSink>,
) {
    use orbs::review::ReviewVerdict;
    if matches!(report.verdict, ReviewVerdict::Revise { .. }) && !report.critique.is_empty() {
        orb.review_critique = Some(report.critique.clone());
    }
    let revise_scope = match report.verdict {
        ReviewVerdict::Revise {
            scope: orbs::review::ReviseScope::Execution,
        } => Some(orbs::orb::ReviseScope::Execution),
        ReviewVerdict::Revise {
            scope: orbs::review::ReviseScope::Decomposition,
        } => Some(orbs::orb::ReviseScope::Decomposition),
        _ => None,
    };
    orb.review_report = Some(report);

    // Always fire the verdict notification hook first.
    if let Some(sink) = hooks {
        let _ = sink.fire_blocking(HookEvent::OnReviewSecondOpinion, FireCtx::for_orb(orb));
    }

    // For Revise verdicts, attempt re-entry. Cap-exceeded surfaces
    // via on-escalate so external systems can flag it for humans.
    if let Some(scope) = revise_scope {
        match orb.try_begin_revision(scope) {
            Ok(()) => {
                tracing::info!(
                    orb_id = %orb.id,
                    scope = ?scope,
                    revision_count = orb.revision_count,
                    "reviewer Revise: re-entering pipeline",
                );
            }
            Err(e) => {
                tracing::warn!(
                    orb_id = %orb.id,
                    error = %e,
                    "reviewer Revise: re-entry rejected (revision cap or invalid state)",
                );
                if let Some(sink) = hooks {
                    let _ = sink.fire_blocking(HookEvent::OnEscalate, FireCtx::for_orb(orb));
                }
            }
        }
    }
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
    #[allow(clippy::cast_precision_loss)]
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
        apply_review_outcome(
            &mut o,
            make_report(ReviewVerdict::Accept, "looks good"),
            None,
        );
        assert!(o.review_report.is_some());
        // Critique field is only populated on Revise — Accept shouldn't
        // leak any "guidance" into the orb's prompt context.
        assert!(o.review_critique.is_none());
    }

    #[test]
    fn apply_reject_sets_report_without_critique_copy() {
        let mut o = done_orb_with_confidence(None);
        apply_review_outcome(
            &mut o,
            make_report(ReviewVerdict::Reject, "unrecoverable"),
            None,
        );
        assert!(o.review_report.is_some());
        assert!(o.review_critique.is_none());
    }

    fn done_task_orb() -> Orb {
        let mut o = Orb::new("t", "d").with_type(OrbType::Task);
        o.set_status(orbs::orb::OrbStatus::Active).unwrap();
        o.set_status(orbs::orb::OrbStatus::Done).unwrap();
        o
    }

    #[test]
    fn apply_revise_execution_copies_critique_and_re_enters() {
        let mut o = done_task_orb();
        apply_review_outcome(
            &mut o,
            make_report(
                ReviewVerdict::Revise {
                    scope: ReviseScope::Execution,
                },
                "wrong tool call sequence",
            ),
            None,
        );
        assert!(o.review_report.is_some());
        assert_eq!(
            o.review_critique.as_deref(),
            Some("wrong tool call sequence")
        );
        // Task orb: both scopes flow to Active.
        assert_eq!(o.status, Some(orbs::orb::OrbStatus::Active));
        assert_eq!(o.revision_count, 1);
    }

    #[test]
    fn apply_revise_decomposition_copies_critique_and_re_enters() {
        let mut o = done_task_orb();
        apply_review_outcome(
            &mut o,
            make_report(
                ReviewVerdict::Revise {
                    scope: ReviseScope::Decomposition,
                },
                "missing migration step",
            ),
            None,
        );
        assert_eq!(o.review_critique.as_deref(), Some("missing migration step"));
        assert_eq!(o.status, Some(orbs::orb::OrbStatus::Active));
        assert_eq!(o.revision_count, 1);
    }

    #[test]
    fn apply_revise_with_empty_critique_does_not_set_field() {
        let mut o = done_task_orb();
        apply_review_outcome(
            &mut o,
            make_report(
                ReviewVerdict::Revise {
                    scope: ReviseScope::Execution,
                },
                "",
            ),
            None,
        );
        assert!(o.review_report.is_some());
        assert!(
            o.review_critique.is_none(),
            "empty critique shouldn't write a misleading hint"
        );
        // Still re-enters even when critique is empty.
        assert_eq!(o.status, Some(orbs::orb::OrbStatus::Active));
    }

    #[test]
    fn apply_revise_at_cap_stays_done_and_skips_re_entry() {
        let mut o = done_task_orb();
        o.revision_count = orbs::orb::MAX_REVISIONS;
        apply_review_outcome(
            &mut o,
            make_report(
                ReviewVerdict::Revise {
                    scope: ReviseScope::Execution,
                },
                "still bad",
            ),
            None,
        );
        // Verdict was recorded but no re-entry happened.
        assert!(o.review_report.is_some());
        assert_eq!(o.status, Some(orbs::orb::OrbStatus::Done));
        assert_eq!(o.revision_count, orbs::orb::MAX_REVISIONS);
    }

    #[cfg(unix)]
    #[test]
    fn apply_fires_on_review_second_opinion_hook_when_sink_present() {
        use crate::hooks::sink::HookSink;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("hooks.toml"),
            r#"
            [[hook]]
            name = "marker"
            on = "on-review-second-opinion"
            run = "true"
            sync = true
            "#,
        )
        .unwrap();
        let sink = HookSink::from_state_dir(dir.path(), dir.path())
            .unwrap()
            .expect("hooks.toml loaded");
        let mut o = done_orb_with_confidence(None);
        apply_review_outcome(&mut o, make_report(ReviewVerdict::Accept, ""), Some(&sink));
        let log = std::fs::read_to_string(dir.path().join("hooks.log.jsonl")).unwrap_or_default();
        assert!(
            log.contains("marker") && log.contains("on-review-second-opinion"),
            "expected hook firing in log: {log}",
        );
    }

    #[test]
    fn apply_accept_does_not_re_enter() {
        let mut o = done_task_orb();
        apply_review_outcome(
            &mut o,
            make_report(ReviewVerdict::Accept, "looks fine"),
            None,
        );
        // Accept verdicts leave the orb at Done.
        assert_eq!(o.status, Some(orbs::orb::OrbStatus::Done));
        assert_eq!(o.revision_count, 0);
    }

    #[test]
    fn apply_reject_does_not_re_enter() {
        let mut o = done_task_orb();
        apply_review_outcome(
            &mut o,
            make_report(ReviewVerdict::Reject, "unrecoverable"),
            None,
        );
        // Reject is terminal — no re-entry attempted.
        assert_eq!(o.status, Some(orbs::orb::OrbStatus::Done));
        assert_eq!(o.revision_count, 0);
    }
}

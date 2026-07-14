use std::fmt::Write as _;
use std::path::PathBuf;

use orbs::orb::{Orb, OrbPhase};
use orbs::pipeline::{self, PipelineDir};

/// Configuration for the iterative refinement loop.
#[derive(Debug, Clone)]
pub struct RefinementConfig {
    /// Maximum number of refinement rounds before forced termination.
    pub max_rounds: u32,
    /// Whether to terminate early when the content hash stabilises.
    pub content_hash_termination: bool,
}

impl Default for RefinementConfig {
    fn default() -> Self {
        Self {
            max_rounds: 5,
            content_hash_termination: true,
        }
    }
}

/// Record of a single refinement round.
#[derive(Debug, Clone)]
pub struct RefinementRound {
    pub round: u32,
    pub changes_made: bool,
    pub content_hash_before: String,
    pub content_hash_after: String,
}

/// Runs the iterative refinement loop on an orb.
///
/// Each round:
/// 1. Computes the content hash before refinement.
/// 2. Applies a refinement pass (currently a stub: trims/normalises the description).
/// 3. Computes the content hash after refinement.
/// 4. If `content_hash_termination` is enabled and the hash is unchanged, stops.
/// 5. If `max_rounds` is reached, stops.
///
/// Returns a vector of round records.
pub fn refine_orb(orb: &mut Orb, config: &RefinementConfig) -> Vec<RefinementRound> {
    let mut rounds = Vec::new();

    for round_num in 1..=config.max_rounds {
        orb.update_content_hash();
        let hash_before = orb.content_hash.clone().unwrap_or_default();

        // Stub refinement: trim and normalise whitespace in description.
        apply_refinement_stub(orb);

        orb.update_content_hash();
        let hash_after = orb.content_hash.clone().unwrap_or_default();

        let changes_made = hash_before != hash_after;

        rounds.push(RefinementRound {
            round: round_num,
            changes_made,
            content_hash_before: hash_before,
            content_hash_after: hash_after,
        });

        if config.content_hash_termination && !changes_made {
            break;
        }
    }

    rounds
}

/// Stub refinement pass: trims leading/trailing whitespace, collapses multiple
/// blank lines, and normalises line endings. Will be replaced by LLM-based
/// refinement later.
fn apply_refinement_stub(orb: &mut Orb) {
    // Trim the description
    let trimmed = orb.description.trim().to_string();

    // Collapse runs of blank lines into a single blank line
    let mut prev_blank = false;
    let normalised: Vec<&str> = trimmed
        .lines()
        .filter(|line| {
            let is_blank = line.trim().is_empty();
            if is_blank && prev_blank {
                return false;
            }
            prev_blank = is_blank;
            true
        })
        .collect();

    orb.description = normalised.join("\n");

    // Also trim design and acceptance_criteria if present
    if let Some(ref mut design) = orb.design {
        *design = design.trim().to_string();
    }
    if let Some(ref mut ac) = orb.acceptance_criteria {
        *ac = ac.trim().to_string();
    }
}

/// Snapshots the current pipeline state for a specific refinement round.
///
/// Creates a snapshot at `snapshots/refinement-{round}/`.
///
/// # Errors
///
/// Returns an error if the snapshot operation fails.
pub fn snapshot_refinement(pipeline_dir: &PipelineDir, round: u32) -> anyhow::Result<PathBuf> {
    let phase_name = format!("refinement-{round}");
    pipeline::snapshot(pipeline_dir, &phase_name)
        .map_err(|e| anyhow::anyhow!("failed to snapshot refinement round {round}: {e}"))
}

/// Transitions an orb from Decomposing to Refining.
///
/// Returns `false` if the orb is not in the Decomposing phase.
pub fn begin_refining(orb: &mut Orb) -> bool {
    if orb.phase != Some(OrbPhase::Decomposing) {
        return false;
    }
    orb.set_phase(OrbPhase::Refining).is_ok()
}

/// Transitions an orb from Refining to Review.
///
/// Returns `false` if the orb is not in the Refining phase.
pub fn finish_refining(orb: &mut Orb) -> bool {
    if orb.phase != Some(OrbPhase::Refining) {
        return false;
    }
    orb.set_phase(OrbPhase::Review).is_ok()
}

// ── Worker-dispatch prompt builder (task 60) ─────────────────────

/// Plan parsed from a refinement worker's response.
///
/// Refinement is a structured edit: the worker is given the current
/// description, design, and acceptance criteria, and asked to
/// produce updated versions of any/all of them. Fields are `Option`
/// so the worker can leave anything it doesn't want to change as
/// null/absent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct RefinementPlan {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub design: Option<String>,
    #[serde(default)]
    pub acceptance_criteria: Option<String>,
    /// Notes from the worker explaining what changed and why. Not
    /// applied to the orb but useful in audit / hook payloads.
    #[serde(default)]
    pub notes: Option<String>,
}

/// Returns `(system, user)` prompts for the refinement worker.
/// Carries forward any pending `review_critique` so the worker has
/// the reviewer's feedback when revising.
#[must_use]
pub fn build_prompt(orb: &Orb) -> (String, String) {
    let system = "You are refining a task spec. Review the current title, description, \
design, and acceptance criteria. Make targeted improvements — sharpen ambiguous \
wording, fill in obvious gaps, fix inconsistencies. DO NOT rewrite for the sake \
of rewriting. Respond with exactly one JSON object — no surrounding prose, no \
code fences — in this shape:\n\
  {\"description\": \"<revised>\" | null,\n\
   \"design\": \"<revised>\" | null,\n\
   \"acceptance_criteria\": \"<revised>\" | null,\n\
   \"notes\": \"<what you changed and why>\"}\n\
Use null for any field you don't want to change. If nothing needs revision, \
return all-null and a brief note explaining why."
        .to_string();
    let mut user = format!(
        "Title: {}\n\nDescription:\n{}\n",
        orb.title, orb.description
    );
    if let Some(ref design) = orb.design {
        let _ = write!(user, "\nDesign:\n{design}\n");
    }
    if let Some(ref ac) = orb.acceptance_criteria {
        let _ = write!(user, "\nAcceptance criteria:\n{ac}\n");
    }
    if let Some(ref critique) = orb.review_critique {
        let _ = write!(user, "\nReviewer feedback to incorporate:\n{critique}\n");
    }
    (system, user)
}

/// Parses the worker's response into a `RefinementPlan`. Accepts
/// strict JSON or a fenced JSON block.
#[must_use]
pub fn parse_response(text: &str) -> Option<RefinementPlan> {
    crate::phases::prompt_util::parse_response_json::<RefinementPlan>(text)
}

/// Applies a refinement plan to the orb. Only writes fields that the
/// plan explicitly sets (Some). Updates `updated_at` and the content
/// hash so refinement-loop termination picks up the change.
pub fn apply_plan(orb: &mut Orb, plan: &RefinementPlan) {
    let mut any_change = false;
    if let Some(ref d) = plan.description {
        orb.description.clone_from(d);
        any_change = true;
    }
    if let Some(ref d) = plan.design {
        orb.design = Some(d.clone());
        any_change = true;
    }
    if let Some(ref ac) = plan.acceptance_criteria {
        orb.acceptance_criteria = Some(ac.clone());
        any_change = true;
    }
    if any_change {
        orb.updated_at = chrono::Utc::now();
        orb.update_content_hash();
    }
}

#[cfg(test)]
mod tests {
    use orbs::orb::OrbType;

    use super::*;

    fn feature_orb(title: &str, desc: &str) -> Orb {
        let mut orb = Orb::new(title, desc).with_type(OrbType::Feature);
        orb.phase = Some(OrbPhase::Refining); // test setup
        orb
    }

    // ── config defaults ─────────────────────────────────────

    #[test]
    fn config_defaults() {
        let config = RefinementConfig::default();
        assert_eq!(config.max_rounds, 5);
        assert!(config.content_hash_termination);
    }

    // ── phase transitions ────────────────────────────────────

    #[test]
    fn begin_refining_from_decomposing() {
        let mut orb = Orb::new("Auth", "Implement auth").with_type(OrbType::Feature);
        orb.phase = Some(OrbPhase::Decomposing); // test setup

        assert!(begin_refining(&mut orb));
        assert_eq!(orb.phase, Some(OrbPhase::Refining));
    }

    #[test]
    fn begin_refining_from_non_decomposing_fails() {
        let mut orb = Orb::new("Auth", "Implement auth").with_type(OrbType::Feature);
        // Phase is Pending (default)
        assert!(!begin_refining(&mut orb));
        assert_eq!(orb.phase, Some(OrbPhase::Pending));
    }

    #[test]
    fn finish_refining_transitions_to_review() {
        let mut orb = feature_orb("Auth", "Implement auth");

        assert!(finish_refining(&mut orb));
        assert_eq!(orb.phase, Some(OrbPhase::Review));
    }

    #[test]
    fn finish_refining_from_non_refining_fails() {
        let mut orb = Orb::new("Auth", "Implement auth").with_type(OrbType::Feature);
        orb.phase = Some(OrbPhase::Decomposing); // test setup

        assert!(!finish_refining(&mut orb));
        assert_eq!(orb.phase, Some(OrbPhase::Decomposing));
    }

    // ── refinement loop ─────────────────────────────────────

    #[test]
    fn refinement_terminates_on_hash_stability() {
        // Clean description → hash won't change after first round
        let mut orb = feature_orb("Auth", "Clean description");
        let config = RefinementConfig::default();

        let rounds = refine_orb(&mut orb, &config);

        // Should terminate early (round 1 changes nothing meaningful,
        // round 2 detects stability). Actually round 1 may or may not
        // change the hash depending on trimming; once stable it stops.
        assert!(
            rounds.len() < config.max_rounds as usize,
            "should terminate before max_rounds ({}) but ran {} rounds",
            config.max_rounds,
            rounds.len()
        );

        // Last round should show no changes
        let last = rounds.last().unwrap();
        assert!(!last.changes_made);
        assert_eq!(last.content_hash_before, last.content_hash_after);
    }

    #[test]
    fn refinement_terminates_on_max_rounds() {
        // With content_hash_termination disabled, must run all rounds
        let mut orb = feature_orb("Auth", "Clean description");
        let config = RefinementConfig {
            max_rounds: 3,
            content_hash_termination: false,
        };

        let rounds = refine_orb(&mut orb, &config);
        assert_eq!(rounds.len(), 3);
    }

    #[test]
    fn refinement_detects_content_change() {
        // Whitespace-heavy description should change on first round
        let mut orb = feature_orb("Auth", "  lots   of   whitespace  \n\n\n\nextra lines  ");
        let config = RefinementConfig {
            max_rounds: 5,
            content_hash_termination: true,
        };

        let rounds = refine_orb(&mut orb, &config);

        // First round should detect a change (trimming modifies description)
        assert!(rounds[0].changes_made);
        assert_ne!(rounds[0].content_hash_before, rounds[0].content_hash_after);
    }

    #[test]
    fn refinement_round_records_correct_round_numbers() {
        let mut orb = feature_orb("Auth", "Test description");
        let config = RefinementConfig {
            max_rounds: 3,
            content_hash_termination: false,
        };

        let rounds = refine_orb(&mut orb, &config);
        for (i, round) in rounds.iter().enumerate() {
            assert_eq!(round.round, u32::try_from(i + 1).unwrap());
        }
    }

    // ── snapshot per round ──────────────────────────────────

    #[test]
    fn snapshot_refinement_creates_round_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let orb = Orb::new("Snap test", "Test snapshotting").with_type(OrbType::Feature);
        let pipeline_dir = pipeline::create_pipeline(tmp.path(), &orb).unwrap();

        // Add data so there's something to snapshot
        let store = pipeline_dir.orb_store();
        store.append(&orb).unwrap();

        let snap_path = snapshot_refinement(&pipeline_dir, 1).unwrap();

        assert!(snap_path.exists());
        assert!(snap_path.join("orbs.jsonl").exists());
        assert!(
            snap_path
                .to_str()
                .unwrap()
                .contains("snapshots/refinement-1"),
            "snapshot should be in snapshots/refinement-1/"
        );
    }

    #[test]
    fn snapshot_refinement_multiple_rounds() {
        let tmp = tempfile::tempdir().unwrap();
        let orb = Orb::new("Snap test", "Test snapshotting").with_type(OrbType::Feature);
        let pipeline_dir = pipeline::create_pipeline(tmp.path(), &orb).unwrap();

        let store = pipeline_dir.orb_store();
        store.append(&orb).unwrap();

        let snap1 = snapshot_refinement(&pipeline_dir, 1).unwrap();
        let snap2 = snapshot_refinement(&pipeline_dir, 2).unwrap();

        assert!(snap1.exists());
        assert!(snap2.exists());
        assert_ne!(snap1, snap2);
        assert!(snap2.to_str().unwrap().contains("refinement-2"));
    }

    // ── end-to-end flow ─────────────────────────────────────

    #[test]
    fn full_refinement_flow() {
        let mut orb = Orb::new("Auth flow", "  Design auth\n\n\n\nImplement login  ")
            .with_type(OrbType::Feature);

        // 1. Start in Decomposing, transition to Refining
        orb.phase = Some(OrbPhase::Decomposing); // test setup
        assert!(begin_refining(&mut orb));
        assert_eq!(orb.phase, Some(OrbPhase::Refining));

        // 2. Run refinement
        let config = RefinementConfig::default();
        let rounds = refine_orb(&mut orb, &config);

        // Should have at least one round
        assert!(!rounds.is_empty());

        // Description should be cleaned up
        assert!(!orb.description.starts_with(' '));
        assert!(!orb.description.ends_with(' '));
        assert!(
            !orb.description.contains("\n\n\n"),
            "consecutive blank lines should be collapsed"
        );

        // 3. Transition to Review
        assert!(finish_refining(&mut orb));
        assert_eq!(orb.phase, Some(OrbPhase::Review));
    }

    // ── build_prompt / parse_response / apply_plan ────────────

    #[test]
    fn build_prompt_includes_critique_when_present() {
        let mut orb = feature_orb("X", "Y");
        orb.review_critique = Some("plan missed step Z".into());
        let (_system, user) = build_prompt(&orb);
        assert!(user.contains("Reviewer feedback"));
        assert!(user.contains("plan missed step Z"));
    }

    #[test]
    fn build_prompt_omits_critique_section_when_absent() {
        let orb = feature_orb("X", "Y");
        let (_system, user) = build_prompt(&orb);
        assert!(!user.contains("Reviewer feedback"));
    }

    #[test]
    fn parse_response_extracts_all_fields() {
        let text = r#"{"description": "new desc", "design": "new design", "acceptance_criteria": "new ac", "notes": "changed everything"}"#;
        let plan = parse_response(text).unwrap();
        assert_eq!(plan.description.as_deref(), Some("new desc"));
        assert_eq!(plan.design.as_deref(), Some("new design"));
        assert_eq!(plan.notes.as_deref(), Some("changed everything"));
    }

    #[test]
    fn parse_response_handles_all_null_fields() {
        let text = r#"{"description": null, "design": null, "acceptance_criteria": null, "notes": "nothing to change"}"#;
        let plan = parse_response(text).unwrap();
        assert!(plan.description.is_none());
        assert!(plan.design.is_none());
        assert!(plan.acceptance_criteria.is_none());
    }

    #[test]
    fn apply_plan_only_writes_set_fields() {
        let mut orb = feature_orb("X", "original desc");
        orb.design = Some("original design".into());
        let plan = RefinementPlan {
            description: Some("new desc".into()),
            design: None, // leave alone
            acceptance_criteria: None,
            notes: None,
        };
        apply_plan(&mut orb, &plan);
        assert_eq!(orb.description, "new desc");
        assert_eq!(orb.design.as_deref(), Some("original design"));
    }

    #[test]
    fn apply_plan_with_all_none_is_noop_on_content_hash() {
        let mut orb = feature_orb("X", "Y");
        orb.update_content_hash();
        let before = orb.content_hash.clone();
        apply_plan(
            &mut orb,
            &RefinementPlan {
                description: None,
                design: None,
                acceptance_criteria: None,
                notes: Some("nothing".into()),
            },
        );
        assert_eq!(orb.content_hash, before);
    }
}

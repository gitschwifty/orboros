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
    orb.set_phase(OrbPhase::Refining);
    true
}

/// Transitions an orb from Refining to Review.
///
/// Returns `false` if the orb is not in the Refining phase.
pub fn finish_refining(orb: &mut Orb) -> bool {
    if orb.phase != Some(OrbPhase::Refining) {
        return false;
    }
    orb.set_phase(OrbPhase::Review);
    true
}

#[cfg(test)]
mod tests {
    use orbs::orb::OrbType;

    use super::*;

    fn feature_orb(title: &str, desc: &str) -> Orb {
        let mut orb = Orb::new(title, desc).with_type(OrbType::Feature);
        orb.set_phase(OrbPhase::Refining);
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
        orb.set_phase(OrbPhase::Decomposing);

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
        orb.set_phase(OrbPhase::Decomposing);

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
        orb.set_phase(OrbPhase::Decomposing);
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
}

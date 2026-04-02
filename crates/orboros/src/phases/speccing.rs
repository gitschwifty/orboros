use orbs::orb::{Orb, OrbPhase};

use crate::worker::process::WorkerConfig;

/// Where the spec originated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecSource {
    /// User already provided design + `acceptance_criteria`.
    Provided,
    /// LLM generated the spec.
    Generated,
    /// No spec available.
    None,
}

/// Result of spec detection or generation.
#[derive(Debug, Clone)]
pub struct SpecResult {
    pub has_spec: bool,
    pub spec_source: SpecSource,
    pub generated_spec: Option<GeneratedSpec>,
}

/// A generated spec payload (design + `acceptance_criteria`).
#[derive(Debug, Clone)]
pub struct GeneratedSpec {
    pub design: String,
    pub acceptance_criteria: String,
}

/// Checks whether the orb already has a complete spec (both design and
/// `acceptance_criteria` populated and non-empty).
pub fn detect_spec(orb: &Orb) -> SpecResult {
    let has_design = orb.design.as_ref().is_some_and(|d| !d.trim().is_empty());
    let has_criteria = orb
        .acceptance_criteria
        .as_ref()
        .is_some_and(|c| !c.trim().is_empty());

    if has_design && has_criteria {
        SpecResult {
            has_spec: true,
            spec_source: SpecSource::Provided,
            generated_spec: None,
        }
    } else {
        SpecResult {
            has_spec: false,
            spec_source: SpecSource::None,
            generated_spec: None,
        }
    }
}

/// Uses a worker to generate a spec from the orb's title/description.
///
/// Currently a stub that returns a placeholder spec. Will be replaced with
/// actual LLM-based generation once the worker pipeline is wired up.
///
/// # Errors
///
/// Returns an error if spec generation fails (currently infallible as a stub).
pub fn generate_spec(orb: &Orb, _config: &WorkerConfig) -> anyhow::Result<SpecResult> {
    let design = format!(
        "Auto-generated design for: {}\n\n{}",
        orb.title, orb.description
    );
    let acceptance_criteria = format!("- [ ] {} is implemented and tested", orb.title);

    Ok(SpecResult {
        has_spec: true,
        spec_source: SpecSource::Generated,
        generated_spec: Some(GeneratedSpec {
            design,
            acceptance_criteria,
        }),
    })
}

/// Applies a spec result to the orb's fields. Only writes fields if the spec
/// contains generated content.
pub fn apply_spec(orb: &mut Orb, spec: &SpecResult) {
    if let Some(generated) = &spec.generated_spec {
        orb.design = Some(generated.design.clone());
        orb.acceptance_criteria = Some(generated.acceptance_criteria.clone());
    }
}

/// Transitions an orb from Pending to Speccing. Returns `false` if the orb is
/// not in the Pending phase.
pub fn begin_speccing(orb: &mut Orb) -> bool {
    if orb.phase != Some(OrbPhase::Pending) {
        return false;
    }
    orb.set_phase(OrbPhase::Speccing);
    true
}

/// Transitions an orb from Speccing to Decomposing (spec is ready). Returns
/// `false` if the orb is not in the Speccing phase.
pub fn finish_speccing(orb: &mut Orb) -> bool {
    if orb.phase != Some(OrbPhase::Speccing) {
        return false;
    }
    orb.set_phase(OrbPhase::Decomposing);
    true
}

#[cfg(test)]
mod tests {
    use orbs::orb::OrbType;

    use super::*;

    fn feature_orb(title: &str, desc: &str) -> Orb {
        Orb::new(title, desc).with_type(OrbType::Feature)
    }

    // ── detect_spec ──────────────────────────────────────────

    #[test]
    fn detect_spec_with_both_fields_populated() {
        let mut orb = feature_orb("Auth flow", "Implement OAuth");
        orb.design = Some("Use PKCE flow with refresh tokens".into());
        orb.acceptance_criteria = Some("- Login works\n- Token refreshes".into());

        let result = detect_spec(&orb);
        assert!(result.has_spec);
        assert_eq!(result.spec_source, SpecSource::Provided);
        assert!(result.generated_spec.is_none());
    }

    #[test]
    fn detect_spec_missing_design() {
        let mut orb = feature_orb("Auth flow", "Implement OAuth");
        orb.acceptance_criteria = Some("- Login works".into());

        let result = detect_spec(&orb);
        assert!(!result.has_spec);
        assert_eq!(result.spec_source, SpecSource::None);
    }

    #[test]
    fn detect_spec_missing_acceptance_criteria() {
        let mut orb = feature_orb("Auth flow", "Implement OAuth");
        orb.design = Some("Use PKCE flow".into());

        let result = detect_spec(&orb);
        assert!(!result.has_spec);
        assert_eq!(result.spec_source, SpecSource::None);
    }

    #[test]
    fn detect_spec_both_missing() {
        let orb = feature_orb("Auth flow", "Implement OAuth");
        let result = detect_spec(&orb);
        assert!(!result.has_spec);
        assert_eq!(result.spec_source, SpecSource::None);
    }

    #[test]
    fn detect_spec_empty_strings_treated_as_missing() {
        let mut orb = feature_orb("Auth flow", "Implement OAuth");
        orb.design = Some("".into());
        orb.acceptance_criteria = Some("  ".into());

        let result = detect_spec(&orb);
        assert!(!result.has_spec);
        assert_eq!(result.spec_source, SpecSource::None);
    }

    // ── apply_spec ───────────────────────────────────────────

    #[test]
    fn apply_spec_updates_orb_fields() {
        let mut orb = feature_orb("Auth flow", "Implement OAuth");
        assert!(orb.design.is_none());
        assert!(orb.acceptance_criteria.is_none());

        let spec = SpecResult {
            has_spec: true,
            spec_source: SpecSource::Generated,
            generated_spec: Some(GeneratedSpec {
                design: "PKCE flow design".into(),
                acceptance_criteria: "- Login works".into(),
            }),
        };

        apply_spec(&mut orb, &spec);
        assert_eq!(orb.design.as_deref(), Some("PKCE flow design"));
        assert_eq!(orb.acceptance_criteria.as_deref(), Some("- Login works"));
    }

    #[test]
    fn apply_spec_noop_when_no_generated_content() {
        let mut orb = feature_orb("Auth flow", "Implement OAuth");
        orb.design = Some("Existing design".into());

        let spec = SpecResult {
            has_spec: true,
            spec_source: SpecSource::Provided,
            generated_spec: None,
        };

        apply_spec(&mut orb, &spec);
        // Existing fields are untouched
        assert_eq!(orb.design.as_deref(), Some("Existing design"));
        assert!(orb.acceptance_criteria.is_none());
    }

    // ── phase transitions ────────────────────────────────────

    #[test]
    fn begin_speccing_from_pending() {
        let mut orb = feature_orb("Auth flow", "Implement OAuth");
        assert_eq!(orb.phase, Some(OrbPhase::Pending));

        assert!(begin_speccing(&mut orb));
        assert_eq!(orb.phase, Some(OrbPhase::Speccing));
    }

    #[test]
    fn begin_speccing_from_non_pending_fails() {
        let mut orb = feature_orb("Auth flow", "Implement OAuth");
        orb.set_phase(OrbPhase::Decomposing);

        assert!(!begin_speccing(&mut orb));
        assert_eq!(orb.phase, Some(OrbPhase::Decomposing));
    }

    #[test]
    fn finish_speccing_transitions_to_decomposing() {
        let mut orb = feature_orb("Auth flow", "Implement OAuth");
        orb.set_phase(OrbPhase::Speccing);

        assert!(finish_speccing(&mut orb));
        assert_eq!(orb.phase, Some(OrbPhase::Decomposing));
    }

    #[test]
    fn finish_speccing_from_non_speccing_fails() {
        let mut orb = feature_orb("Auth flow", "Implement OAuth");
        assert_eq!(orb.phase, Some(OrbPhase::Pending));

        assert!(!finish_speccing(&mut orb));
        assert_eq!(orb.phase, Some(OrbPhase::Pending));
    }

    // ── generate_spec (stub) ─────────────────────────────────

    #[test]
    fn generate_spec_returns_placeholder() {
        let orb = feature_orb("Auth flow", "Implement OAuth");
        let config = stub_worker_config();

        let result = generate_spec(&orb, &config).unwrap();
        assert!(result.has_spec);
        assert_eq!(result.spec_source, SpecSource::Generated);

        let generated = result.generated_spec.unwrap();
        assert!(generated.design.contains("Auth flow"));
        assert!(generated.acceptance_criteria.contains("Auth flow"));
    }

    // ── end-to-end flow ──────────────────────────────────────

    #[test]
    fn full_speccing_flow_with_generation() {
        let mut orb = feature_orb("Auth flow", "Implement OAuth");
        let config = stub_worker_config();

        // 1. Detect: no spec yet
        let detected = detect_spec(&orb);
        assert!(!detected.has_spec);

        // 2. Begin speccing
        assert!(begin_speccing(&mut orb));
        assert_eq!(orb.phase, Some(OrbPhase::Speccing));

        // 3. Generate spec
        let spec = generate_spec(&orb, &config).unwrap();
        assert!(spec.has_spec);

        // 4. Apply spec
        apply_spec(&mut orb, &spec);
        assert!(orb.design.is_some());
        assert!(orb.acceptance_criteria.is_some());

        // 5. Finish speccing → decomposing
        assert!(finish_speccing(&mut orb));
        assert_eq!(orb.phase, Some(OrbPhase::Decomposing));

        // 6. Detect again: now has spec
        let detected = detect_spec(&orb);
        assert!(detected.has_spec);
        assert_eq!(detected.spec_source, SpecSource::Provided);
    }

    #[test]
    fn full_speccing_flow_with_existing_spec() {
        let mut orb = feature_orb("Auth flow", "Implement OAuth");
        orb.design = Some("Already designed".into());
        orb.acceptance_criteria = Some("Already specified".into());

        // 1. Detect: spec already present
        let detected = detect_spec(&orb);
        assert!(detected.has_spec);
        assert_eq!(detected.spec_source, SpecSource::Provided);

        // 2. Begin speccing
        assert!(begin_speccing(&mut orb));

        // 3. Skip generation, go straight to finish
        assert!(finish_speccing(&mut orb));
        assert_eq!(orb.phase, Some(OrbPhase::Decomposing));

        // Fields untouched
        assert_eq!(orb.design.as_deref(), Some("Already designed"));
    }

    fn stub_worker_config() -> WorkerConfig {
        WorkerConfig {
            command: "echo".into(),
            args: vec![],
            cwd: None,
            env: vec![],
            model: "stub".into(),
            system_prompt: String::new(),
            tools: vec![],
            max_iterations: None,
            init_timeout: None,
            send_timeout: None,
            shutdown_timeout: None,
            task_id: None,
            worker_id: None,
        }
    }
}

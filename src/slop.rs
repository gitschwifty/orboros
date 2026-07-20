use orbs::orb::Orb;
use serde::{Deserialize, Serialize};

/// Post-completion slop check mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlopMode {
    /// Auto-fix issues found.
    Fix,
    /// Report issues without fixing.
    Review,
    /// Suggest improvements.
    Suggest,
}

/// Severity level for a slop check finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// Configuration for slop checking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlopConfig {
    pub mode: SlopMode,
    pub max_passes: u32,
    pub enabled: bool,
}

impl Default for SlopConfig {
    fn default() -> Self {
        Self {
            mode: SlopMode::Review,
            max_passes: 3,
            enabled: true,
        }
    }
}

/// A single slop check definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlopCheck {
    pub name: String,
    pub description: String,
    pub severity: Severity,
}

/// Report from running slop checks on an orb.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlopReport {
    pub issues_found: u32,
    pub issues_fixed: u32,
    pub suggestions: Vec<String>,
    pub passes_run: u32,
}

/// Run slop checks against a completed orb.
///
/// Stub implementation that checks:
/// - Empty/too-short result -> Error
/// - TODO/FIXME/HACK markers in result -> Warning
/// - Description mentions tests but result doesn't -> Info suggestion
pub fn run_slop_check(orb: &Orb, config: &SlopConfig) -> SlopReport {
    if !config.enabled {
        return SlopReport::default();
    }

    let mut report = SlopReport {
        passes_run: 1,
        ..Default::default()
    };

    let result_text = orb.result.as_deref().unwrap_or("");

    // Check 1: empty or too-short result
    if result_text.len() < 10 {
        report.issues_found += 1;
        report
            .suggestions
            .push("Result is empty or too short (fewer than 10 characters)".to_string());
    }

    // Check 2: TODO/FIXME/HACK markers
    let markers = ["TODO", "FIXME", "HACK"];
    for marker in &markers {
        if result_text.contains(marker) {
            report.issues_found += 1;
            report
                .suggestions
                .push(format!("Result contains {marker} marker"));
        }
    }

    // Check 3: description mentions tests but result doesn't
    let desc_lower = orb.description.to_lowercase();
    let result_lower = result_text.to_lowercase();
    if desc_lower.contains("test") && !result_lower.contains("test") {
        report.issues_found += 1;
        report
            .suggestions
            .push("Description mentions tests but result does not reference testing".to_string());
    }

    report
}

/// Format a slop report as a human-readable string.
pub fn format_report(report: &SlopReport) -> String {
    let mut lines = Vec::new();

    lines.push(format!("Slop Report ({} pass(es) run)", report.passes_run));
    lines.push(format!("Issues found: {}", report.issues_found));
    lines.push(format!("Issues fixed: {}", report.issues_fixed));

    if !report.suggestions.is_empty() {
        lines.push(String::new());
        lines.push("Suggestions:".to_string());
        for suggestion in &report.suggestions {
            lines.push(format!("  - {suggestion}"));
        }
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_orb(description: &str, result: Option<&str>) -> Orb {
        let mut orb = Orb::new("Test orb", description);
        orb.result = result.map(String::from);
        orb
    }

    fn default_config() -> SlopConfig {
        SlopConfig::default()
    }

    // ── Config defaults ───────────────────────────────────────────

    #[test]
    fn config_defaults() {
        let config = SlopConfig::default();
        assert_eq!(config.mode, SlopMode::Review);
        assert_eq!(config.max_passes, 3);
        assert!(config.enabled);
    }

    // ── Clean result (no issues) ──────────────────────────────────

    #[test]
    fn clean_result_has_no_issues() {
        let orb = make_orb(
            "Implement the frobulator",
            Some("Implemented the frobulator with proper error handling and documentation."),
        );
        let report = run_slop_check(&orb, &default_config());
        assert_eq!(report.issues_found, 0);
        assert!(report.suggestions.is_empty());
        assert_eq!(report.passes_run, 1);
    }

    // ── Empty result (error) ──────────────────────────────────────

    #[test]
    fn empty_result_reports_error() {
        let orb = make_orb("Do something", None);
        let report = run_slop_check(&orb, &default_config());
        assert!(report.issues_found >= 1);
        assert!(report
            .suggestions
            .iter()
            .any(|s| s.contains("empty or too short")));
    }

    #[test]
    fn short_result_reports_error() {
        let orb = make_orb("Do something", Some("ok"));
        let report = run_slop_check(&orb, &default_config());
        assert!(report.issues_found >= 1);
        assert!(report.suggestions.iter().any(|s| s.contains("too short")));
    }

    // ── TODO markers (warning) ────────────────────────────────────

    #[test]
    fn todo_marker_reports_warning() {
        let orb = make_orb(
            "Implement feature",
            Some("Implemented the feature. TODO: add error handling later."),
        );
        let report = run_slop_check(&orb, &default_config());
        assert!(report.issues_found >= 1);
        assert!(report.suggestions.iter().any(|s| s.contains("TODO")));
    }

    #[test]
    fn fixme_marker_reports_warning() {
        let orb = make_orb(
            "Implement feature",
            Some("Implemented the feature. FIXME: this is a hack."),
        );
        let report = run_slop_check(&orb, &default_config());
        assert!(report.issues_found >= 1);
        assert!(report.suggestions.iter().any(|s| s.contains("FIXME")));
    }

    #[test]
    fn hack_marker_reports_warning() {
        let orb = make_orb(
            "Implement feature",
            Some("Implemented the feature with a HACK workaround."),
        );
        let report = run_slop_check(&orb, &default_config());
        assert!(report.issues_found >= 1);
        assert!(report.suggestions.iter().any(|s| s.contains("HACK")));
    }

    // ── Missing tests mention (suggestion) ────────────────────────

    #[test]
    fn missing_tests_mention_reports_suggestion() {
        let orb = make_orb(
            "Write tests for the auth module",
            Some("Implemented the auth module with proper validation and error handling."),
        );
        let report = run_slop_check(&orb, &default_config());
        assert!(report.issues_found >= 1);
        assert!(report.suggestions.iter().any(|s| s.contains("test")));
    }

    #[test]
    fn tests_mentioned_in_both_no_suggestion() {
        let orb = make_orb(
            "Write tests for the auth module",
            Some("Added comprehensive tests for the auth module with edge cases covered."),
        );
        let report = run_slop_check(&orb, &default_config());
        // Should NOT flag missing tests since result mentions "tests"
        assert!(!report
            .suggestions
            .iter()
            .any(|s| s.contains("tests but result")));
    }

    // ── Disabled config ───────────────────────────────────────────

    #[test]
    fn disabled_config_returns_empty_report() {
        let orb = make_orb("Do something", None);
        let config = SlopConfig {
            enabled: false,
            ..Default::default()
        };
        let report = run_slop_check(&orb, &config);
        assert_eq!(report.issues_found, 0);
        assert_eq!(report.passes_run, 0);
    }

    // ── max_passes field ──────────────────────────────────────────

    #[test]
    fn max_passes_stored_in_config() {
        let config = SlopConfig {
            max_passes: 5,
            ..Default::default()
        };
        assert_eq!(config.max_passes, 5);
    }

    // ── Report formatting ─────────────────────────────────────────

    #[test]
    fn format_report_no_suggestions() {
        let report = SlopReport {
            issues_found: 0,
            issues_fixed: 0,
            suggestions: vec![],
            passes_run: 1,
        };
        let text = format_report(&report);
        assert!(text.contains("Issues found: 0"));
        assert!(text.contains("1 pass(es) run"));
        assert!(!text.contains("Suggestions:"));
    }

    #[test]
    fn format_report_with_suggestions() {
        let report = SlopReport {
            issues_found: 2,
            issues_fixed: 1,
            suggestions: vec![
                "Result contains TODO marker".to_string(),
                "Result is empty or too short".to_string(),
            ],
            passes_run: 1,
        };
        let text = format_report(&report);
        assert!(text.contains("Issues found: 2"));
        assert!(text.contains("Issues fixed: 1"));
        assert!(text.contains("Suggestions:"));
        assert!(text.contains("  - Result contains TODO marker"));
        assert!(text.contains("  - Result is empty or too short"));
    }

    // ── Serde round-trip ──────────────────────────────────────────

    #[test]
    fn slop_mode_serde() {
        let json = serde_json::to_string(&SlopMode::Fix).unwrap();
        assert_eq!(json, "\"fix\"");
        let parsed: SlopMode = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, SlopMode::Fix);
    }

    #[test]
    fn severity_serde() {
        let json = serde_json::to_string(&Severity::Warning).unwrap();
        assert_eq!(json, "\"warning\"");
        let parsed: Severity = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, Severity::Warning);
    }

    #[test]
    fn slop_config_serde_round_trip() {
        let config = SlopConfig {
            mode: SlopMode::Suggest,
            max_passes: 5,
            enabled: false,
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: SlopConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.mode, SlopMode::Suggest);
        assert_eq!(parsed.max_passes, 5);
        assert!(!parsed.enabled);
    }

    #[test]
    fn slop_check_struct() {
        let check = SlopCheck {
            name: "empty_result".to_string(),
            description: "Checks for empty results".to_string(),
            severity: Severity::Error,
        };
        assert_eq!(check.name, "empty_result");
        assert_eq!(check.severity, Severity::Error);
    }
}

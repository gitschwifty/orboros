//! Benchmark case schema and loader.
//!
//! Cases live in self-contained directories under
//! `bench/<tier>/<NNN>-<slug>/case.toml`.
//! Loaded eagerly at harness startup.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::routing::profile::PhaseToolPolicy;

/// Default case timeout when neither benchmark config nor the case
/// provides one.
pub const DEFAULT_TIMEOUT_S: u32 = 120;

/// Benchmark tier — affects which runner code path executes the case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BenchTier {
    /// Single-shot worker test, no decomposition.
    T1,
    /// Modify-existing-project with a seed repo.
    T2,
    /// Greenfield from a single prompt; rubric grader.
    T3,
}

impl BenchTier {
    /// Lowercase string used in CLI args and result store paths.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            BenchTier::T1 => "t1",
            BenchTier::T2 => "t2",
            BenchTier::T3 => "t3",
        }
    }
}

impl std::str::FromStr for BenchTier {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "t1" | "1" => Ok(BenchTier::T1),
            "t2" | "2" => Ok(BenchTier::T2),
            "t3" | "3" => Ok(BenchTier::T3),
            other => Err(format!(
                "unknown bench tier '{other}', expected one of: t1, t2, t3"
            )),
        }
    }
}

impl std::fmt::Display for BenchTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How to score a case's output.
///
/// `Exact` and `Regex` are used by T1 single-shot cases. `TestsPass`
/// runs a command in the case's working directory (typically a copied
/// seed repo) and treats the case as passing iff the command exits 0
/// — used by T2. `Rubric` defers to a grader worker that scores
/// against a list of criteria — used by T3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BenchExpected {
    Exact { text: String },
    Regex { pattern: String },
    TestsPass { command: String },
    Rubric { criteria: Vec<String> },
}

/// Execution strategy for a benchmark case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchRunner {
    /// T2 runner creates one task orb from the case prompt.
    SingleTask,
    /// T2 runner creates a feature root and drives speccing/decomposition
    /// before dispatching the generated child task orbs.
    Decompose,
}

/// A single benchmark case.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchCase {
    /// Stable identifier — used in result store rows and CLI selectors.
    pub id: String,
    pub tier: BenchTier,
    pub name: String,
    /// Human description of what the case exercises. Not sent to the
    /// worker — `prompt` is.
    pub description: String,
    /// Prompt sent to the worker as the user message.
    pub prompt: String,
    pub expected: BenchExpected,
    /// Optional runner override. Defaults to `single_task`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner: Option<BenchRunner>,
    /// Per-case timeout in seconds. Overrides `[bench].timeout_s`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<u32>,
    /// Per-case worker iteration/tool-call budget. Overrides
    /// `[bench].max_iterations`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<u32>,
    /// Per-case cost ceiling in cents. The harness enforces this
    /// unless invoked with `--no-budget`.
    #[serde(default = "default_max_cost_cents")]
    pub max_cost_cents: u32,
    /// Optional benchmark-only phase tool policy. It can select a profile or
    /// exact tools for every phase, then refine individual phases. The runner's
    /// base tool list remains a hard ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_policy: Option<PhaseToolPolicy>,
    /// Stable human-facing selector derived from the case directory,
    /// for example `t2.001`. It is not part of case.toml.
    #[serde(skip)]
    pub selector: String,
    /// Directory containing `case.toml` and optional local resources.
    #[serde(skip)]
    pub case_dir: PathBuf,
    /// T2 fixture directory, when present at `<case_dir>/fixture`.
    #[serde(skip)]
    pub fixture_dir: Option<PathBuf>,
    /// Optional grading overlay at `<case_dir>/overlay`.
    #[serde(skip)]
    pub test_overlay_dir: Option<PathBuf>,
}

fn default_max_cost_cents() -> u32 {
    50
}

#[derive(Debug, thiserror::Error)]
pub enum CorpusError {
    #[error("failed to read case file {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse case file {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("case id mismatch: file {path} has id={file_id} but loader expected {expected_id}")]
    IdMismatch {
        path: PathBuf,
        file_id: String,
        expected_id: String,
    },
    #[error("case tier mismatch: file {path} has tier={file_tier} but is under {expected_tier} directory")]
    TierMismatch {
        path: PathBuf,
        file_tier: BenchTier,
        expected_tier: BenchTier,
    },
    #[error("invalid case directory {path}; expected NNN-slug")]
    InvalidCaseDirectory { path: PathBuf },
    #[error("missing case.toml in case directory {path}")]
    MissingCaseFile { path: PathBuf },
    #[error("T2 case {path} is missing its fixture/ directory")]
    MissingFixture { path: PathBuf },
    #[error("duplicate benchmark selector `{selector}` under {path}")]
    DuplicateSelector { path: PathBuf, selector: String },
}

/// Loads a single case from a TOML file. Verifies the embedded `tier`
/// matches `expected_tier` when provided.
///
/// # Errors
///
/// Returns a [`CorpusError`] for I/O failures, TOML parse errors, or
/// tier/id mismatches.
pub fn load_case(path: &Path, expected_tier: Option<BenchTier>) -> Result<BenchCase, CorpusError> {
    let raw = std::fs::read_to_string(path).map_err(|e| CorpusError::Read {
        path: path.to_path_buf(),
        source: e,
    })?;
    let mut case: BenchCase = toml::from_str(&raw).map_err(|e| CorpusError::Parse {
        path: path.to_path_buf(),
        source: e,
    })?;
    if let Some(t) = expected_tier {
        if case.tier != t {
            return Err(CorpusError::TierMismatch {
                path: path.to_path_buf(),
                file_tier: case.tier,
                expected_tier: t,
            });
        }
    }
    case.selector = case.id.clone();
    case.case_dir = path.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
    case.fixture_dir = None;
    case.test_overlay_dir = None;
    Ok(case)
}

/// Loads all cases under `root/<tier>/<NNN>-<slug>/case.toml`.
/// Returns cases sorted by their numeric selectors for stable iteration.
///
/// # Errors
///
/// Returns a [`CorpusError`] if a case file is malformed. Missing
/// tier directories return an empty Vec rather than erroring — useful
/// when T2/T3 corpora haven't been authored yet.
pub fn load_tier(root: &Path, tier: BenchTier) -> Result<Vec<BenchCase>, CorpusError> {
    let dir = root.join(tier.as_str());
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let entries = std::fs::read_dir(&dir).map_err(|e| CorpusError::Read {
        path: dir.clone(),
        source: e,
    })?;
    let mut out = Vec::new();
    let mut selectors = HashSet::new();
    for entry in entries.flatten() {
        let case_dir = entry.path();
        if !case_dir.is_dir() {
            continue;
        }
        let Some(name) = case_dir.file_name().and_then(|name| name.to_str()) else {
            return Err(CorpusError::InvalidCaseDirectory { path: case_dir });
        };
        let Some((number, slug)) = name.split_once('-') else {
            return Err(CorpusError::InvalidCaseDirectory { path: case_dir });
        };
        if number.len() != 3 || !number.chars().all(char::is_numeric) || slug.is_empty() {
            return Err(CorpusError::InvalidCaseDirectory { path: case_dir });
        }
        let path = case_dir.join("case.toml");
        if !path.is_file() {
            return Err(CorpusError::MissingCaseFile { path: case_dir });
        }
        let mut case = load_case(&path, Some(tier))?;
        if case.id != slug {
            return Err(CorpusError::IdMismatch {
                path,
                file_id: case.id,
                expected_id: slug.to_owned(),
            });
        }
        let fixture = case_dir.join("fixture");
        if tier == BenchTier::T2 && !fixture.is_dir() {
            return Err(CorpusError::MissingFixture { path: case_dir });
        }
        case.selector = format!("{}.{}", tier.as_str(), number);
        if !selectors.insert(case.selector.clone()) {
            return Err(CorpusError::DuplicateSelector {
                path: case_dir,
                selector: case.selector,
            });
        }
        case.case_dir = case_dir.clone();
        case.fixture_dir = fixture.is_dir().then_some(fixture);
        let overlay = case_dir.join("overlay");
        case.test_overlay_dir = overlay.is_dir().then_some(overlay);
        out.push(case);
    }
    out.sort_by(|a, b| a.selector.cmp(&b.selector));
    Ok(out)
}

/// Loads cases from all three tiers. Each tier is loaded independently
/// — a missing tier directory contributes zero cases without error.
///
/// # Errors
///
/// As [`load_tier`].
pub fn load_all(root: &Path) -> Result<Vec<BenchCase>, CorpusError> {
    let mut all = Vec::new();
    for tier in [BenchTier::T1, BenchTier::T2, BenchTier::T3] {
        all.extend(load_tier(root, tier)?);
    }
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_case(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    // ── tier parsing ──────────────────────────────────────────

    #[test]
    fn tier_parses_short_and_long_forms() {
        assert_eq!("t1".parse::<BenchTier>().unwrap(), BenchTier::T1);
        assert_eq!("T2".parse::<BenchTier>().unwrap(), BenchTier::T2);
        assert_eq!("3".parse::<BenchTier>().unwrap(), BenchTier::T3);
    }

    #[test]
    fn tier_unknown_value_errors() {
        assert!("t4".parse::<BenchTier>().is_err());
        assert!("".parse::<BenchTier>().is_err());
    }

    #[test]
    fn tier_round_trips_through_display() {
        for t in [BenchTier::T1, BenchTier::T2, BenchTier::T3] {
            assert_eq!(t.to_string().parse::<BenchTier>().unwrap(), t);
        }
    }

    // ── expected variants ─────────────────────────────────────

    #[test]
    fn expected_exact_round_trips() {
        let exp = BenchExpected::Exact {
            text: "hello".into(),
        };
        let s = toml::to_string(&exp).unwrap();
        assert!(s.contains("kind = \"exact\""));
        let parsed: BenchExpected = toml::from_str(&s).unwrap();
        assert_eq!(parsed, exp);
    }

    #[test]
    fn expected_rubric_round_trips() {
        let exp = BenchExpected::Rubric {
            criteria: vec!["compiles".into(), "tests pass".into()],
        };
        let s = toml::to_string(&exp).unwrap();
        assert!(s.contains("kind = \"rubric\""));
        let parsed: BenchExpected = toml::from_str(&s).unwrap();
        assert_eq!(parsed, exp);
    }

    // ── case loading ──────────────────────────────────────────

    #[test]
    fn load_case_parses_minimal_t1_case() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"
id = "smoke-1"
tier = "t1"
name = "Echoes hello"
description = "Sanity check that the worker echoes its input."
prompt = "Say hello"

[expected]
kind = "exact"
text = "hello"
"#;
        let p = write_case(dir.path(), "smoke-1.toml", body);
        let case = load_case(&p, Some(BenchTier::T1)).unwrap();
        assert_eq!(case.id, "smoke-1");
        assert_eq!(case.tier, BenchTier::T1);
        assert_eq!(case.timeout_s, None, "timeout inherits harness default");
        assert_eq!(case.max_cost_cents, 50, "default cost ceiling applied");
        assert!(case.fixture_dir.is_none());
        match case.expected {
            BenchExpected::Exact { text } => assert_eq!(text, "hello"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn load_case_overrides_for_t2() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"
id = "add-flag"
tier = "t2"
name = "Add --dry-run flag"
description = "Modify the CLI to accept --dry-run."
prompt = "Add a --dry-run flag to the CLI."
timeout_s = 300
max_cost_cents = 200

[tool_policy]
profile = "execute"

[tool_policy.phases.speccing]
allowed_tools = ["read_file", "glob"]

[expected]
kind = "tests_pass"
command = "cargo test"
"#;
        let p = write_case(dir.path(), "add-flag.toml", body);
        let case = load_case(&p, Some(BenchTier::T2)).unwrap();
        assert_eq!(case.tier, BenchTier::T2);
        assert_eq!(case.timeout_s, Some(300));
        assert_eq!(case.max_cost_cents, 200);
        assert!(
            case.fixture_dir.is_none(),
            "direct loading has no local fixture metadata"
        );
        let policy = case.tool_policy.as_ref().unwrap();
        assert_eq!(policy.profile.as_deref(), Some("execute"));
        assert_eq!(
            policy.phases["speccing"].allowed_tools,
            Some(vec!["read_file".into(), "glob".into()])
        );
    }

    #[test]
    fn load_case_tier_mismatch_errors() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"
id = "x"
tier = "t1"
name = "x"
description = "x"
prompt = "x"
[expected]
kind = "exact"
text = "x"
"#;
        let p = write_case(dir.path(), "x.toml", body);
        let err = load_case(&p, Some(BenchTier::T2)).unwrap_err();
        assert!(matches!(err, CorpusError::TierMismatch { .. }));
    }

    #[test]
    fn load_case_rejects_unknown_field() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"
id = "x"
tier = "t1"
name = "x"
description = "x"
prompt = "x"
typo_field = "what"
[expected]
kind = "exact"
text = "x"
"#;
        let p = write_case(dir.path(), "x.toml", body);
        let err = load_case(&p, None).unwrap_err();
        assert!(matches!(err, CorpusError::Parse { .. }));
    }

    // ── tier loaders ──────────────────────────────────────────

    #[test]
    fn load_tier_returns_empty_when_dir_missing() {
        let dir = tempfile::tempdir().unwrap();
        let cases = load_tier(dir.path(), BenchTier::T1).unwrap();
        assert!(cases.is_empty());
    }

    #[test]
    fn load_tier_returns_numbered_cases_in_selector_order() {
        let dir = tempfile::tempdir().unwrap();
        let t1_dir = dir.path().join("t1");
        std::fs::create_dir_all(&t1_dir).unwrap();
        for (number, id) in [("003", "c"), ("001", "a"), ("002", "b")] {
            let case_dir = t1_dir.join(format!("{number}-{id}"));
            std::fs::create_dir_all(&case_dir).unwrap();
            write_case(
                &case_dir,
                "case.toml",
                &format!(
                    r#"
id = "{id}"
tier = "t1"
name = "n"
description = "d"
prompt = "p"
[expected]
kind = "exact"
text = "x"
"#,
                ),
            );
        }
        let cases = load_tier(dir.path(), BenchTier::T1).unwrap();
        assert_eq!(
            cases.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
        assert_eq!(cases[0].selector, "t1.001");
    }

    #[test]
    fn load_tier_skips_non_case_directories() {
        let dir = tempfile::tempdir().unwrap();
        let t1_dir = dir.path().join("t1");
        std::fs::create_dir_all(&t1_dir).unwrap();
        std::fs::write(t1_dir.join("README.md"), "not a case").unwrap();
        let case_dir = t1_dir.join("001-only");
        std::fs::create_dir_all(&case_dir).unwrap();
        write_case(
            &case_dir,
            "case.toml",
            r#"
id = "only"
tier = "t1"
name = "n"
description = "d"
prompt = "p"
[expected]
kind = "exact"
text = "x"
"#,
        );
        let cases = load_tier(dir.path(), BenchTier::T1).unwrap();
        assert_eq!(cases.len(), 1);
    }

    #[test]
    fn load_tier_rejects_t2_case_without_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let case_dir = dir.path().join("t2").join("001-missing-fixture");
        std::fs::create_dir_all(&case_dir).unwrap();
        write_case(
            &case_dir,
            "case.toml",
            r#"
id = "missing-fixture"
tier = "t2"
name = "n"
description = "d"
prompt = "p"
[expected]
kind = "tests_pass"
command = "true"
"#,
        );
        assert!(matches!(
            load_tier(dir.path(), BenchTier::T2),
            Err(CorpusError::MissingFixture { .. })
        ));
    }

    #[test]
    fn load_tier_rejects_slug_id_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let case_dir = dir.path().join("t1").join("001-directory-slug");
        std::fs::create_dir_all(&case_dir).unwrap();
        write_case(
            &case_dir,
            "case.toml",
            r#"
id = "different-id"
tier = "t1"
name = "n"
description = "d"
prompt = "p"
[expected]
kind = "exact"
text = "x"
"#,
        );
        assert!(matches!(
            load_tier(dir.path(), BenchTier::T1),
            Err(CorpusError::IdMismatch { .. })
        ));
    }

    #[test]
    fn load_all_picks_up_all_three_tiers() {
        let dir = tempfile::tempdir().unwrap();
        for (tier, id) in [
            (BenchTier::T1, "a"),
            (BenchTier::T2, "b"),
            (BenchTier::T3, "c"),
        ] {
            let tdir = dir.path().join(tier.as_str()).join(format!("001-{id}"));
            std::fs::create_dir_all(&tdir).unwrap();
            if tier == BenchTier::T2 {
                std::fs::create_dir(tdir.join("fixture")).unwrap();
            }
            write_case(
                &tdir,
                "case.toml",
                &format!(
                    r#"
id = "{id}"
tier = "{tier_str}"
name = "n"
description = "d"
prompt = "p"
[expected]
kind = "exact"
text = "x"
"#,
                    tier_str = tier.as_str(),
                ),
            );
        }
        let all = load_all(dir.path()).unwrap();
        assert_eq!(all.len(), 3);
    }
}

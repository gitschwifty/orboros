//! Benchmark case schema and loader.
//!
//! Cases live as TOML files under `bench/cases/<tier>/<id>.toml`.
//! Loaded eagerly at harness startup.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
    /// Optional seed repo path (T2). Relative to `bench/fixtures/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_repo: Option<PathBuf>,
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
    let case: BenchCase = toml::from_str(&raw).map_err(|e| CorpusError::Parse {
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
    Ok(case)
}

/// Loads all cases under `root/<tier>/`. Skips non-`.toml` files.
/// Returns cases sorted by id for stable iteration.
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
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }
        out.push(load_case(&path, Some(tier))?);
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
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
        assert!(case.seed_repo.is_none());
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
seed_repo = "small-cli"
timeout_s = 300
max_cost_cents = 200

[expected]
kind = "tests_pass"
command = "cargo test"
"#;
        let p = write_case(dir.path(), "add-flag.toml", body);
        let case = load_case(&p, Some(BenchTier::T2)).unwrap();
        assert_eq!(case.tier, BenchTier::T2);
        assert_eq!(case.timeout_s, Some(300));
        assert_eq!(case.max_cost_cents, 200);
        assert_eq!(
            case.seed_repo.as_deref().and_then(Path::to_str),
            Some("small-cli")
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
    fn load_tier_returns_sorted_cases() {
        let dir = tempfile::tempdir().unwrap();
        let t1_dir = dir.path().join("t1");
        std::fs::create_dir_all(&t1_dir).unwrap();
        for id in ["c", "a", "b"] {
            write_case(
                &t1_dir,
                &format!("{id}.toml"),
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
    }

    #[test]
    fn load_tier_skips_non_toml() {
        let dir = tempfile::tempdir().unwrap();
        let t1_dir = dir.path().join("t1");
        std::fs::create_dir_all(&t1_dir).unwrap();
        std::fs::write(t1_dir.join("README.md"), "not a case").unwrap();
        write_case(
            &t1_dir,
            "only.toml",
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
    fn load_all_picks_up_all_three_tiers() {
        let dir = tempfile::tempdir().unwrap();
        for (tier, id) in [
            (BenchTier::T1, "a"),
            (BenchTier::T2, "b"),
            (BenchTier::T3, "c"),
        ] {
            let tdir = dir.path().join(tier.as_str());
            std::fs::create_dir_all(&tdir).unwrap();
            write_case(
                &tdir,
                &format!("{id}.toml"),
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

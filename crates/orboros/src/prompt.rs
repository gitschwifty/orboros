use std::path::{Path, PathBuf};

use crate::config::{PromptConfig, PromptOverride};

pub const RESEARCH_WORKER_SYSTEM_PROMPT: &str = r"You are a research worker for Orboros, a software development orchestrator.

Your job is to investigate the task and produce a concise, useful summary. Work read-only unless the user prompt explicitly asks for code examples.

Constraints:
- Do not modify files.
- Read relevant project context before drawing conclusions.
- Cite file paths and line numbers when referring to local code.
- Distinguish facts from assumptions and unresolved questions.

Output: A structured summary that directly answers the task. Use short sections and end with open questions when any remain.";

pub const EDIT_WORKER_SYSTEM_PROMPT: &str = r"You are an edit worker for Orboros, a software development orchestrator.

Your job is to implement the requested code change end to end in a single turn: inspect the relevant code, make the change, update or add tests, and verify the result.

Constraints:
- Read the relevant code before editing.
- Keep changes scoped to the task.
- Follow the project's existing style and abstractions.
- Run the most relevant tests and fix failures caused by your change.

Output: A brief implementation summary, the tests run, and any remaining risk.";

pub const REVIEW_WORKER_SYSTEM_PROMPT: &str = r"You are a review worker for Orboros, a software development orchestrator.

Your job is to review code, a design, or an implementation result against the task description and acceptance criteria. You are read-only.

Constraints:
- Do not modify files.
- Prioritize correctness bugs, regressions, missing edge cases, spec mismatches, and test gaps.
- Separate blocking issues from non-blocking suggestions.
- Ground findings in concrete file paths, line numbers, or observed behavior when available.

Output: A structured review with findings first, then open questions, then a concise verdict.";

pub const TEST_WORKER_SYSTEM_PROMPT: &str = r"You are a test worker for Orboros, a software development orchestrator.

Your job is to verify behavior against the task description and acceptance criteria by writing, updating, and running focused tests.

Constraints:
- Prefer test changes over implementation changes.
- Do not modify implementation code unless the task explicitly asks for it.
- Cover important edge cases and failure paths, not only happy paths.
- Report exact test commands and outcomes.

Output: A summary of tests added or run, pass/fail results, and any issues discovered.";

pub const PLAN_WORKER_SYSTEM_PROMPT: &str = r"You are a planning worker for Orboros, a software development orchestrator.

Your job is to turn ambiguous or high-level work into a concrete execution plan that other workers can follow.

Constraints:
- Do not implement the plan.
- Identify dependencies, sequencing, risks, and validation steps.
- Keep tasks small enough for focused worker execution.
- Call out assumptions that materially affect the plan.

Output: A concise ordered plan with validation steps and open questions.";

pub const EXECUTE_WORKER_SYSTEM_PROMPT: &str = r"You are an execution worker for Orboros, a software development orchestrator.

Your job is to complete the task described in the user message and return the best finished result you can produce in one turn.

Constraints:
- Use the task description and any provided acceptance criteria as the source of truth.
- Keep work scoped to the task.
- Verify the result when verification is possible.
- Surface blockers clearly instead of guessing.

Output: The completed result, followed by a short verification note when applicable.";

pub const DEFAULT_WORKER_SYSTEM_PROMPT: &str = r"You are a focused worker for Orboros, a software development orchestrator.

Complete the task described in the user message. Keep the work scoped, verify the result when possible, and report any blockers clearly.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind<'a> {
    Worker(&'a str),
    Coordinator(&'a str),
    Phase(&'a str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPrompt {
    pub system_prompt: String,
    pub source: PromptSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptSource {
    BuiltIn,
    ConfigInline { key: String },
    ConfigFile { key: String, path: PathBuf },
    CliInline,
    CliFile { path: PathBuf },
}

impl PromptSource {
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::BuiltIn => "built_in".to_string(),
            Self::ConfigInline { key } => format!("config_inline:{key}"),
            Self::ConfigFile { key, path } => {
                format!("config_file:{key}:{}", path.display())
            }
            Self::CliInline => "cli_inline".to_string(),
            Self::CliFile { path } => format!("cli_file:{}", path.display()),
        }
    }
}

impl PromptKind<'_> {
    #[must_use]
    pub fn category(&self) -> String {
        match self {
            Self::Worker(worker_type) => format!("worker.{worker_type}"),
            Self::Coordinator(coordinator) => format!("coordinator.{coordinator}"),
            Self::Phase(phase) => format!("phase.{phase}"),
        }
    }
}

/// SHA-256 of prompt text, hex-encoded. Used to attribute worker
/// results and benchmark rows to the exact prompt content that
/// produced them.
#[must_use]
pub fn prompt_hash(prompt: &str) -> String {
    use sha2::{Digest, Sha256};

    let mut h = Sha256::new();
    h.update(prompt.as_bytes());
    let digest = h.finalize();
    digest.iter().fold(String::with_capacity(64), |mut acc, b| {
        use std::fmt::Write;
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

#[derive(Debug, Clone, Default)]
pub struct PromptResolver {
    config: PromptConfig,
    home: Option<PathBuf>,
    project_dir: Option<PathBuf>,
    cli_override: Option<ResolvedPrompt>,
}

#[must_use]
pub fn built_in_worker_system_prompt(worker_type: &str) -> &'static str {
    match worker_type {
        "research" => RESEARCH_WORKER_SYSTEM_PROMPT,
        "edit" => EDIT_WORKER_SYSTEM_PROMPT,
        "review" => REVIEW_WORKER_SYSTEM_PROMPT,
        "test" => TEST_WORKER_SYSTEM_PROMPT,
        "plan" => PLAN_WORKER_SYSTEM_PROMPT,
        "execute" => EXECUTE_WORKER_SYSTEM_PROMPT,
        _ => DEFAULT_WORKER_SYSTEM_PROMPT,
    }
}

impl PromptResolver {
    #[must_use]
    pub fn new(config: PromptConfig, home: Option<PathBuf>, project_dir: Option<PathBuf>) -> Self {
        Self {
            config,
            home,
            project_dir,
            cli_override: None,
        }
    }

    #[must_use]
    pub fn from_config(config: PromptConfig, project_dir: Option<&Path>) -> Self {
        Self::new(config, dirs::home_dir(), project_dir.map(Path::to_path_buf))
    }

    #[must_use]
    pub fn with_cli_override(mut self, cli_override: Option<ResolvedPrompt>) -> Self {
        self.cli_override = cli_override;
        self
    }

    /// Resolve a system prompt for a phase or worker type.
    ///
    /// Lookup order is specific key, then `[prompts.default]`, then the
    /// caller-provided built-in prompt. `system_file` wins over `system`
    /// when both are set so file-backed prompts can override inline
    /// defaults in layered config.
    ///
    /// # Errors
    ///
    /// Returns an error when a configured prompt file cannot be read.
    pub fn resolve_system_prompt(
        &self,
        kind: PromptKind<'_>,
        built_in: &str,
    ) -> anyhow::Result<ResolvedPrompt> {
        if let Some(cli_override) = &self.cli_override {
            return Ok(cli_override.clone());
        }

        let (key, specific) = match kind {
            PromptKind::Worker(worker_type) => (
                format!("workers.{worker_type}"),
                self.config.workers.get(worker_type),
            ),
            PromptKind::Coordinator(coordinator) => (
                format!("coordinators.{coordinator}"),
                self.config.coordinators.get(coordinator),
            ),
            PromptKind::Phase(phase) => (format!("phases.{phase}"), self.config.phases.get(phase)),
        };

        if let Some(resolved) = self.resolve_override(&key, specific)? {
            return Ok(resolved);
        }
        if let PromptKind::Coordinator(coordinator) = kind {
            let legacy_key = format!("workers.{coordinator}");
            if let Some(resolved) =
                self.resolve_override(&legacy_key, self.config.workers.get(coordinator))?
            {
                return Ok(resolved);
            }
        }
        if let Some(resolved) = self.resolve_override("default", Some(&self.config.default))? {
            return Ok(resolved);
        }
        Ok(ResolvedPrompt {
            system_prompt: built_in.to_string(),
            source: PromptSource::BuiltIn,
        })
    }

    fn resolve_override(
        &self,
        key: &str,
        override_cfg: Option<&PromptOverride>,
    ) -> anyhow::Result<Option<ResolvedPrompt>> {
        let Some(override_cfg) = override_cfg else {
            return Ok(None);
        };

        if let Some(path) = &override_cfg.system_file {
            let resolved_path = self.resolve_prompt_path(path);
            let system_prompt = std::fs::read_to_string(&resolved_path).map_err(|e| {
                anyhow::anyhow!(
                    "failed to read prompt file for prompts.{key} at {}: {e}",
                    resolved_path.display()
                )
            })?;
            return Ok(Some(ResolvedPrompt {
                system_prompt,
                source: PromptSource::ConfigFile {
                    key: key.to_string(),
                    path: resolved_path,
                },
            }));
        }

        Ok(override_cfg.system.as_ref().map(|system| ResolvedPrompt {
            system_prompt: system.clone(),
            source: PromptSource::ConfigInline {
                key: key.to_string(),
            },
        }))
    }

    fn resolve_prompt_path(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            return path.to_path_buf();
        }

        if let Some(project_dir) = &self.project_dir {
            let candidate = project_dir.join(".orbs").join(path);
            if candidate.exists() {
                return candidate;
            }
        }

        if let Some(home) = &self.home {
            let candidate = home.join(".orboros").join(path);
            if candidate.exists() {
                return candidate;
            }
        }

        self.project_dir.as_ref().map_or_else(
            || path.to_path_buf(),
            |project_dir| project_dir.join(".orbs").join(path),
        )
    }
}

/// Resolves an explicit CLI system-prompt override.
///
/// `system_file` is read exactly as provided, relative to the current
/// process directory when not absolute. Config-backed prompt files use
/// [`PromptResolver`]'s project/global lookup rules instead.
///
/// # Errors
///
/// Returns an error when both override forms are provided or when the
/// prompt file cannot be read.
pub fn resolve_cli_system_prompt(
    system: Option<&str>,
    system_file: Option<&Path>,
) -> anyhow::Result<Option<ResolvedPrompt>> {
    match (system, system_file) {
        (Some(_), Some(_)) => {
            anyhow::bail!("use either --system-prompt or --system-prompt-file, not both")
        }
        (Some(system), None) => Ok(Some(ResolvedPrompt {
            system_prompt: system.to_string(),
            source: PromptSource::CliInline,
        })),
        (None, Some(path)) => {
            let system_prompt = std::fs::read_to_string(path).map_err(|e| {
                anyhow::anyhow!(
                    "failed to read --system-prompt-file {}: {e}",
                    path.display()
                )
            })?;
            Ok(Some(ResolvedPrompt {
                system_prompt,
                source: PromptSource::CliFile {
                    path: path.to_path_buf(),
                },
            }))
        }
        (None, None) => Ok(None),
    }
}

#[must_use]
pub fn with_confidence_addendum(system_prompt: &str) -> String {
    format!(
        "{system_prompt}{}",
        crate::worker::process::CONFIDENCE_PROMPT_ADDENDUM
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tempfile::tempdir;

    use super::*;
    use crate::config::PromptOverride;

    #[test]
    fn resolves_builtin_when_no_override_exists() {
        let resolver = PromptResolver::default();
        let answer = resolver
            .resolve_system_prompt(
                PromptKind::Worker("edit"),
                built_in_worker_system_prompt("edit"),
            )
            .unwrap();

        assert_eq!(answer.system_prompt, EDIT_WORKER_SYSTEM_PROMPT);
        assert_eq!(answer.source, PromptSource::BuiltIn);
    }

    #[test]
    fn built_in_worker_prompts_are_role_specific() {
        assert_ne!(
            built_in_worker_system_prompt("research"),
            built_in_worker_system_prompt("edit")
        );
        assert_ne!(
            built_in_worker_system_prompt("edit"),
            built_in_worker_system_prompt("review")
        );
        assert_ne!(
            built_in_worker_system_prompt("review"),
            built_in_worker_system_prompt("test")
        );
        assert_ne!(
            built_in_worker_system_prompt("plan"),
            built_in_worker_system_prompt("execute")
        );
        assert!(built_in_worker_system_prompt("research").contains("Do not modify files"));
        assert!(built_in_worker_system_prompt("edit").contains("implement"));
        assert!(built_in_worker_system_prompt("review").contains("read-only"));
        assert!(built_in_worker_system_prompt("test").contains("tests"));
    }

    #[test]
    fn prompt_hash_is_stable_and_content_derived() {
        let a = prompt_hash("same prompt");
        let b = prompt_hash("same prompt");
        let c = prompt_hash("different prompt");

        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn prompt_kind_category_is_persistable() {
        assert_eq!(PromptKind::Worker("edit").category(), "worker.edit");
        assert_eq!(
            PromptKind::Coordinator("aggregate").category(),
            "coordinator.aggregate"
        );
        assert_eq!(PromptKind::Phase("speccing").category(), "phase.speccing");
    }

    #[test]
    fn prompt_source_label_identifies_origin() {
        assert_eq!(PromptSource::BuiltIn.label(), "built_in");
        assert_eq!(PromptSource::CliInline.label(), "cli_inline");
        assert_eq!(
            PromptSource::CliFile {
                path: PathBuf::from("prompt.md")
            }
            .label(),
            "cli_file:prompt.md"
        );
        assert_eq!(
            PromptSource::ConfigInline {
                key: "workers.edit".into()
            }
            .label(),
            "config_inline:workers.edit"
        );
        assert_eq!(
            PromptSource::ConfigFile {
                key: "phases.review".into(),
                path: PathBuf::from("prompts/review.md")
            }
            .label(),
            "config_file:phases.review:prompts/review.md"
        );
    }

    #[test]
    fn cli_override_wins_over_specific_config() {
        let mut workers = BTreeMap::new();
        workers.insert(
            "edit".into(),
            PromptOverride {
                system: Some("config edit".into()),
                system_file: None,
            },
        );
        let resolver = PromptResolver::new(
            PromptConfig {
                workers,
                ..PromptConfig::default()
            },
            None,
            None,
        )
        .with_cli_override(Some(ResolvedPrompt {
            system_prompt: "cli prompt".into(),
            source: PromptSource::CliInline,
        }));

        let answer = resolver
            .resolve_system_prompt(PromptKind::Worker("edit"), "built in")
            .unwrap();

        assert_eq!(answer.system_prompt, "cli prompt");
        assert_eq!(answer.source, PromptSource::CliInline);
    }

    #[test]
    fn resolves_cli_prompt_file_exactly() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("prompt.md");
        std::fs::write(&path, "from file").unwrap();

        let answer = resolve_cli_system_prompt(None, Some(&path))
            .unwrap()
            .unwrap();

        assert_eq!(answer.system_prompt, "from file");
        assert_eq!(answer.source, PromptSource::CliFile { path });
    }

    #[test]
    fn cli_prompt_rejects_inline_and_file_together() {
        let err = resolve_cli_system_prompt(Some("inline"), Some(Path::new("prompt.md")))
            .unwrap_err()
            .to_string();
        assert!(err.contains("either --system-prompt or --system-prompt-file"));
    }

    #[test]
    fn worker_override_wins_over_default() {
        let config = PromptConfig {
            default: PromptOverride {
                system: Some("default prompt".into()),
                system_file: None,
            },
            workers: [(
                "edit".into(),
                PromptOverride {
                    system: Some("edit prompt".into()),
                    system_file: None,
                },
            )]
            .into(),
            coordinators: BTreeMap::new(),
            phases: BTreeMap::new(),
        };
        let resolver = PromptResolver::new(config, None, None);

        let answer = resolver
            .resolve_system_prompt(PromptKind::Worker("edit"), "built in")
            .unwrap();

        assert_eq!(answer.system_prompt, "edit prompt");
        assert_eq!(
            answer.source,
            PromptSource::ConfigInline {
                key: "workers.edit".into()
            }
        );
    }

    #[test]
    fn coordinator_override_wins_over_legacy_worker_key() {
        let config = PromptConfig {
            workers: [(
                "decompose".into(),
                PromptOverride {
                    system: Some("legacy decompose".into()),
                    system_file: None,
                },
            )]
            .into(),
            coordinators: [(
                "decompose".into(),
                PromptOverride {
                    system: Some("coordinator decompose".into()),
                    system_file: None,
                },
            )]
            .into(),
            ..Default::default()
        };
        let resolver = PromptResolver::new(config, None, None);

        let answer = resolver
            .resolve_system_prompt(PromptKind::Coordinator("decompose"), "built in")
            .unwrap();

        assert_eq!(answer.system_prompt, "coordinator decompose");
        assert_eq!(
            answer.source,
            PromptSource::ConfigInline {
                key: "coordinators.decompose".into()
            }
        );
    }

    #[test]
    fn coordinator_prompt_falls_back_to_legacy_worker_key() {
        let config = PromptConfig {
            workers: [(
                "aggregate".into(),
                PromptOverride {
                    system: Some("legacy aggregate".into()),
                    system_file: None,
                },
            )]
            .into(),
            ..Default::default()
        };
        let resolver = PromptResolver::new(config, None, None);

        let answer = resolver
            .resolve_system_prompt(PromptKind::Coordinator("aggregate"), "built in")
            .unwrap();

        assert_eq!(answer.system_prompt, "legacy aggregate");
        assert_eq!(
            answer.source,
            PromptSource::ConfigInline {
                key: "workers.aggregate".into()
            }
        );
    }

    #[test]
    fn default_applies_to_unknown_worker_type() {
        let config = PromptConfig {
            default: PromptOverride {
                system: Some("default prompt".into()),
                system_file: None,
            },
            ..Default::default()
        };
        let resolver = PromptResolver::new(config, None, None);

        let answer = resolver
            .resolve_system_prompt(PromptKind::Worker("research"), "built in")
            .unwrap();

        assert_eq!(answer.system_prompt, "default prompt");
    }

    #[test]
    fn phase_prompt_can_load_relative_project_file() {
        let project = tempdir().unwrap();
        let prompt_dir = project.path().join(".orbs").join("prompts");
        std::fs::create_dir_all(&prompt_dir).unwrap();
        std::fs::write(prompt_dir.join("speccing.md"), "project speccing").unwrap();
        let config = PromptConfig {
            phases: [(
                "speccing".into(),
                PromptOverride {
                    system: None,
                    system_file: Some("prompts/speccing.md".into()),
                },
            )]
            .into(),
            ..Default::default()
        };
        let resolver = PromptResolver::new(config, None, Some(project.path().to_path_buf()));

        let answer = resolver
            .resolve_system_prompt(PromptKind::Phase("speccing"), "built in")
            .unwrap();

        assert_eq!(answer.system_prompt, "project speccing");
        assert!(matches!(answer.source, PromptSource::ConfigFile { .. }));
    }

    #[test]
    fn relative_file_falls_back_to_global_prompt_dir() {
        let home = tempdir().unwrap();
        let prompt_dir = home.path().join(".orboros").join("prompts");
        std::fs::create_dir_all(&prompt_dir).unwrap();
        std::fs::write(prompt_dir.join("review.md"), "global review").unwrap();
        let config = PromptConfig {
            workers: [(
                "review".into(),
                PromptOverride {
                    system: None,
                    system_file: Some("prompts/review.md".into()),
                },
            )]
            .into(),
            ..Default::default()
        };
        let resolver = PromptResolver::new(config, Some(home.path().to_path_buf()), None);

        let answer = resolver
            .resolve_system_prompt(PromptKind::Worker("review"), "built in")
            .unwrap();

        assert_eq!(answer.system_prompt, "global review");
    }
}

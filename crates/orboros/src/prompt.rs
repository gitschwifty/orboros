use std::path::{Path, PathBuf};

use crate::config::{PromptConfig, PromptOverride};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind<'a> {
    Worker(&'a str),
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
}

#[derive(Debug, Clone, Default)]
pub struct PromptResolver {
    config: PromptConfig,
    home: Option<PathBuf>,
    project_dir: Option<PathBuf>,
}

impl PromptResolver {
    #[must_use]
    pub fn new(config: PromptConfig, home: Option<PathBuf>, project_dir: Option<PathBuf>) -> Self {
        Self {
            config,
            home,
            project_dir,
        }
    }

    #[must_use]
    pub fn from_config(config: PromptConfig, project_dir: Option<&Path>) -> Self {
        Self::new(config, dirs::home_dir(), project_dir.map(Path::to_path_buf))
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
        let (key, specific) = match kind {
            PromptKind::Worker(worker_type) => (
                format!("workers.{worker_type}"),
                self.config.workers.get(worker_type),
            ),
            PromptKind::Phase(phase) => (format!("phases.{phase}"), self.config.phases.get(phase)),
        };

        if let Some(resolved) = self.resolve_override(&key, specific)? {
            return Ok(resolved);
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
            .resolve_system_prompt(PromptKind::Worker("edit"), "built in")
            .unwrap();

        assert_eq!(answer.system_prompt, "built in");
        assert_eq!(answer.source, PromptSource::BuiltIn);
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

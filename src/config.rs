use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// OrbConfig — layered config (global → project → CLI)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct OrbConfig {
    pub default_model: String,
    pub max_concurrency: usize,
    pub worker_binary: Option<String>,
    pub models: ModelConfig,
    pub bench: BenchConfig,
    pub prompts: PromptConfig,
    pub review: ReviewConfig,
    pub second_opinion: SecondOpinionConfig,
    pub notification: NotificationConfig,
}

impl Default for OrbConfig {
    fn default() -> Self {
        Self {
            default_model: "openrouter/free".to_string(),
            max_concurrency: 4,
            worker_binary: None,
            models: ModelConfig::default(),
            bench: BenchConfig::default(),
            prompts: PromptConfig::default(),
            review: ReviewConfig::default(),
            second_opinion: SecondOpinionConfig::default(),
            notification: NotificationConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct BenchConfig {
    pub timeout_s: Option<u32>,
    pub max_iterations: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ModelConfig {
    pub default: ModelDefaults,
    pub options: BTreeMap<String, ModelOption>,
    pub workers: BTreeMap<String, String>,
    pub coordinators: BTreeMap<String, String>,
    pub phases: BTreeMap<String, String>,
    pub bench: BenchModelConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ModelDefaults {
    pub worker: Option<String>,
    pub coordinator: Option<String>,
    pub phase: Option<String>,
    pub reviewer: Option<String>,
    pub bench: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct BenchModelConfig {
    pub default: Option<String>,
    pub grader: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ModelOption {
    pub model: String,
    pub description: Option<String>,
    pub provider: Option<String>,
    pub router: Option<String>,
    pub reasoning: Option<String>,
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelRole<'a> {
    Worker(&'a str),
    Coordinator(&'a str),
    Phase(&'a str),
    Reviewer,
    BenchDefault,
    BenchGrader,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModel {
    pub key: Option<String>,
    pub model: String,
    pub description: Option<String>,
    pub provider: Option<String>,
    pub router: Option<String>,
    pub reasoning: Option<String>,
    pub effort: Option<String>,
    pub source: String,
}

pub struct ModelResolver<'a> {
    config: &'a OrbConfig,
}

impl ModelConfig {
    /// Validates catalog references. Raw `provider/model` selectors remain
    /// allowed for compatibility with the existing `default_model` surface.
    ///
    /// # Errors
    ///
    /// Returns an error when a named selector points at no catalog option, or
    /// a catalog option has an empty model string.
    pub fn validate(&self) -> Result<(), String> {
        for (key, option) in &self.options {
            if option.model.trim().is_empty() {
                return Err(format!("models.options.{key}.model must not be empty"));
            }
        }

        for (path, selector) in self.selectors() {
            self.validate_selector(&path, selector)?;
        }

        Ok(())
    }

    fn selectors(&self) -> Vec<(String, &str)> {
        let mut selectors = Vec::new();

        for (path, selector) in [
            ("models.default.worker", self.default.worker.as_deref()),
            (
                "models.default.coordinator",
                self.default.coordinator.as_deref(),
            ),
            ("models.default.phase", self.default.phase.as_deref()),
            ("models.default.reviewer", self.default.reviewer.as_deref()),
            ("models.default.bench", self.default.bench.as_deref()),
            ("models.bench.default", self.bench.default.as_deref()),
            ("models.bench.grader", self.bench.grader.as_deref()),
        ] {
            if let Some(selector) = selector {
                selectors.push((path.to_string(), selector));
            }
        }

        selectors.extend(
            self.workers
                .iter()
                .map(|(key, selector)| (format!("models.workers.{key}"), selector.as_str())),
        );
        selectors.extend(
            self.coordinators
                .iter()
                .map(|(key, selector)| (format!("models.coordinators.{key}"), selector.as_str())),
        );
        selectors.extend(
            self.phases
                .iter()
                .map(|(key, selector)| (format!("models.phases.{key}"), selector.as_str())),
        );

        selectors
    }

    fn validate_selector(&self, path: &str, selector: &str) -> Result<(), String> {
        if selector.trim().is_empty() {
            return Err(format!("{path} must not be empty"));
        }
        if !selector.contains('/') && !self.options.contains_key(selector) {
            return Err(format!(
                "{path} references unknown model option `{selector}`"
            ));
        }
        Ok(())
    }
}

impl OrbConfig {
    #[must_use]
    pub fn model_resolver(&self) -> ModelResolver<'_> {
        ModelResolver { config: self }
    }
}

impl ModelResolver<'_> {
    /// Resolves a model role to the configured catalog option or legacy raw
    /// model string.
    ///
    /// # Errors
    ///
    /// Returns an error when a selector names an unknown catalog option.
    pub fn resolve(&self, role: ModelRole<'_>) -> anyhow::Result<ResolvedModel> {
        let models = &self.config.models;
        let (selector, source) = match role {
            ModelRole::Worker(worker_type) => models
                .workers
                .get(worker_type)
                .map(|selector| (selector.as_str(), format!("models.workers.{worker_type}")))
                .or_else(|| {
                    models
                        .default
                        .worker
                        .as_deref()
                        .map(|selector| (selector, "models.default.worker".to_string()))
                }),
            ModelRole::Coordinator(name) => models
                .coordinators
                .get(name)
                .map(|selector| (selector.as_str(), format!("models.coordinators.{name}")))
                .or_else(|| {
                    models
                        .default
                        .coordinator
                        .as_deref()
                        .map(|selector| (selector, "models.default.coordinator".to_string()))
                }),
            ModelRole::Phase(name) => models
                .phases
                .get(name)
                .map(|selector| (selector.as_str(), format!("models.phases.{name}")))
                .or_else(|| {
                    models
                        .default
                        .phase
                        .as_deref()
                        .map(|selector| (selector, "models.default.phase".to_string()))
                }),
            ModelRole::Reviewer => self
                .config
                .second_opinion
                .reviewer_model
                .as_deref()
                .map(|selector| (selector, "second_opinion.reviewer_model".to_string()))
                .or_else(|| {
                    models
                        .default
                        .reviewer
                        .as_deref()
                        .map(|selector| (selector, "models.default.reviewer".to_string()))
                }),
            ModelRole::BenchDefault => models
                .bench
                .default
                .as_deref()
                .map(|selector| (selector, "models.bench.default".to_string()))
                .or_else(|| {
                    models
                        .default
                        .bench
                        .as_deref()
                        .map(|selector| (selector, "models.default.bench".to_string()))
                }),
            ModelRole::BenchGrader => models
                .bench
                .grader
                .as_deref()
                .map(|selector| (selector, "models.bench.grader".to_string()))
                .or_else(|| {
                    models
                        .default
                        .bench
                        .as_deref()
                        .map(|selector| (selector, "models.default.bench".to_string()))
                }),
        }
        .unwrap_or((&self.config.default_model, "default_model".to_string()));

        self.resolve_selector(selector, source)
    }

    /// Resolves a specific selector to a model.
    ///
    /// # Errors
    ///
    /// Returns an error when `selector` is neither a known catalog key nor a
    /// raw `provider/model` string.
    pub fn resolve_selector(
        &self,
        selector: &str,
        source: String,
    ) -> anyhow::Result<ResolvedModel> {
        if let Some(option) = self.config.models.options.get(selector) {
            let provider = option
                .provider
                .clone()
                .or_else(|| infer_provider(&option.model));
            return Ok(ResolvedModel {
                key: Some(selector.to_string()),
                model: option.model.clone(),
                description: option.description.clone(),
                provider,
                router: option.router.clone().or(Some("openrouter".to_string())),
                reasoning: option.reasoning.clone(),
                effort: option.effort.clone(),
                source,
            });
        }

        if selector.contains('/') {
            return Ok(ResolvedModel {
                key: None,
                model: selector.to_string(),
                description: None,
                provider: infer_provider(selector),
                router: None,
                reasoning: None,
                effort: None,
                source,
            });
        }

        anyhow::bail!("unknown model option `{selector}` referenced by {source}");
    }
}

fn infer_provider(model: &str) -> Option<String> {
    model
        .split_once('/')
        .and_then(|(provider, _)| (!provider.is_empty()).then(|| provider.to_string()))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PromptConfig {
    pub default: PromptOverride,
    pub workers: BTreeMap<String, PromptOverride>,
    pub coordinators: BTreeMap<String, PromptOverride>,
    pub phases: BTreeMap<String, PromptOverride>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PromptOverride {
    pub system: Option<String>,
    pub system_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ReviewConfig {
    pub requires_approval_by_default: bool,
    pub review_on_completion: bool,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            requires_approval_by_default: false,
            review_on_completion: true,
        }
    }
}

/// How to trigger the automated second-opinion reviewer (task 58).
///
/// Distinct from [`ReviewConfig`] which governs the human-in-the-loop
/// review checkpoints. The second-opinion reviewer runs after an orb
/// reaches `Done` and emits an automated verdict (Accept / Reject /
/// Revise) without blocking the human.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SecondOpinionMode {
    /// Reviewer never runs.
    #[default]
    Off,
    /// Reviewer always runs after `Done`.
    Always,
    /// Reviewer runs when `confidence < confidence_threshold`.
    Confidence,
    /// Reviewer runs on a random sample (`sampling_rate` fraction
    /// of completed orbs).
    Sampling,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct SecondOpinionConfig {
    /// Trigger mode (see [`SecondOpinionMode`]).
    pub mode: SecondOpinionMode,
    /// Confidence threshold used when `mode = "confidence"`.
    /// Orbs with `confidence < threshold` are routed for review.
    pub confidence_threshold: f32,
    /// Fraction of completed orbs to review when `mode = "sampling"`,
    /// in `[0.0, 1.0]`.
    pub sampling_rate: f32,
    /// Reviewer model identifier. When `None`, falls back to the
    /// project's `default_model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_model: Option<String>,
}

impl Default for SecondOpinionConfig {
    fn default() -> Self {
        Self {
            mode: SecondOpinionMode::Off,
            confidence_threshold: 0.7,
            sampling_rate: 0.1,
            reviewer_model: None,
        }
    }
}

impl SecondOpinionConfig {
    /// Validates the config values, returning an error if any field is
    /// out of range. Called at config load time so misconfiguration
    /// surfaces immediately rather than at trigger evaluation.
    ///
    /// # Errors
    ///
    /// Returns an error if `confidence_threshold` or `sampling_rate`
    /// are outside `[0.0, 1.0]` or non-finite.
    pub fn validate(&self) -> Result<(), String> {
        if !self.confidence_threshold.is_finite()
            || !(0.0..=1.0).contains(&self.confidence_threshold)
        {
            return Err(format!(
                "second_opinion.confidence_threshold must be in [0.0, 1.0]; got {}",
                self.confidence_threshold
            ));
        }
        if !self.sampling_rate.is_finite() || !(0.0..=1.0).contains(&self.sampling_rate) {
            return Err(format!(
                "second_opinion.sampling_rate must be in [0.0, 1.0]; got {}",
                self.sampling_rate
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct NotificationConfig {
    pub enabled: bool,
    pub desktop_enabled: bool,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            desktop_enabled: false,
        }
    }
}

/// Merge two configs: values from `overlay` override `base` when they differ
/// from defaults. We use TOML-level merging: parse both as tables, then
/// overlay non-default fields.
fn merge_toml_tables(base: &mut toml::value::Table, overlay: &toml::value::Table) {
    for (key, value) in overlay {
        match (base.get_mut(key), value) {
            (Some(toml::Value::Table(base_sub)), toml::Value::Table(overlay_sub)) => {
                merge_toml_tables(base_sub, overlay_sub);
            }
            _ => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

/// Load config with hierarchy: global (`~/.orboros/config.toml`) then project
/// (`.orbs/config.toml` relative to `project_dir`). Fields in the project
/// config override global ones.
///
/// # Errors
/// Returns an error if a config file exists but cannot be parsed.
pub fn load_config(project_dir: Option<&Path>) -> anyhow::Result<OrbConfig> {
    load_config_with_home(dirs::home_dir().as_deref(), project_dir)
}

/// Load normal config, then overlay benchmark config after project config.
///
/// If `bench_config_path` is provided it must exist. Otherwise
/// `<bench_root>/config.toml` is loaded when present and ignored when absent.
///
/// # Errors
/// Returns an error if any config file exists but cannot be read or parsed.
pub fn load_config_with_bench(
    project_dir: Option<&Path>,
    bench_root: &Path,
    bench_config_path: Option<&Path>,
) -> anyhow::Result<(OrbConfig, Option<PathBuf>)> {
    let resolved_bench_config = match bench_config_path {
        Some(path) => {
            if !path.exists() {
                anyhow::bail!("bench config not found: {}", path.display());
            }
            Some(path.to_path_buf())
        }
        None => {
            let default_path = bench_root.join("config.toml");
            default_path.exists().then_some(default_path)
        }
    };

    let cfg = load_config_with_home_and_bench(
        dirs::home_dir().as_deref(),
        project_dir,
        resolved_bench_config.as_deref(),
    )?;
    Ok((cfg, resolved_bench_config))
}

/// Testable version that accepts a custom home directory.
pub(crate) fn load_config_with_home(
    home: Option<&Path>,
    project_dir: Option<&Path>,
) -> anyhow::Result<OrbConfig> {
    load_config_with_home_and_bench(home, project_dir, None)
}

pub(crate) fn load_config_with_home_and_bench(
    home: Option<&Path>,
    project_dir: Option<&Path>,
    bench_config_path: Option<&Path>,
) -> anyhow::Result<OrbConfig> {
    let mut base_table = toml::value::Table::new();

    // 1. Global config
    if let Some(home) = home {
        let global_path = home.join(".orboros").join("config.toml");
        if global_path.exists() {
            let content = std::fs::read_to_string(&global_path)?;
            let table: toml::value::Table = toml::from_str(&content)?;
            base_table = table;
        }
    }

    // 2. Project config
    if let Some(dir) = project_dir {
        let project_path = dir.join(".orbs").join("config.toml");
        if project_path.exists() {
            let content = std::fs::read_to_string(&project_path)?;
            let table: toml::value::Table = toml::from_str(&content)?;
            merge_toml_tables(&mut base_table, &table);
        }
    }

    // 3. Benchmark corpus config
    if let Some(path) = bench_config_path {
        let content = std::fs::read_to_string(path)?;
        let table: toml::value::Table = toml::from_str(&content)?;
        merge_toml_tables(&mut base_table, &table);
    }

    let config: OrbConfig = toml::Value::Table(base_table).try_into()?;
    config
        .second_opinion
        .validate()
        .map_err(|e| anyhow::anyhow!(e))?;
    config.models.validate().map_err(|e| anyhow::anyhow!(e))?;
    Ok(config)
}

// ---------------------------------------------------------------------------
// Projects registry — ~/.orboros/projects.toml
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectEntry {
    pub name: String,
    pub path: PathBuf,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ProjectsFile {
    #[serde(default)]
    projects: Vec<ProjectEntry>,
}

fn projects_path(home: &Path) -> PathBuf {
    home.join(".orboros").join("projects.toml")
}

fn load_projects_file(home: &Path) -> anyhow::Result<ProjectsFile> {
    let path = projects_path(home);
    if !path.exists() {
        return Ok(ProjectsFile::default());
    }
    let content = std::fs::read_to_string(&path)?;
    let pf: ProjectsFile = toml::from_str(&content)?;
    Ok(pf)
}

fn save_projects_file(home: &Path, pf: &ProjectsFile) -> anyhow::Result<()> {
    let path = projects_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = toml::to_string_pretty(pf)?;
    std::fs::write(&path, content)?;
    Ok(())
}

/// Register a project. If a project with the same name exists, update its path.
///
/// # Errors
/// Returns an error if the projects file cannot be read or written.
pub fn register_project(home: &Path, name: &str, path: &Path) -> anyhow::Result<ProjectEntry> {
    let mut pf = load_projects_file(home)?;

    // Update existing or insert new
    if let Some(existing) = pf.projects.iter_mut().find(|p| p.name == name) {
        existing.path = path.to_path_buf();
        let entry = existing.clone();
        save_projects_file(home, &pf)?;
        return Ok(entry);
    }

    let entry = ProjectEntry {
        name: name.to_string(),
        path: path.to_path_buf(),
        created_at: Utc::now(),
    };
    pf.projects.push(entry.clone());
    save_projects_file(home, &pf)?;
    Ok(entry)
}

/// List all registered projects.
///
/// # Errors
/// Returns an error if the projects file cannot be read.
pub fn list_projects(home: &Path) -> anyhow::Result<Vec<ProjectEntry>> {
    let pf = load_projects_file(home)?;
    Ok(pf.projects)
}

/// Find a project by name.
///
/// # Errors
/// Returns an error if the projects file cannot be read.
pub fn find_project(home: &Path, name: &str) -> anyhow::Result<Option<ProjectEntry>> {
    let pf = load_projects_file(home)?;
    Ok(pf.projects.into_iter().find(|p| p.name == name))
}

// ---------------------------------------------------------------------------
// Init command logic
// ---------------------------------------------------------------------------

/// Initialize a project directory: create `.orbs/`, write default config,
/// create empty store, and register in global projects list.
///
/// # Errors
/// Returns an error if directory creation or file writing fails.
pub fn init_project(home: &Path, project_dir: &Path) -> anyhow::Result<()> {
    let orbs_dir = project_dir.join(".orbs");
    std::fs::create_dir_all(&orbs_dir)?;

    // Write default config
    let default_config = OrbConfig::default();
    let config_content = toml::to_string_pretty(&default_config)?;
    let config_path = orbs_dir.join("config.toml");
    if !config_path.exists() {
        std::fs::write(&config_path, &config_content)?;
    }

    // Create empty store file
    let store_path = orbs_dir.join("orbs.jsonl");
    if !store_path.exists() {
        std::fs::write(&store_path, "")?;
    }

    // Register project — derive name from directory
    let name = project_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unnamed");
    register_project(home, name, project_dir)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // --- Config loading ---

    #[test]
    fn default_config_has_expected_values() {
        let cfg = OrbConfig::default();
        assert_eq!(cfg.default_model, "openrouter/free");
        assert_eq!(cfg.max_concurrency, 4);
        assert!(cfg.worker_binary.is_none());
        assert!(cfg.models.options.is_empty());
        assert!(!cfg.review.requires_approval_by_default);
        assert!(cfg.review.review_on_completion);
        assert!(cfg.notification.enabled);
        assert!(!cfg.notification.desktop_enabled);
        assert_eq!(cfg.second_opinion.mode, SecondOpinionMode::Off);
        assert!((cfg.second_opinion.confidence_threshold - 0.7).abs() < f32::EPSILON);
        assert!((cfg.second_opinion.sampling_rate - 0.1).abs() < f32::EPSILON);
        assert!(cfg.second_opinion.reviewer_model.is_none());
    }

    #[test]
    fn model_catalog_parses_from_toml() {
        let toml_str = r#"
default_model = "openrouter/free"

[models.default]
worker = "balanced"
coordinator = "planner"
reviewer = "fast"
bench = "balanced"

[models.options.balanced]
model = "openrouter/anthropic/claude-sonnet-4"
description = "Default coding model"
provider = "anthropic"
router = "openrouter"
reasoning = "medium"
effort = "medium"

[models.options.fast]
model = "openrouter/openai/gpt-4.1-mini"
description = "Cheap fast model"

[models.options.planner]
model = "openrouter/openai/gpt-5"
description = "Planning model"
reasoning = "high"

[models.workers]
research = "fast"

[models.coordinators]
decompose = "planner"

[models.phases]
speccing = "planner"

[models.bench]
grader = "fast"
"#;

        let parsed: OrbConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(parsed.models.default.worker.as_deref(), Some("balanced"));
        assert_eq!(
            parsed.models.options["balanced"].description.as_deref(),
            Some("Default coding model")
        );
        assert_eq!(parsed.models.workers["research"], "fast");
        assert!(parsed.models.validate().is_ok());
    }

    #[test]
    fn model_resolver_prefers_role_specific_catalog_option() {
        let cfg: OrbConfig = toml::from_str(
            r#"
default_model = "openrouter/free"

[models.default]
worker = "balanced"

[models.options.balanced]
model = "openrouter/anthropic/claude-sonnet-4"
description = "Balanced default"

[models.options.fast]
model = "openrouter/openai/gpt-4.1-mini"
description = "Fast cheap model"
reasoning = "low"
effort = "low"

[models.workers]
research = "fast"
"#,
        )
        .unwrap();

        let resolved = cfg
            .model_resolver()
            .resolve(ModelRole::Worker("research"))
            .unwrap();
        assert_eq!(resolved.key.as_deref(), Some("fast"));
        assert_eq!(resolved.model, "openrouter/openai/gpt-4.1-mini");
        assert_eq!(resolved.description.as_deref(), Some("Fast cheap model"));
        assert_eq!(resolved.router.as_deref(), Some("openrouter"));
        assert_eq!(resolved.reasoning.as_deref(), Some("low"));
        assert_eq!(resolved.effort.as_deref(), Some("low"));
        assert_eq!(resolved.source, "models.workers.research");
    }

    #[test]
    fn model_resolver_falls_back_to_default_model() {
        let cfg = OrbConfig {
            default_model: "openrouter/free".into(),
            ..Default::default()
        };

        let resolved = cfg
            .model_resolver()
            .resolve(ModelRole::Coordinator("decompose"))
            .unwrap();
        assert_eq!(resolved.key, None);
        assert_eq!(resolved.model, "openrouter/free");
        assert_eq!(resolved.provider.as_deref(), Some("openrouter"));
        assert_eq!(resolved.source, "default_model");
    }

    #[test]
    fn model_resolver_uses_reviewer_legacy_override() {
        let cfg = OrbConfig {
            default_model: "openrouter/free".into(),
            second_opinion: SecondOpinionConfig {
                reviewer_model: Some("openai/gpt-4.1-mini".into()),
                ..Default::default()
            },
            models: ModelConfig {
                default: ModelDefaults {
                    reviewer: Some("reviewer".into()),
                    ..Default::default()
                },
                options: [(
                    "reviewer".into(),
                    ModelOption {
                        model: "openrouter/anthropic/claude-haiku".into(),
                        ..Default::default()
                    },
                )]
                .into(),
                ..Default::default()
            },
            ..Default::default()
        };

        let resolved = cfg.model_resolver().resolve(ModelRole::Reviewer).unwrap();
        assert_eq!(resolved.model, "openai/gpt-4.1-mini");
        assert_eq!(resolved.source, "second_opinion.reviewer_model");
    }

    #[test]
    fn model_resolver_errors_on_unknown_catalog_key() {
        let cfg = OrbConfig {
            default_model: "openrouter/free".into(),
            models: ModelConfig {
                workers: [("edit".into(), "missing".into())].into(),
                ..Default::default()
            },
            ..Default::default()
        };

        let err = cfg
            .model_resolver()
            .resolve(ModelRole::Worker("edit"))
            .unwrap_err();
        assert!(format!("{err}").contains("unknown model option"));
    }

    #[test]
    fn load_config_rejects_unknown_model_catalog_key() {
        let home = tempdir().unwrap();
        let global_dir = home.path().join(".orboros");
        std::fs::create_dir_all(&global_dir).unwrap();
        std::fs::write(
            global_dir.join("config.toml"),
            r#"
[models.workers]
edit = "missing"
"#,
        )
        .unwrap();

        let err = load_config_with_home(Some(home.path()), None).unwrap_err();
        assert!(format!("{err}").contains("unknown model option"));
    }

    #[test]
    fn load_config_merges_model_catalog_sections() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();

        let global_dir = home.path().join(".orboros");
        std::fs::create_dir_all(&global_dir).unwrap();
        std::fs::write(
            global_dir.join("config.toml"),
            r#"
[models.default]
worker = "balanced"

[models.options.balanced]
model = "openrouter/anthropic/claude-sonnet-4"

[models.options.fast]
model = "openrouter/openai/gpt-4.1-mini"

[models.workers]
research = "fast"
edit = "balanced"
"#,
        )
        .unwrap();

        let orbs_dir = project.path().join(".orbs");
        std::fs::create_dir_all(&orbs_dir).unwrap();
        std::fs::write(
            orbs_dir.join("config.toml"),
            r#"
[models.workers]
edit = "fast"
"#,
        )
        .unwrap();

        let cfg = load_config_with_home(Some(home.path()), Some(project.path())).unwrap();
        assert_eq!(cfg.models.default.worker.as_deref(), Some("balanced"));
        assert_eq!(cfg.models.workers["research"], "fast");
        assert_eq!(cfg.models.workers["edit"], "fast");
    }

    #[test]
    fn second_opinion_parses_from_toml_with_all_fields() {
        let toml_str = r#"
            [second_opinion]
            mode = "confidence"
            confidence_threshold = 0.6
            sampling_rate = 0.25
            reviewer_model = "anthropic/claude-sonnet-4-6"
        "#;
        let parsed: OrbConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(parsed.second_opinion.mode, SecondOpinionMode::Confidence);
        assert!((parsed.second_opinion.confidence_threshold - 0.6).abs() < f32::EPSILON);
        assert!((parsed.second_opinion.sampling_rate - 0.25).abs() < f32::EPSILON);
        assert_eq!(
            parsed.second_opinion.reviewer_model.as_deref(),
            Some("anthropic/claude-sonnet-4-6")
        );
    }

    #[test]
    fn second_opinion_modes_serialize_snake_case() {
        for (mode, expected) in [
            (SecondOpinionMode::Off, "off"),
            (SecondOpinionMode::Always, "always"),
            (SecondOpinionMode::Confidence, "confidence"),
            (SecondOpinionMode::Sampling, "sampling"),
        ] {
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(json, format!("\"{expected}\""), "mode {mode:?}");
        }
    }

    #[test]
    fn second_opinion_validate_rejects_threshold_above_one() {
        let cfg = SecondOpinionConfig {
            confidence_threshold: 1.2,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn second_opinion_validate_rejects_threshold_below_zero() {
        let cfg = SecondOpinionConfig {
            confidence_threshold: -0.1,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn second_opinion_validate_rejects_sampling_rate_out_of_range() {
        let mut cfg = SecondOpinionConfig {
            sampling_rate: 1.5,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
        cfg.sampling_rate = -0.5;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn second_opinion_validate_rejects_non_finite() {
        let mut cfg = SecondOpinionConfig {
            confidence_threshold: f32::NAN,
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
        cfg.confidence_threshold = 0.5;
        cfg.sampling_rate = f32::INFINITY;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn second_opinion_validate_accepts_boundary_values() {
        let cfg = SecondOpinionConfig {
            mode: SecondOpinionMode::Sampling,
            confidence_threshold: 0.0,
            sampling_rate: 1.0,
            reviewer_model: None,
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn second_opinion_rejects_unknown_field_in_toml() {
        // Strict — typos at the section level surface immediately.
        let toml_str = r#"
            [second_opinion]
            mode = "off"
            confidence_threshhold = 0.5
        "#;
        let parsed: Result<OrbConfig, _> = toml::from_str(toml_str);
        assert!(parsed.is_err());
    }

    #[test]
    fn load_config_propagates_validation_error() {
        let dir = tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".orboros")).unwrap();
        std::fs::write(
            home.join(".orboros").join("config.toml"),
            r#"[second_opinion]
mode = "confidence"
confidence_threshold = 2.0
"#,
        )
        .unwrap();
        let result = load_config_with_home(Some(&home), None);
        assert!(result.is_err(), "validation should bubble up");
    }

    #[test]
    fn load_config_returns_defaults_when_no_files_exist() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        let cfg = load_config_with_home(Some(home.path()), Some(project.path())).unwrap();
        assert_eq!(cfg, OrbConfig::default());
    }

    #[test]
    fn load_config_reads_global_config() {
        let home = tempdir().unwrap();
        let global_dir = home.path().join(".orboros");
        std::fs::create_dir_all(&global_dir).unwrap();
        std::fs::write(
            global_dir.join("config.toml"),
            r#"
default_model = "claude-sonnet"
max_concurrency = 8
"#,
        )
        .unwrap();

        let cfg = load_config_with_home(Some(home.path()), None).unwrap();
        assert_eq!(cfg.default_model, "claude-sonnet");
        assert_eq!(cfg.max_concurrency, 8);
        // Unspecified fields should be defaults
        assert!(cfg.worker_binary.is_none());
    }

    #[test]
    fn load_config_project_overrides_global() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();

        // Global: model=global-model, concurrency=2
        let global_dir = home.path().join(".orboros");
        std::fs::create_dir_all(&global_dir).unwrap();
        std::fs::write(
            global_dir.join("config.toml"),
            r#"
default_model = "global-model"
max_concurrency = 2
"#,
        )
        .unwrap();

        // Project: model=project-model (concurrency not set, should keep global's 2)
        let orbs_dir = project.path().join(".orbs");
        std::fs::create_dir_all(&orbs_dir).unwrap();
        std::fs::write(
            orbs_dir.join("config.toml"),
            r#"
default_model = "project-model"
"#,
        )
        .unwrap();

        let cfg = load_config_with_home(Some(home.path()), Some(project.path())).unwrap();
        assert_eq!(cfg.default_model, "project-model");
        assert_eq!(cfg.max_concurrency, 2);
    }

    #[test]
    fn load_config_with_bench_overrides_project() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        let bench = tempdir().unwrap();

        let global_dir = home.path().join(".orboros");
        std::fs::create_dir_all(&global_dir).unwrap();
        std::fs::write(
            global_dir.join("config.toml"),
            r#"
default_model = "global/model"
worker_binary = "/global/heddle"

[bench]
timeout_s = 100
"#,
        )
        .unwrap();

        let orbs_dir = project.path().join(".orbs");
        std::fs::create_dir_all(&orbs_dir).unwrap();
        std::fs::write(
            orbs_dir.join("config.toml"),
            r#"
default_model = "project/model"

[bench]
max_iterations = 8
"#,
        )
        .unwrap();

        let bench_config = bench.path().join("config.toml");
        std::fs::write(
            &bench_config,
            r#"
worker_binary = "/bench/heddle"

[bench]
timeout_s = 200

[models.bench]
default = "bench/model"
"#,
        )
        .unwrap();

        let cfg = load_config_with_home_and_bench(
            Some(home.path()),
            Some(project.path()),
            Some(&bench_config),
        )
        .unwrap();
        assert_eq!(cfg.default_model, "project/model");
        assert_eq!(cfg.worker_binary.as_deref(), Some("/bench/heddle"));
        assert_eq!(cfg.bench.timeout_s, Some(200));
        assert_eq!(cfg.bench.max_iterations, Some(8));
        assert_eq!(cfg.models.bench.default.as_deref(), Some("bench/model"));
    }

    #[test]
    fn load_config_with_bench_errors_when_explicit_path_is_missing() {
        let bench = tempdir().unwrap();
        let missing = bench.path().join("missing.toml");
        let err = load_config_with_bench(None, bench.path(), Some(&missing)).unwrap_err();
        assert!(err.to_string().contains("bench config not found"));
    }

    #[test]
    fn load_config_merges_nested_sections() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();

        let global_dir = home.path().join(".orboros");
        std::fs::create_dir_all(&global_dir).unwrap();
        std::fs::write(
            global_dir.join("config.toml"),
            r"
[review]
requires_approval_by_default = true
review_on_completion = false
",
        )
        .unwrap();

        // Project only overrides one nested field
        let orbs_dir = project.path().join(".orbs");
        std::fs::create_dir_all(&orbs_dir).unwrap();
        std::fs::write(
            orbs_dir.join("config.toml"),
            r"
[review]
review_on_completion = true
",
        )
        .unwrap();

        let cfg = load_config_with_home(Some(home.path()), Some(project.path())).unwrap();
        assert!(cfg.review.requires_approval_by_default); // from global
        assert!(cfg.review.review_on_completion); // overridden by project
    }

    #[test]
    fn load_config_merges_prompt_sections() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();

        let global_dir = home.path().join(".orboros");
        std::fs::create_dir_all(&global_dir).unwrap();
        std::fs::write(
            global_dir.join("config.toml"),
            r#"
[prompts.default]
system = "global default"

[prompts.workers.edit]
system = "global edit"

[prompts.coordinators.decompose]
system = "global decompose"

[prompts.phases.speccing]
system = "global speccing"
"#,
        )
        .unwrap();

        let orbs_dir = project.path().join(".orbs");
        std::fs::create_dir_all(&orbs_dir).unwrap();
        std::fs::write(
            orbs_dir.join("config.toml"),
            r#"
[prompts.phases.speccing]
system = "project speccing"
"#,
        )
        .unwrap();

        let cfg = load_config_with_home(Some(home.path()), Some(project.path())).unwrap();
        assert_eq!(
            cfg.prompts.default.system.as_deref(),
            Some("global default")
        );
        assert_eq!(
            cfg.prompts.workers["edit"].system.as_deref(),
            Some("global edit")
        );
        assert_eq!(
            cfg.prompts.coordinators["decompose"].system.as_deref(),
            Some("global decompose")
        );
        assert_eq!(
            cfg.prompts.phases["speccing"].system.as_deref(),
            Some("project speccing")
        );
    }

    #[test]
    fn load_config_errors_on_invalid_toml() {
        let home = tempdir().unwrap();
        let global_dir = home.path().join(".orboros");
        std::fs::create_dir_all(&global_dir).unwrap();
        std::fs::write(global_dir.join("config.toml"), "this is [not valid").unwrap();

        let result = load_config_with_home(Some(home.path()), None);
        assert!(result.is_err());
    }

    #[test]
    fn config_roundtrips_through_toml() {
        let cfg = OrbConfig {
            default_model: "test-model".to_string(),
            max_concurrency: 16,
            worker_binary: Some("/usr/bin/heddle".to_string()),
            models: ModelConfig {
                default: ModelDefaults {
                    worker: Some("balanced".into()),
                    ..Default::default()
                },
                options: [(
                    "balanced".into(),
                    ModelOption {
                        model: "openrouter/anthropic/claude-sonnet-4".into(),
                        description: Some("Balanced test model".into()),
                        ..Default::default()
                    },
                )]
                .into(),
                workers: [("edit".into(), "balanced".into())].into(),
                ..Default::default()
            },
            bench: BenchConfig {
                timeout_s: Some(300),
                max_iterations: Some(8),
            },
            prompts: PromptConfig {
                workers: [(
                    "edit".into(),
                    PromptOverride {
                        system: Some("edit prompt".into()),
                        system_file: None,
                    },
                )]
                .into(),
                ..Default::default()
            },
            review: ReviewConfig {
                requires_approval_by_default: true,
                review_on_completion: false,
            },
            second_opinion: SecondOpinionConfig {
                mode: SecondOpinionMode::Always,
                confidence_threshold: 0.6,
                sampling_rate: 0.2,
                reviewer_model: Some("test-reviewer".into()),
            },
            notification: NotificationConfig {
                enabled: false,
                desktop_enabled: true,
            },
        };
        let serialized = toml::to_string_pretty(&cfg).unwrap();
        let deserialized: OrbConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(cfg, deserialized);
    }

    // --- Projects registry ---

    #[test]
    fn register_and_list_projects() {
        let home = tempdir().unwrap();

        register_project(home.path(), "alpha", Path::new("/tmp/alpha")).unwrap();
        register_project(home.path(), "beta", Path::new("/tmp/beta")).unwrap();

        let projects = list_projects(home.path()).unwrap();
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].name, "alpha");
        assert_eq!(projects[1].name, "beta");
    }

    #[test]
    fn register_project_updates_existing() {
        let home = tempdir().unwrap();

        register_project(home.path(), "proj", Path::new("/old/path")).unwrap();
        register_project(home.path(), "proj", Path::new("/new/path")).unwrap();

        let projects = list_projects(home.path()).unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].path, Path::new("/new/path"));
    }

    #[test]
    fn find_project_returns_match() {
        let home = tempdir().unwrap();
        register_project(home.path(), "myproj", Path::new("/projects/myproj")).unwrap();

        let found = find_project(home.path(), "myproj").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "myproj");
    }

    #[test]
    fn find_project_returns_none_for_missing() {
        let home = tempdir().unwrap();
        let found = find_project(home.path(), "nope").unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn list_projects_empty_when_no_file() {
        let home = tempdir().unwrap();
        let projects = list_projects(home.path()).unwrap();
        assert!(projects.is_empty());
    }

    // --- Init ---

    #[test]
    fn init_creates_orbs_directory_and_files() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();

        init_project(home.path(), project.path()).unwrap();

        let orbs_dir = project.path().join(".orbs");
        assert!(orbs_dir.is_dir());
        assert!(orbs_dir.join("config.toml").is_file());
        assert!(orbs_dir.join("orbs.jsonl").is_file());

        // Config should parse as valid OrbConfig
        let content = std::fs::read_to_string(orbs_dir.join("config.toml")).unwrap();
        let cfg: OrbConfig = toml::from_str(&content).unwrap();
        assert_eq!(cfg, OrbConfig::default());
    }

    #[test]
    fn init_registers_project() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();

        init_project(home.path(), project.path()).unwrap();

        let projects = list_projects(home.path()).unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].path, project.path());
    }

    #[test]
    fn init_is_idempotent() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();

        init_project(home.path(), project.path()).unwrap();
        // Write custom content to config to verify it's not overwritten
        let config_path = project.path().join(".orbs").join("config.toml");
        std::fs::write(&config_path, "default_model = \"custom\"\n").unwrap();

        init_project(home.path(), project.path()).unwrap();

        // Config should still have custom content (not overwritten)
        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("custom"));
    }

    #[test]
    fn init_store_file_is_empty() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();

        init_project(home.path(), project.path()).unwrap();

        let content = std::fs::read_to_string(project.path().join(".orbs/orbs.jsonl")).unwrap();
        assert!(content.is_empty());
    }
}

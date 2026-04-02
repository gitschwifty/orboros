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
    pub review: ReviewConfig,
    pub notification: NotificationConfig,
}

impl Default for OrbConfig {
    fn default() -> Self {
        Self {
            default_model: "openrouter/free".to_string(),
            max_concurrency: 4,
            worker_binary: None,
            review: ReviewConfig::default(),
            notification: NotificationConfig::default(),
        }
    }
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

/// Testable version that accepts a custom home directory.
pub(crate) fn load_config_with_home(
    home: Option<&Path>,
    project_dir: Option<&Path>,
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

    let config: OrbConfig = toml::Value::Table(base_table).try_into()?;
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
        assert!(!cfg.review.requires_approval_by_default);
        assert!(cfg.review.review_on_completion);
        assert!(cfg.notification.enabled);
        assert!(!cfg.notification.desktop_enabled);
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
    fn load_config_merges_nested_sections() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();

        let global_dir = home.path().join(".orboros");
        std::fs::create_dir_all(&global_dir).unwrap();
        std::fs::write(
            global_dir.join("config.toml"),
            r#"
[review]
requires_approval_by_default = true
review_on_completion = false
"#,
        )
        .unwrap();

        // Project only overrides one nested field
        let orbs_dir = project.path().join(".orbs");
        std::fs::create_dir_all(&orbs_dir).unwrap();
        std::fs::write(
            orbs_dir.join("config.toml"),
            r#"
[review]
review_on_completion = true
"#,
        )
        .unwrap();

        let cfg = load_config_with_home(Some(home.path()), Some(project.path())).unwrap();
        assert!(cfg.review.requires_approval_by_default); // from global
        assert!(cfg.review.review_on_completion); // overridden by project
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
            review: ReviewConfig {
                requires_approval_by_default: true,
                review_on_completion: false,
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

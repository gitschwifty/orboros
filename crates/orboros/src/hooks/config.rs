//! TOML config for lifecycle hooks. Parsed once at startup; matchers
//! and regexes compile at load time so misconfigs surface immediately
//! rather than at fire time.
//!
//! Layout (per design doc §6):
//! - `~/.orboros/hooks.toml` (global) loaded first
//! - `<state_dir>/hooks.toml` (project) loaded second
//! - Both layers concatenate into one ordered list. Sync hook firing
//!   uses that order to determine which short-circuits the chain.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use orbs::orb::{OrbPhase, OrbStatus, OrbType};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::hooks::event::HookEvent;

/// Where a hook entry was loaded from. Used for diagnostics and the
/// global-first firing order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigLayer {
    Global,
    Project,
}

impl ConfigLayer {
    fn label(self) -> &'static str {
        match self {
            ConfigLayer::Global => "global",
            ConfigLayer::Project => "project",
        }
    }
}

/// One hook entry as defined in a TOML config.
#[derive(Debug, Clone)]
pub struct HookEntry {
    pub name: String,
    pub on: HookEvent,
    pub run: String,
    pub sync: bool,
    pub timeout_ms: u64,
    pub timeout_aborts: bool,
    pub enabled: bool,
    pub match_rules: HookMatch,
    pub source: ConfigLayer,
}

/// All matchers AND-combine. `None` fields don't constrain.
#[derive(Debug, Clone, Default)]
pub struct HookMatch {
    pub orb_type: Option<Vec<OrbType>>,
    pub labels_any: Option<Vec<String>>,
    pub labels_all: Option<Vec<String>>,
    pub status: Option<Vec<OrbStatus>>,
    pub phase: Option<Vec<OrbPhase>>,
    pub priority_max: Option<u8>,
    pub priority_min: Option<u8>,
    pub worker_type: Option<String>,
    pub title_regex: Option<Regex>,
    pub description_regex: Option<Regex>,
    pub scope_includes: Option<Vec<String>>,
}

/// Full loaded config — flat ordered list, global hooks first.
#[derive(Debug, Clone, Default)]
pub struct HooksConfig {
    pub hooks: Vec<HookEntry>,
}

impl HooksConfig {
    /// Loads global + project layers, in that order. Either path missing
    /// is treated as an empty layer (not an error). Compiles regexes at
    /// load time; warns on duplicate hook names (across both layers).
    ///
    /// # Errors
    ///
    /// Returns an error if a present file fails to parse, an unknown
    /// event name appears, or a regex fails to compile.
    pub fn load(global: Option<&Path>, project: Option<&Path>) -> anyhow::Result<Self> {
        let mut hooks = Vec::new();
        if let Some(path) = global {
            if path.exists() {
                let mut layer = parse_layer(path, ConfigLayer::Global)?;
                hooks.append(&mut layer);
            }
        }
        if let Some(path) = project {
            if path.exists() {
                let mut layer = parse_layer(path, ConfigLayer::Project)?;
                hooks.append(&mut layer);
            }
        }
        warn_on_duplicate_names(&hooks);
        Ok(Self { hooks })
    }

    /// Returns enabled hooks targeting the given event in source order
    /// (global before project, then declaration order within each layer).
    pub fn enabled_for(&self, event: HookEvent) -> impl Iterator<Item = &HookEntry> {
        self.hooks
            .iter()
            .filter(move |h| h.enabled && h.on == event)
    }
}

// ── on-disk schema ──

#[derive(Debug, Deserialize, Serialize)]
struct RawConfig {
    #[serde(default, rename = "hook")]
    hooks: Vec<RawHook>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawHook {
    #[serde(default)]
    name: Option<String>,
    on: String,
    run: String,
    #[serde(default)]
    sync: Option<bool>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    timeout_aborts: Option<bool>,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default, rename = "match")]
    match_rules: RawMatch,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawMatch {
    #[serde(default)]
    orb_type: Option<StringOrList>,
    #[serde(default)]
    labels: Option<Vec<String>>,
    #[serde(default)]
    labels_all: Option<Vec<String>>,
    #[serde(default)]
    status: Option<StringOrList>,
    #[serde(default)]
    phase: Option<StringOrList>,
    #[serde(default)]
    priority_max: Option<u8>,
    #[serde(default)]
    priority_min: Option<u8>,
    #[serde(default)]
    worker_type: Option<String>,
    #[serde(default)]
    title_regex: Option<String>,
    #[serde(default)]
    description_regex: Option<String>,
    #[serde(default)]
    scope_includes: Option<StringOrList>,
}

/// TOML value that may be a single string or a list of strings.
#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum StringOrList {
    One(String),
    Many(Vec<String>),
}

impl StringOrList {
    fn into_vec(self) -> Vec<String> {
        match self {
            StringOrList::One(s) => vec![s],
            StringOrList::Many(v) => v,
        }
    }
}

fn parse_layer(path: &Path, layer: ConfigLayer) -> anyhow::Result<Vec<HookEntry>> {
    let body = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("failed to read hooks config {}: {e}", path.display()))?;
    let raw: RawConfig = toml::from_str(&body)
        .map_err(|e| anyhow::anyhow!("failed to parse hooks config {}: {e}", path.display()))?;

    raw.hooks
        .into_iter()
        .enumerate()
        .map(|(idx, h)| compile_hook(idx, h, path, layer))
        .collect()
}

fn compile_hook(
    idx: usize,
    raw: RawHook,
    path: &Path,
    layer: ConfigLayer,
) -> anyhow::Result<HookEntry> {
    let on: HookEvent = raw
        .on
        .parse()
        .map_err(|e| anyhow::anyhow!("{}: hook[{}].on is invalid: {}", path.display(), idx, e))?;
    let name = raw.name.unwrap_or_else(|| format!("{on}#{idx}"));
    let sync = raw.sync.unwrap_or_else(|| on.is_pre_event());
    let timeout_ms = raw.timeout_ms.unwrap_or_else(|| on.default_timeout_ms());
    let timeout_aborts = raw.timeout_aborts.unwrap_or(false);
    let match_rules = compile_match(&name, path, raw.match_rules)?;
    if raw.run.trim().is_empty() {
        anyhow::bail!("{}: hook '{}' has empty `run`", path.display(), name);
    }
    Ok(HookEntry {
        name,
        on,
        run: raw.run,
        sync,
        timeout_ms,
        timeout_aborts,
        enabled: raw.enabled,
        match_rules,
        source: layer,
    })
}

fn compile_match(name: &str, path: &Path, raw: RawMatch) -> anyhow::Result<HookMatch> {
    Ok(HookMatch {
        orb_type: raw
            .orb_type
            .map(|v| {
                v.into_vec()
                    .into_iter()
                    .map(|s| parse_orb_type(&s))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()
            .map_err(|e| anyhow::anyhow!("{}: hook '{}': {e}", path.display(), name))?,
        labels_any: raw.labels,
        labels_all: raw.labels_all,
        status: raw
            .status
            .map(|v| {
                v.into_vec()
                    .into_iter()
                    .map(|s| parse_status(&s))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()
            .map_err(|e| anyhow::anyhow!("{}: hook '{}': {e}", path.display(), name))?,
        phase: raw
            .phase
            .map(|v| {
                v.into_vec()
                    .into_iter()
                    .map(|s| parse_phase(&s))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()
            .map_err(|e| anyhow::anyhow!("{}: hook '{}': {e}", path.display(), name))?,
        priority_max: raw.priority_max,
        priority_min: raw.priority_min,
        worker_type: raw.worker_type,
        title_regex: compile_optional_regex(name, "title_regex", raw.title_regex)?,
        description_regex: compile_optional_regex(
            name,
            "description_regex",
            raw.description_regex,
        )?,
        scope_includes: raw.scope_includes.map(StringOrList::into_vec),
    })
}

fn compile_optional_regex(
    hook_name: &str,
    field: &str,
    pattern: Option<String>,
) -> anyhow::Result<Option<Regex>> {
    match pattern {
        None => Ok(None),
        Some(p) => Regex::new(&p)
            .map(Some)
            .map_err(|e| anyhow::anyhow!("hook '{hook_name}' has invalid {field} regex: {e}")),
    }
}

fn parse_orb_type(s: &str) -> anyhow::Result<OrbType> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "epic" => OrbType::Epic,
        "feature" => OrbType::Feature,
        "task" => OrbType::Task,
        "bug" => OrbType::Bug,
        "chore" => OrbType::Chore,
        "docs" => OrbType::Docs,
        other => anyhow::bail!("unknown orb_type: {other}"),
    })
}

fn parse_status(s: &str) -> anyhow::Result<OrbStatus> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "draft" => OrbStatus::Draft,
        "pending" => OrbStatus::Pending,
        "active" => OrbStatus::Active,
        "review" => OrbStatus::Review,
        "done" => OrbStatus::Done,
        "failed" => OrbStatus::Failed,
        "cancelled" => OrbStatus::Cancelled,
        "deferred" => OrbStatus::Deferred,
        "tombstone" => OrbStatus::Tombstone,
        other => anyhow::bail!("unknown status: {other}"),
    })
}

fn parse_phase(s: &str) -> anyhow::Result<OrbPhase> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "draft" => OrbPhase::Draft,
        "pending" => OrbPhase::Pending,
        "speccing" => OrbPhase::Speccing,
        "decomposing" => OrbPhase::Decomposing,
        "refining" => OrbPhase::Refining,
        "review" => OrbPhase::Review,
        "waiting" => OrbPhase::Waiting,
        "executing" => OrbPhase::Executing,
        "reevaluating" => OrbPhase::Reevaluating,
        "done" => OrbPhase::Done,
        "failed" => OrbPhase::Failed,
        "cancelled" => OrbPhase::Cancelled,
        "deferred" => OrbPhase::Deferred,
        "tombstone" => OrbPhase::Tombstone,
        other => anyhow::bail!("unknown phase: {other}"),
    })
}

fn warn_on_duplicate_names(hooks: &[HookEntry]) {
    let mut seen: HashSet<&str> = HashSet::new();
    for h in hooks {
        if !seen.insert(&h.name) {
            tracing::warn!(
                hook = %h.name,
                layer = %h.source.label(),
                "duplicate hook name across config layers (both will still fire in source order)"
            );
        }
    }
}

/// Resolves the default global + project hooks paths.
#[must_use]
pub fn default_paths(state_dir: &Path) -> (Option<PathBuf>, Option<PathBuf>) {
    let global = dirs::home_dir().map(|h| h.join(".orboros").join("hooks.toml"));
    let project = Some(state_dir.join("hooks.toml"));
    (global, project)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn load_missing_files_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.toml");
        let config = HooksConfig::load(Some(&missing), Some(&missing)).unwrap();
        assert!(config.hooks.is_empty());
    }

    #[test]
    fn load_one_simple_hook() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "hooks.toml",
            r#"
            [[hook]]
            name = "fmt-after-edit"
            on = "post-worker-complete"
            run = "cargo fmt --check"
        "#,
        );
        let config = HooksConfig::load(None, Some(&path)).unwrap();
        assert_eq!(config.hooks.len(), 1);
        let h = &config.hooks[0];
        assert_eq!(h.name, "fmt-after-edit");
        assert_eq!(h.on, HookEvent::PostWorkerComplete);
        assert_eq!(h.run, "cargo fmt --check");
        assert!(!h.sync, "post-* defaults to async");
        assert_eq!(h.timeout_ms, 30_000);
        assert!(h.enabled);
        assert_eq!(h.source, ConfigLayer::Project);
    }

    #[test]
    fn default_name_derived_from_event_and_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "hooks.toml",
            r#"
            [[hook]]
            on = "post-worker-complete"
            run = "x"

            [[hook]]
            on = "post-worker-complete"
            run = "y"
        "#,
        );
        let config = HooksConfig::load(None, Some(&path)).unwrap();
        assert_eq!(config.hooks[0].name, "post-worker-complete#0");
        assert_eq!(config.hooks[1].name, "post-worker-complete#1");
    }

    #[test]
    fn pre_event_defaults_sync_true_and_short_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "hooks.toml",
            r#"
            [[hook]]
            on = "pre-worker-spawn"
            run = "./scripts/prep.sh"
        "#,
        );
        let config = HooksConfig::load(None, Some(&path)).unwrap();
        let h = &config.hooks[0];
        assert!(h.sync);
        assert_eq!(h.timeout_ms, 5_000);
    }

    #[test]
    fn explicit_sync_and_timeout_override_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "hooks.toml",
            r#"
            [[hook]]
            on = "post-worker-complete"
            run = "./lint.sh"
            sync = true
            timeout_ms = 60000
            timeout_aborts = true
        "#,
        );
        let config = HooksConfig::load(None, Some(&path)).unwrap();
        let h = &config.hooks[0];
        assert!(h.sync);
        assert_eq!(h.timeout_ms, 60_000);
        assert!(h.timeout_aborts);
    }

    #[test]
    fn invalid_event_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "hooks.toml",
            r#"
            [[hook]]
            on = "not-a-real-event"
            run = "x"
        "#,
        );
        let err = HooksConfig::load(None, Some(&path)).unwrap_err();
        assert!(format!("{err:?}").contains("not-a-real-event"));
    }

    #[test]
    fn invalid_regex_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "hooks.toml",
            r#"
            [[hook]]
            name = "bad-regex"
            on = "on-orb-create"
            run = "x"
            match.title_regex = "[unclosed"
        "#,
        );
        let err = HooksConfig::load(None, Some(&path)).unwrap_err();
        assert!(format!("{err:?}").contains("invalid title_regex"));
    }

    #[test]
    fn empty_run_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "hooks.toml",
            r#"
            [[hook]]
            on = "on-orb-create"
            run = "   "
        "#,
        );
        let err = HooksConfig::load(None, Some(&path)).unwrap_err();
        assert!(format!("{err:?}").contains("empty `run`"));
    }

    #[test]
    fn unknown_top_level_field_errors_via_deny_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "hooks.toml",
            r#"
            [[hook]]
            on = "on-orb-create"
            run = "x"
            unkwn = true
        "#,
        );
        let err = HooksConfig::load(None, Some(&path)).unwrap_err();
        assert!(format!("{err:?}").contains("unknown"));
    }

    #[test]
    fn match_orb_type_string_or_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "hooks.toml",
            r#"
            [[hook]]
            name = "one"
            on = "on-orb-create"
            run = "x"
            match.orb_type = "task"

            [[hook]]
            name = "many"
            on = "on-orb-create"
            run = "x"
            match.orb_type = ["task", "bug"]
        "#,
        );
        let config = HooksConfig::load(None, Some(&path)).unwrap();
        assert_eq!(
            config.hooks[0].match_rules.orb_type,
            Some(vec![OrbType::Task])
        );
        assert_eq!(
            config.hooks[1].match_rules.orb_type,
            Some(vec![OrbType::Task, OrbType::Bug])
        );
    }

    #[test]
    fn match_labels_and_labels_all_parse() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "hooks.toml",
            r#"
            [[hook]]
            on = "on-orb-create"
            run = "x"
            match.labels = ["db"]
            match.labels_all = ["security", "external-input"]
        "#,
        );
        let config = HooksConfig::load(None, Some(&path)).unwrap();
        let m = &config.hooks[0].match_rules;
        assert_eq!(m.labels_any, Some(vec!["db".to_string()]));
        assert_eq!(
            m.labels_all,
            Some(vec!["security".to_string(), "external-input".to_string()])
        );
    }

    #[test]
    fn match_phase_parses_event_with_phase_arg() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "hooks.toml",
            r#"
            [[hook]]
            on = "pre-phase-transition(refining)"
            run = "x"
        "#,
        );
        let config = HooksConfig::load(None, Some(&path)).unwrap();
        assert_eq!(
            config.hooks[0].on,
            HookEvent::PrePhaseTransition(OrbPhase::Refining)
        );
        // pre-phase-transition is a pre-event → defaults to sync + 5s.
        assert!(config.hooks[0].sync);
        assert_eq!(config.hooks[0].timeout_ms, 5_000);
    }

    #[test]
    fn global_first_then_project_order() {
        let dir = tempfile::tempdir().unwrap();
        let global = write(
            dir.path(),
            "global.toml",
            r#"
            [[hook]]
            name = "g1"
            on = "on-orb-create"
            run = "g1"
        "#,
        );
        let project = write(
            dir.path(),
            "project.toml",
            r#"
            [[hook]]
            name = "p1"
            on = "on-orb-create"
            run = "p1"
        "#,
        );
        let config = HooksConfig::load(Some(&global), Some(&project)).unwrap();
        let names: Vec<_> = config.hooks.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, vec!["g1", "p1"]);
        assert_eq!(config.hooks[0].source, ConfigLayer::Global);
        assert_eq!(config.hooks[1].source, ConfigLayer::Project);
    }

    #[test]
    fn enabled_false_excluded_from_enabled_for() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "hooks.toml",
            r#"
            [[hook]]
            name = "off"
            on = "on-orb-create"
            run = "x"
            enabled = false

            [[hook]]
            name = "on"
            on = "on-orb-create"
            run = "y"
        "#,
        );
        let config = HooksConfig::load(None, Some(&path)).unwrap();
        let active: Vec<_> = config
            .enabled_for(HookEvent::OnOrbCreate)
            .map(|h| h.name.as_str())
            .collect();
        assert_eq!(active, vec!["on"]);
    }

    #[test]
    fn enabled_for_filters_by_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "hooks.toml",
            r#"
            [[hook]]
            name = "a"
            on = "on-orb-create"
            run = "x"

            [[hook]]
            name = "b"
            on = "post-worker-complete"
            run = "y"
        "#,
        );
        let config = HooksConfig::load(None, Some(&path)).unwrap();
        let only_create: Vec<_> = config
            .enabled_for(HookEvent::OnOrbCreate)
            .map(|h| h.name.as_str())
            .collect();
        assert_eq!(only_create, vec!["a"]);
    }

    #[test]
    fn invalid_orb_type_in_match_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "hooks.toml",
            r#"
            [[hook]]
            on = "on-orb-create"
            run = "x"
            match.orb_type = ["banana"]
        "#,
        );
        let err = HooksConfig::load(None, Some(&path)).unwrap_err();
        assert!(format!("{err:?}").contains("banana"));
    }
}

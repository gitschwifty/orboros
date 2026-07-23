use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A tool profile defining which tools a worker type is allowed to use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolProfile {
    /// Tools the worker is allowed to use.
    pub allowed_tools: Vec<String>,
}

const NONE: &[&str] = &[];
const READ_ONLY: &[&str] = &["read_file", "glob", "grep"];
const TEST: &[&str] = &["read_file", "glob", "grep", "bash"];
const RESEARCH: &[&str] = &["read_file", "glob", "grep", "web_fetch", "write_file"];
const EDIT: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "glob",
    "grep",
    "bash",
];

/// Returns the canonical Heddle tool names for an Orboros worker role.
#[must_use]
pub fn builtin_tools(worker_type: &str) -> &'static [&'static str] {
    match worker_type {
        "none" | "bench_t1" => NONE,
        "coordinator" | "review" | "read_only" => READ_ONLY,
        "test" => TEST,
        "research" => RESEARCH,
        "edit" | "execute" | "bench_t2" => EDIT,
        // Unknown execution roles must never silently become no-tool workers.
        _ => EDIT,
    }
}

/// Resolves a worker role to either its configured profile or the built-in
/// canonical Heddle capability set.
#[must_use]
pub fn resolve_tools(profiles: &BTreeMap<String, ToolProfile>, worker_type: &str) -> Vec<String> {
    profile_for(profiles, worker_type).map_or_else(
        || {
            builtin_tools(worker_type)
                .iter()
                .map(ToString::to_string)
                .collect()
        },
        |profile| profile.allowed_tools.clone(),
    )
}

/// Validates that a config profile only names concrete Heddle tools.
pub fn validate_profiles(profiles: &BTreeMap<String, ToolProfile>) -> Result<(), String> {
    for (profile, config) in profiles {
        for tool in &config.allowed_tools {
            if !EDIT.contains(&tool.as_str()) && !RESEARCH.contains(&tool.as_str()) {
                let suggestion = match tool.as_str() {
                    "read" => Some("read_file"),
                    "write" => Some("write_file or edit_file"),
                    "execute" => Some("bash"),
                    "web_search" => Some("web_fetch"),
                    _ => None,
                };
                let hint = suggestion.map_or_else(String::new, |name| format!("; use `{name}`"));
                return Err(format!(
                    "tool_profiles.{profile}.allowed_tools contains unknown Heddle tool `{tool}`{hint}"
                ));
            }
        }
    }
    Ok(())
}

/// Returns the tool profile for a worker type, falling back to the
/// "default" profile when present.
#[must_use]
pub fn profile_for<'a>(
    profiles: &'a BTreeMap<String, ToolProfile>,
    worker_type: &str,
) -> Option<&'a ToolProfile> {
    profiles
        .get(worker_type)
        .or_else(|| profiles.get("default"))
}

/// Result of filtering requested tools against a profile.
#[derive(Debug, Clone, PartialEq)]
pub struct FilteredTools {
    /// Tools that passed the filter.
    pub allowed: Vec<String>,
    /// Tools that were denied by the profile.
    pub denied: Vec<String>,
}

/// Filters requested tools against a profile.
///
/// If no profile is provided, all requested tools are allowed.
pub fn filter_tools(requested: &[String], profile: Option<&ToolProfile>) -> FilteredTools {
    let Some(profile) = profile else {
        return FilteredTools {
            allowed: requested.to_vec(),
            denied: vec![],
        };
    };

    let mut allowed = Vec::new();
    let mut denied = Vec::new();

    for tool in requested {
        if profile.allowed_tools.contains(tool) {
            allowed.push(tool.clone());
        } else {
            denied.push(tool.clone());
        }
    }

    FilteredTools { allowed, denied }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_serde_round_trip() {
        let profile = ToolProfile {
            allowed_tools: vec!["read".into(), "write".into(), "execute".into()],
        };
        let json = serde_json::to_string(&profile).unwrap();
        let parsed: ToolProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(profile, parsed);
    }

    #[test]
    fn filter_no_profile_passes_all() {
        let requested = vec!["read".into(), "write".into()];
        let result = filter_tools(&requested, None);
        assert_eq!(result.allowed, requested);
        assert!(result.denied.is_empty());
    }

    #[test]
    fn filter_all_allowed() {
        let profile = ToolProfile {
            allowed_tools: vec!["read".into(), "write".into()],
        };
        let requested = vec!["read".into(), "write".into()];
        let result = filter_tools(&requested, Some(&profile));
        assert_eq!(result.allowed, vec!["read", "write"]);
        assert!(result.denied.is_empty());
    }

    #[test]
    fn filter_some_denied() {
        let profile = ToolProfile {
            allowed_tools: vec!["read".into()],
        };
        let requested = vec!["read".into(), "write".into(), "execute".into()];
        let result = filter_tools(&requested, Some(&profile));
        assert_eq!(result.allowed, vec!["read"]);
        assert_eq!(result.denied, vec!["write", "execute"]);
    }

    #[test]
    fn filter_all_denied() {
        let profile = ToolProfile {
            allowed_tools: vec!["web_search".into()],
        };
        let requested = vec!["read".into(), "write".into()];
        let result = filter_tools(&requested, Some(&profile));
        assert!(result.allowed.is_empty());
        assert_eq!(result.denied, vec!["read", "write"]);
    }

    #[test]
    fn filter_empty_requested() {
        let profile = ToolProfile {
            allowed_tools: vec!["read".into()],
        };
        let result = filter_tools(&[], Some(&profile));
        assert!(result.allowed.is_empty());
        assert!(result.denied.is_empty());
    }

    #[test]
    fn filter_empty_profile() {
        let profile = ToolProfile {
            allowed_tools: vec![],
        };
        let requested = vec!["read".into(), "write".into()];
        let result = filter_tools(&requested, Some(&profile));
        assert!(result.allowed.is_empty());
        assert_eq!(result.denied, vec!["read", "write"]);
    }

    #[test]
    fn builtins_use_concrete_heddle_tool_names() {
        assert_eq!(builtin_tools("bench_t1"), NONE);
        assert_eq!(builtin_tools("coordinator"), READ_ONLY);
        assert_eq!(builtin_tools("test"), TEST);
        assert_eq!(builtin_tools("research"), RESEARCH);
        assert_eq!(builtin_tools("bench_t2"), EDIT);
    }

    #[test]
    fn configured_profile_overrides_builtin() {
        let profiles = BTreeMap::from([(
            "edit".into(),
            ToolProfile {
                allowed_tools: vec!["read_file".into()],
            },
        )]);
        assert_eq!(resolve_tools(&profiles, "edit"), vec!["read_file"]);
    }

    #[test]
    fn invalid_alias_is_rejected_with_a_suggestion() {
        let profiles = BTreeMap::from([(
            "edit".into(),
            ToolProfile {
                allowed_tools: vec!["execute".into()],
            },
        )]);
        let err = validate_profiles(&profiles).unwrap_err();
        assert!(err.contains("bash"));
    }
}

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// A tool profile defining which tools a worker type is allowed to use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolProfile {
    /// Tools the worker is allowed to use.
    pub allowed_tools: Vec<String>,
}

/// Optional tool-policy override for a benchmark case or other bounded run.
///
/// `profile` selects either a configured `tool_profiles.<name>` profile or a
/// built-in profile. `allowed_tools` is an exact list. A phase-specific entry
/// takes precedence over the top-level default. The final list is always
/// intersected with the caller's base worker tools, which remains the ceiling.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhaseToolPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub phases: BTreeMap<String, ToolPolicyOverride>,
}

/// A single phase's override within [`PhaseToolPolicy`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolPolicyOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
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
    known_builtin_tools(worker_type).unwrap_or(EDIT)
}

fn known_builtin_tools(worker_type: &str) -> Option<&'static [&'static str]> {
    match worker_type {
        "none" | "bench_t1" => Some(NONE),
        "coordinator" | "review" | "read_only" => Some(READ_ONLY),
        "test" => Some(TEST),
        "research" => Some(RESEARCH),
        "edit" | "execute" | "bench_t2" => Some(EDIT),
        // Unknown execution roles must never silently become no-tool workers.
        _ => None,
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

/// Resolves a phase's effective tools while preserving `base_tools` as a hard
/// capability ceiling. This is used at dispatch time: phase defaults are safe
/// by default, config profiles can tune those defaults, and an optional
/// benchmark policy can replace the requested set without gaining capabilities
/// the caller did not already grant.
#[must_use]
pub fn resolve_phase_tools(
    profiles: &BTreeMap<String, ToolProfile>,
    base_tools: &[String],
    phase: &str,
    default_profile: &str,
    policy: Option<&PhaseToolPolicy>,
) -> Vec<String> {
    let phase_override = policy.and_then(|policy| policy.phases.get(phase));
    let explicit_tools = phase_override
        .and_then(|override_| override_.allowed_tools.as_ref())
        .or_else(|| policy.and_then(|policy| policy.allowed_tools.as_ref()));
    let profile = phase_override
        .and_then(|override_| override_.profile.as_deref())
        .or_else(|| policy.and_then(|policy| policy.profile.as_deref()))
        .unwrap_or(default_profile);
    // Unlike general worker-role resolution, phase policies deliberately do
    // not fall through to `tool_profiles.default`: a broad global default
    // must not turn a planning phase into an edit-capable worker. Selecting
    // `profile = "default"` remains available as an explicit override.
    let requested = explicit_tools.cloned().unwrap_or_else(|| {
        profiles.get(profile).map_or_else(
            || {
                known_builtin_tools(profile)
                    .unwrap_or(NONE)
                    .iter()
                    .map(ToString::to_string)
                    .collect()
            },
            |configured| configured.allowed_tools.clone(),
        )
    });
    let base: BTreeSet<&str> = base_tools.iter().map(String::as_str).collect();
    requested
        .into_iter()
        .filter(|tool| base.contains(tool.as_str()))
        .collect()
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
    fn phase_defaults_are_intersected_with_the_base_ceiling() {
        let base = EDIT.iter().map(|tool| (*tool).into()).collect::<Vec<_>>();
        assert_eq!(
            resolve_phase_tools(&BTreeMap::new(), &base, "speccing", "read_only", None),
            READ_ONLY
        );

        let no_bash = base
            .into_iter()
            .filter(|tool| tool != "bash")
            .collect::<Vec<_>>();
        assert!(
            !resolve_phase_tools(&BTreeMap::new(), &no_bash, "execute", "execute", None,)
                .contains(&"bash".into())
        );
    }

    #[test]
    fn phase_policy_allows_overall_and_phase_specific_overrides() {
        let base = EDIT.iter().map(|tool| (*tool).into()).collect::<Vec<_>>();
        let policy = PhaseToolPolicy {
            allowed_tools: Some(vec!["read_file".into(), "glob".into()]),
            phases: BTreeMap::from([(
                "execute".into(),
                ToolPolicyOverride {
                    allowed_tools: Some(vec!["bash".into(), "edit_file".into()]),
                    ..ToolPolicyOverride::default()
                },
            )]),
            ..PhaseToolPolicy::default()
        };
        assert_eq!(
            resolve_phase_tools(
                &BTreeMap::new(),
                &base,
                "speccing",
                "read_only",
                Some(&policy),
            ),
            ["read_file", "glob"]
        );
        assert_eq!(
            resolve_phase_tools(&BTreeMap::new(), &base, "execute", "execute", Some(&policy),),
            ["bash", "edit_file"]
        );
    }

    #[test]
    fn phase_defaults_ignore_a_broad_global_default_profile() {
        let base = EDIT.iter().map(|tool| (*tool).into()).collect::<Vec<_>>();
        let profiles = BTreeMap::from([(
            "default".into(),
            ToolProfile {
                allowed_tools: EDIT.iter().map(|tool| (*tool).into()).collect(),
            },
        )]);
        assert_eq!(
            resolve_phase_tools(&profiles, &base, "speccing", "read_only", None),
            READ_ONLY
        );
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

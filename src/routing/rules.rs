use std::collections::HashMap;

use serde::Deserialize;

use crate::routing::profile::ToolProfile;

/// Legacy tool profile configuration.
///
/// `routing.toml` used to own both model routing rules and tool profiles.
/// Model routing now lives in `OrbConfig.models`; this type remains only as a
/// compatibility reader for old profile files.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RoutingConfig {
    /// Tool profiles keyed by worker type.
    #[serde(default)]
    pub profiles: HashMap<String, ToolProfile>,
}

impl RoutingConfig {
    /// Returns the tool profile for a worker type.
    /// Falls back to the "default" profile if no exact match exists.
    /// Returns `None` if neither the worker type nor "default" has a profile.
    pub fn profile_for(&self, worker_type: &str) -> Option<&ToolProfile> {
        self.profiles
            .get(worker_type)
            .or_else(|| self.profiles.get("default"))
    }

    /// Validates the config and returns a list of warnings.
    ///
    /// Checks for:
    /// - Profiles with empty `allowed_tools`
    pub fn validate(&self) -> Vec<String> {
        let mut warnings = Vec::new();

        for (name, profile) in &self.profiles {
            if profile.allowed_tools.is_empty() {
                warnings.push(format!(
                    "Profile '{name}' has empty allowed_tools — workers will have no tools"
                ));
            }
        }

        warnings
    }
}

/// Loads routing config from a TOML string.
///
/// # Errors
///
/// Returns an error if the TOML is invalid.
pub fn parse_routing_config(toml_str: &str) -> Result<RoutingConfig, toml::de::Error> {
    toml::from_str(toml_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let config = RoutingConfig::default();
        assert!(config.profiles.is_empty());
    }

    #[test]
    fn parse_profiles_from_toml() {
        let toml = r#"
[profiles.edit]
allowed_tools = ["read", "write", "execute"]
"#;

        let config = parse_routing_config(toml).unwrap();
        assert_eq!(
            config.profiles["edit"].allowed_tools,
            vec!["read", "write", "execute"]
        );
    }

    #[test]
    fn legacy_model_rules_are_ignored() {
        let toml = r#"
default_model = "openrouter/auto"

[[rules]]
worker_type = "edit"
model = "anthropic/claude-sonnet-4-20250514"
"#;
        let config = parse_routing_config(toml).unwrap();
        assert!(config.profiles.is_empty());
    }

    #[test]
    fn parse_config_with_profiles() {
        let toml = r#"
default_model = "openrouter/auto"

[[rules]]
worker_type = "edit"
model = "anthropic/claude-sonnet-4-20250514"

[profiles.edit]
allowed_tools = ["read", "write", "execute"]

[profiles.research]
allowed_tools = ["read", "web_search"]
"#;
        let config = parse_routing_config(toml).unwrap();
        assert_eq!(config.profiles.len(), 2);
        assert_eq!(
            config.profiles["edit"].allowed_tools,
            vec!["read", "write", "execute"]
        );
        assert_eq!(
            config.profiles["research"].allowed_tools,
            vec!["read", "web_search"]
        );
    }

    #[test]
    fn parse_config_without_profiles_backwards_compat() {
        let toml = r#"
default_model = "openrouter/auto"

[[rules]]
worker_type = "edit"
model = "anthropic/claude-sonnet-4-20250514"
"#;
        let config = parse_routing_config(toml).unwrap();
        assert!(config.profiles.is_empty());
    }

    #[test]
    fn default_config_has_empty_profiles() {
        let config = RoutingConfig::default();
        assert!(config.profiles.is_empty());
    }

    #[test]
    fn profile_for_exact_match() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "edit".into(),
            ToolProfile {
                allowed_tools: vec!["read".into(), "write".into()],
            },
        );
        profiles.insert(
            "default".into(),
            ToolProfile {
                allowed_tools: vec!["read".into()],
            },
        );
        let config = RoutingConfig {
            profiles,
            ..Default::default()
        };
        let profile = config.profile_for("edit").unwrap();
        assert_eq!(profile.allowed_tools, vec!["read", "write"]);
    }

    #[test]
    fn profile_for_fallback_to_default() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "default".into(),
            ToolProfile {
                allowed_tools: vec!["read".into()],
            },
        );
        let config = RoutingConfig {
            profiles,
            ..Default::default()
        };
        let profile = config.profile_for("unknown_type").unwrap();
        assert_eq!(profile.allowed_tools, vec!["read"]);
    }

    #[test]
    fn profile_for_no_match_no_default() {
        let config = RoutingConfig::default();
        assert!(config.profile_for("edit").is_none());
    }

    // --- validate tests ---

    #[test]
    fn validate_empty_config_clean() {
        let config = RoutingConfig::default();
        assert!(config.validate().is_empty());
    }

    #[test]
    fn validate_ignores_legacy_model_rules() {
        let config = RoutingConfig::default();
        assert!(config.validate().is_empty());
    }

    #[test]
    fn validate_empty_allowed_tools_warns() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "edit".into(),
            ToolProfile {
                allowed_tools: vec![],
            },
        );
        let config = RoutingConfig {
            profiles,
            ..Default::default()
        };
        let warnings = config.validate();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("edit"));
        assert!(warnings[0].contains("empty allowed_tools"));
    }

    #[test]
    fn validate_valid_config_clean() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "edit".into(),
            ToolProfile {
                allowed_tools: vec!["read".into(), "write".into()],
            },
        );
        profiles.insert(
            "research".into(),
            ToolProfile {
                allowed_tools: vec!["read".into(), "web_search".into()],
            },
        );
        let config = RoutingConfig { profiles };
        assert!(config.validate().is_empty());
    }
}

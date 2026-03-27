use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::routing::profile::ToolProfile;

/// Model routing configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingConfig {
    /// Default model when no rule matches.
    pub default_model: String,
    /// Routing rules, checked in order.
    #[serde(default)]
    pub rules: Vec<RoutingRule>,
    /// Tool profiles keyed by worker type.
    #[serde(default)]
    pub profiles: HashMap<String, ToolProfile>,
}

/// A single routing rule mapping a worker type to a model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingRule {
    /// Worker type to match (e.g., "research", "edit", "review", "test").
    pub worker_type: String,
    /// Model to use for this worker type.
    pub model: String,
    /// Optional reason for this routing choice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl RoutingConfig {
    /// Returns the model for a given worker type.
    /// Falls back to `default_model` if no rule matches.
    pub fn model_for(&self, worker_type: &str) -> &str {
        self.rules
            .iter()
            .find(|r| r.worker_type == worker_type)
            .map_or(&self.default_model, |r| &r.model)
    }

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
    /// - Rules whose `worker_type` has no matching profile and no "default" fallback
    /// - Profiles with empty `allowed_tools`
    pub fn validate(&self) -> Vec<String> {
        let mut warnings = Vec::new();

        for rule in &self.rules {
            if self.profile_for(&rule.worker_type).is_none() {
                warnings.push(format!(
                    "Rule for worker type '{}' has no matching profile and no 'default' profile",
                    rule.worker_type
                ));
            }
        }

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

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            default_model: "openrouter/free".into(),
            rules: vec![],
            profiles: HashMap::new(),
        }
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
        assert_eq!(config.model_for("anything"), "openrouter/free");
    }

    #[test]
    fn model_for_matching_type() {
        let config = RoutingConfig {
            default_model: "openrouter/auto".into(),
            rules: vec![
                RoutingRule {
                    worker_type: "research".into(),
                    model: "google/gemini-2.0-flash-001".into(),
                    reason: Some("cheap".into()),
                },
                RoutingRule {
                    worker_type: "edit".into(),
                    model: "anthropic/claude-sonnet-4-20250514".into(),
                    reason: None,
                },
            ],
            ..Default::default()
        };

        assert_eq!(config.model_for("research"), "google/gemini-2.0-flash-001");
        assert_eq!(
            config.model_for("edit"),
            "anthropic/claude-sonnet-4-20250514"
        );
        assert_eq!(config.model_for("test"), "openrouter/auto"); // falls back
    }

    #[test]
    fn parse_from_toml() {
        let toml = r#"
default_model = "openrouter/auto"

[[rules]]
worker_type = "research"
model = "google/gemini-2.0-flash-001"
reason = "cheap, fast"

[[rules]]
worker_type = "edit"
model = "anthropic/claude-sonnet-4-20250514"

[[rules]]
worker_type = "review"
model = "google/gemini-2.0-flash-001"
reason = "cheap"
"#;

        let config = parse_routing_config(toml).unwrap();
        assert_eq!(config.default_model, "openrouter/auto");
        assert_eq!(config.rules.len(), 3);
        assert_eq!(config.model_for("research"), "google/gemini-2.0-flash-001");
        assert_eq!(
            config.model_for("edit"),
            "anthropic/claude-sonnet-4-20250514"
        );
        assert_eq!(config.model_for("review"), "google/gemini-2.0-flash-001");
        assert_eq!(config.model_for("unknown"), "openrouter/auto");
    }

    #[test]
    fn round_trip_toml() {
        let config = RoutingConfig {
            default_model: "test/model".into(),
            rules: vec![RoutingRule {
                worker_type: "edit".into(),
                model: "better/model".into(),
                reason: Some("because".into()),
            }],
            ..Default::default()
        };
        let serialized = toml::to_string(&config).unwrap();
        let parsed: RoutingConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed.default_model, "test/model");
        assert_eq!(parsed.rules.len(), 1);
        assert_eq!(parsed.model_for("edit"), "better/model");
    }

    #[test]
    fn empty_rules_uses_default() {
        let toml = r#"default_model = "openrouter/free""#;
        let config = parse_routing_config(toml).unwrap();
        assert_eq!(config.model_for("anything"), "openrouter/free");
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
    fn validate_rule_without_profile_warns() {
        let config = RoutingConfig {
            rules: vec![RoutingRule {
                worker_type: "edit".into(),
                model: "test/model".into(),
                reason: None,
            }],
            ..Default::default()
        };
        let warnings = config.validate();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("edit"));
        assert!(warnings[0].contains("no matching profile"));
    }

    #[test]
    fn validate_matching_profile_clean() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "edit".into(),
            ToolProfile {
                allowed_tools: vec!["read".into()],
            },
        );
        let config = RoutingConfig {
            rules: vec![RoutingRule {
                worker_type: "edit".into(),
                model: "test/model".into(),
                reason: None,
            }],
            profiles,
            ..Default::default()
        };
        assert!(config.validate().is_empty());
    }

    #[test]
    fn validate_default_fallback_clean() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "default".into(),
            ToolProfile {
                allowed_tools: vec!["read".into()],
            },
        );
        let config = RoutingConfig {
            rules: vec![RoutingRule {
                worker_type: "edit".into(),
                model: "test/model".into(),
                reason: None,
            }],
            profiles,
            ..Default::default()
        };
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
        let config = RoutingConfig {
            rules: vec![
                RoutingRule {
                    worker_type: "edit".into(),
                    model: "test/model".into(),
                    reason: None,
                },
                RoutingRule {
                    worker_type: "research".into(),
                    model: "test/model".into(),
                    reason: None,
                },
            ],
            profiles,
            ..Default::default()
        };
        assert!(config.validate().is_empty());
    }
}

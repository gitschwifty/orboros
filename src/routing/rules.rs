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
}

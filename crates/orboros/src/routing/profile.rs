use serde::{Deserialize, Serialize};

/// A tool profile defining which tools a worker type is allowed to use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolProfile {
    /// Tools the worker is allowed to use.
    pub allowed_tools: Vec<String>,
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
}

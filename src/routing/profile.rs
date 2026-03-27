use serde::{Deserialize, Serialize};

/// A tool profile defining which tools a worker type is allowed to use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolProfile {
    /// Tools the worker is allowed to use.
    pub allowed_tools: Vec<String>,
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
}

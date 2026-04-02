use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::id::OrbId;

/// Type of dependency relationship between two orbs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeType {
    /// `from` blocks `to` — `to` cannot start until `from` is done.
    Blocks,
    /// `from` depends on `to` — `from` cannot start until `to` is done.
    DependsOn,
    /// `from` is the parent of `to`.
    Parent,
    /// `from` is a child of `to`.
    Child,
    /// Informational link — no scheduling constraint.
    Related,
    /// `from` duplicates `to`.
    Duplicates,
    /// `from` should be done after `to` (soft ordering).
    Follows,
}

impl EdgeType {
    /// Returns true if this edge type implies a hard scheduling dependency
    /// (the target must be done before the source can proceed).
    pub fn is_blocking(&self) -> bool {
        matches!(self, Self::Blocks | Self::DependsOn)
    }
}

/// A directed dependency edge between two orbs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepEdge {
    /// Source orb.
    pub from: OrbId,
    /// Target orb.
    pub to: OrbId,
    /// Type of relationship.
    pub edge_type: EdgeType,
    /// When this edge was created.
    pub created_at: DateTime<Utc>,
    /// Soft-delete timestamp. If set, this edge is considered removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_at: Option<DateTime<Utc>>,
}

impl DepEdge {
    /// Creates a new dependency edge.
    pub fn new(from: OrbId, to: OrbId, edge_type: EdgeType) -> Self {
        Self {
            from,
            to,
            edge_type,
            created_at: Utc::now(),
            removed_at: None,
        }
    }

    /// Returns true if this edge has been soft-deleted.
    pub fn is_removed(&self) -> bool {
        self.removed_at.is_some()
    }

    /// Marks this edge as removed.
    pub fn remove(&mut self) {
        self.removed_at = Some(Utc::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_type_serde_round_trip() {
        for edge_type in [
            EdgeType::Blocks,
            EdgeType::DependsOn,
            EdgeType::Parent,
            EdgeType::Child,
            EdgeType::Related,
            EdgeType::Duplicates,
            EdgeType::Follows,
        ] {
            let json = serde_json::to_string(&edge_type).unwrap();
            let parsed: EdgeType = serde_json::from_str(&json).unwrap();
            assert_eq!(edge_type, parsed);
        }
    }

    #[test]
    fn edge_type_snake_case_serialization() {
        assert_eq!(
            serde_json::to_string(&EdgeType::Blocks).unwrap(),
            "\"blocks\""
        );
        assert_eq!(
            serde_json::to_string(&EdgeType::DependsOn).unwrap(),
            "\"depends_on\""
        );
        assert_eq!(
            serde_json::to_string(&EdgeType::Parent).unwrap(),
            "\"parent\""
        );
        assert_eq!(
            serde_json::to_string(&EdgeType::Child).unwrap(),
            "\"child\""
        );
        assert_eq!(
            serde_json::to_string(&EdgeType::Related).unwrap(),
            "\"related\""
        );
        assert_eq!(
            serde_json::to_string(&EdgeType::Duplicates).unwrap(),
            "\"duplicates\""
        );
        assert_eq!(
            serde_json::to_string(&EdgeType::Follows).unwrap(),
            "\"follows\""
        );
    }

    #[test]
    fn dep_edge_serde_round_trip() {
        let edge = DepEdge::new(
            OrbId::from_raw("orb-aaa"),
            OrbId::from_raw("orb-bbb"),
            EdgeType::Blocks,
        );
        let json = serde_json::to_string(&edge).unwrap();
        let parsed: DepEdge = serde_json::from_str(&json).unwrap();
        assert_eq!(edge.from, parsed.from);
        assert_eq!(edge.to, parsed.to);
        assert_eq!(edge.edge_type, parsed.edge_type);
        assert!(parsed.removed_at.is_none());
    }

    #[test]
    fn dep_edge_removed_serde_round_trip() {
        let mut edge = DepEdge::new(
            OrbId::from_raw("orb-aaa"),
            OrbId::from_raw("orb-bbb"),
            EdgeType::DependsOn,
        );
        edge.remove();
        assert!(edge.is_removed());

        let json = serde_json::to_string(&edge).unwrap();
        let parsed: DepEdge = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_removed());
    }

    #[test]
    fn new_edge_is_not_removed() {
        let edge = DepEdge::new(
            OrbId::from_raw("orb-x"),
            OrbId::from_raw("orb-y"),
            EdgeType::Related,
        );
        assert!(!edge.is_removed());
    }

    #[test]
    fn is_blocking_for_hard_deps() {
        assert!(EdgeType::Blocks.is_blocking());
        assert!(EdgeType::DependsOn.is_blocking());
        assert!(!EdgeType::Parent.is_blocking());
        assert!(!EdgeType::Child.is_blocking());
        assert!(!EdgeType::Related.is_blocking());
        assert!(!EdgeType::Duplicates.is_blocking());
        assert!(!EdgeType::Follows.is_blocking());
    }
}

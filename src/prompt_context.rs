use std::fmt::Write;

use orbs::dep::{DepEdge, EdgeType};
use orbs::id::OrbId;
use orbs::orb::Orb;

const FIELD_MAX_CHARS: usize = 1_200;
const RESULT_MAX_CHARS: usize = 800;
const LIST_MAX_ITEMS: usize = 8;

/// Builds task-specific context for an orb worker invocation.
///
/// Heddle owns project-level context such as `AGENTS.md`. This helper
/// only injects Orboros task context: the current orb, nearby tree
/// relationships, and dependency status/results.
#[must_use]
pub fn build_orb_task_context(orb: &Orb, all_orbs: &[Orb], edges: &[DepEdge]) -> String {
    let mut out = String::from("## Orboros Task Context\n\n");
    push_current_orb(&mut out, orb);
    push_parent_and_root(&mut out, orb, all_orbs);
    push_siblings(&mut out, orb, all_orbs);
    push_children(&mut out, orb, all_orbs);
    push_upstream_dependencies(&mut out, orb, all_orbs, edges);
    out
}

/// Appends a task-context block after the base user prompt.
#[must_use]
pub fn append_task_context(user_prompt: &str, context: &str) -> String {
    if context.trim().is_empty() {
        return user_prompt.to_string();
    }
    format!("{user_prompt}\n\n---\n\n{context}")
}

fn push_current_orb(out: &mut String, orb: &Orb) {
    let _ = writeln!(out, "### Current Orb");
    push_orb_summary(out, orb, true);
    out.push('\n');
}

fn push_parent_and_root(out: &mut String, orb: &Orb, all_orbs: &[Orb]) {
    if let Some(parent) = orb.parent_id.as_ref().and_then(|id| find_orb(all_orbs, id)) {
        let _ = writeln!(out, "### Parent Orb");
        push_orb_summary(out, parent, true);
        out.push('\n');
    }

    if let Some(root) = orb
        .root_id
        .as_ref()
        .filter(|id| Some(*id) != orb.parent_id.as_ref())
        .and_then(|id| find_orb(all_orbs, id))
    {
        let _ = writeln!(out, "### Root Orb");
        push_orb_summary(out, root, false);
        out.push('\n');
    }
}

fn push_siblings(out: &mut String, orb: &Orb, all_orbs: &[Orb]) {
    let Some(parent_id) = orb.parent_id.as_ref() else {
        return;
    };
    let siblings: Vec<&Orb> = all_orbs
        .iter()
        .filter(|candidate| {
            candidate.id != orb.id && candidate.parent_id.as_ref() == Some(parent_id)
        })
        .take(LIST_MAX_ITEMS)
        .collect();
    if siblings.is_empty() {
        return;
    }

    let _ = writeln!(out, "### Sibling Orbs");
    for sibling in siblings {
        let _ = writeln!(
            out,
            "- {} [{}] {}",
            sibling.id,
            status_label(sibling),
            sibling.title
        );
    }
    out.push('\n');
}

fn push_children(out: &mut String, orb: &Orb, all_orbs: &[Orb]) {
    let children: Vec<&Orb> = all_orbs
        .iter()
        .filter(|candidate| candidate.parent_id.as_ref() == Some(&orb.id))
        .take(LIST_MAX_ITEMS)
        .collect();
    if children.is_empty() {
        return;
    }

    let _ = writeln!(out, "### Child Orbs");
    for child in children {
        let _ = writeln!(
            out,
            "- {} [{}] {}",
            child.id,
            status_label(child),
            child.title
        );
        if let Some(result) = child.result.as_deref().filter(|s| !s.trim().is_empty()) {
            let _ = writeln!(out, "  Result: {}", truncate(result, RESULT_MAX_CHARS));
        }
    }
    out.push('\n');
}

fn push_upstream_dependencies(out: &mut String, orb: &Orb, all_orbs: &[Orb], edges: &[DepEdge]) {
    let upstream: Vec<(&DepEdge, &Orb)> = edges
        .iter()
        .filter_map(|edge| {
            upstream_id_for(edge, &orb.id).and_then(|id| find_orb(all_orbs, id).map(|o| (edge, o)))
        })
        .take(LIST_MAX_ITEMS)
        .collect();
    if upstream.is_empty() {
        return;
    }

    let _ = writeln!(out, "### Upstream Dependencies");
    for (edge, dependency) in upstream {
        let _ = writeln!(
            out,
            "- {} via {:?} [{}] {}",
            dependency.id,
            edge.edge_type,
            status_label(dependency),
            dependency.title
        );
        if let Some(result) = dependency
            .result
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            let _ = writeln!(out, "  Result: {}", truncate(result, RESULT_MAX_CHARS));
        }
    }
    out.push('\n');
}

fn upstream_id_for<'a>(edge: &'a DepEdge, orb_id: &OrbId) -> Option<&'a OrbId> {
    match edge.edge_type {
        EdgeType::Blocks if edge.to == *orb_id => Some(&edge.from),
        EdgeType::DependsOn | EdgeType::Follows if edge.from == *orb_id => Some(&edge.to),
        _ => None,
    }
}

fn push_orb_summary(out: &mut String, orb: &Orb, include_spec: bool) {
    let _ = writeln!(out, "- id: {}", orb.id);
    let _ = writeln!(out, "- title: {}", orb.title);
    let _ = writeln!(out, "- type: {:?}", orb.orb_type);
    let _ = writeln!(out, "- status: {}", status_label(orb));
    let _ = writeln!(out, "- priority: {}", orb.priority);
    if !orb.labels.is_empty() {
        let _ = writeln!(out, "- labels: {}", orb.labels.join(", "));
    }
    let _ = writeln!(
        out,
        "- description: {}",
        truncate(&orb.description, FIELD_MAX_CHARS)
    );
    if include_spec {
        if let Some(design) = orb.design.as_deref().filter(|s| !s.trim().is_empty()) {
            let _ = writeln!(out, "- design: {}", truncate(design, FIELD_MAX_CHARS));
        }
        if let Some(ac) = orb
            .acceptance_criteria
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            let _ = writeln!(
                out,
                "- acceptance_criteria: {}",
                truncate(ac, FIELD_MAX_CHARS)
            );
        }
    }
}

fn find_orb<'a>(all_orbs: &'a [Orb], id: &OrbId) -> Option<&'a Orb> {
    all_orbs.iter().find(|orb| orb.id == *id)
}

fn status_label(orb: &Orb) -> String {
    if let Some(status) = orb.status {
        format!("{status:?}")
    } else if let Some(phase) = orb.phase {
        format!("{phase:?}")
    } else {
        "unknown".to_string()
    }
}

fn truncate(input: &str, max_chars: usize) -> String {
    let trimmed = input.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(max_chars).collect();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use orbs::dep::DepEdge;
    use orbs::orb::{OrbStatus, OrbType};

    use super::*;

    #[test]
    fn context_includes_current_parent_sibling_and_dependency() {
        let root = Orb::new("Epic", "Root work").with_type(OrbType::Epic);
        let mut parent = Orb::new("Feature", "Parent work").with_type(OrbType::Feature);
        parent.parent_id = Some(root.id.clone());
        parent.root_id = Some(root.id.clone());
        let mut current = Orb::new("Task", "Current work").with_type(OrbType::Task);
        current.parent_id = Some(parent.id.clone());
        current.root_id = Some(root.id.clone());
        current.design = Some("Use the existing queue path".into());
        current.acceptance_criteria = Some("- [ ] context is present".into());
        let mut sibling = Orb::new("Sibling", "Other work").with_type(OrbType::Task);
        sibling.parent_id = Some(parent.id.clone());
        sibling.root_id = Some(root.id.clone());
        let mut blocker = Orb::new("Blocker", "First work").with_type(OrbType::Task);
        blocker.set_status(OrbStatus::Active).unwrap();
        blocker.set_status(OrbStatus::Done).unwrap();
        blocker.result = Some("Blocker finished".into());
        let edge = DepEdge::new(blocker.id.clone(), current.id.clone(), EdgeType::Blocks);

        let context = build_orb_task_context(
            &current,
            &[root, parent, current.clone(), sibling, blocker],
            &[edge],
        );

        assert!(context.contains("### Current Orb"));
        assert!(context.contains("Current work"));
        assert!(context.contains("### Parent Orb"));
        assert!(context.contains("Parent work"));
        assert!(context.contains("### Root Orb"));
        assert!(context.contains("Root work"));
        assert!(context.contains("### Sibling Orbs"));
        assert!(context.contains("Sibling"));
        assert!(context.contains("### Upstream Dependencies"));
        assert!(context.contains("Blocker finished"));
        assert!(context.contains("acceptance_criteria"));
    }

    #[test]
    fn append_context_keeps_base_prompt_first() {
        let combined = append_task_context("Do the task", "## Context");
        assert!(combined.starts_with("Do the task\n\n---"));
        assert!(combined.contains("## Context"));
    }
}

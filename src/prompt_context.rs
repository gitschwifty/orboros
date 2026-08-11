use std::fmt::Write;

use orbs::dep::{DepEdge, EdgeType};
use orbs::id::OrbId;
use orbs::orb::Orb;

use crate::execution::PromptContextMetrics;

const FIELD_MAX_CHARS: usize = 1_200;
const RESULT_MAX_CHARS: usize = 800;
const LIST_MAX_ITEMS: usize = 8;

/// A deterministic context ceiling for one dispatch role. The ceiling applies
/// only to task context constructed by Orboros; Heddle/provider context is
/// intentionally outside this accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    pub max_chars: usize,
}

pub const CHILD_EXECUTION_CONTEXT_BUDGET: ContextBudget = ContextBudget { max_chars: 8_000 };
pub const REVIEW_CONTEXT_BUDGET: ContextBudget = ContextBudget { max_chars: 6_000 };
pub const PARENT_FINAL_CONTEXT_BUDGET: ContextBudget = ContextBudget { max_chars: 10_000 };

/// Builds task-specific context for an orb worker invocation.
///
/// Heddle owns project-level context such as `AGENTS.md`. This helper
/// only injects Orboros task context: the current orb, nearby tree
/// relationships, and dependency status/results.
#[must_use]
pub fn build_orb_task_context(orb: &Orb, all_orbs: &[Orb], edges: &[DepEdge]) -> String {
    build_orb_task_context_with_metrics(orb, all_orbs, edges).text
}

/// Task context plus a character-level attribution for each injected source.
#[derive(Debug, Clone)]
pub struct BuiltTaskContext {
    pub text: String,
    pub metrics: PromptContextMetrics,
}

/// Builds task context and records the exact character contribution of each
/// Orboros-owned source. Provider/runtime context is intentionally outside
/// this measurement.
#[must_use]
pub fn build_orb_task_context_with_metrics(
    orb: &Orb,
    all_orbs: &[Orb],
    edges: &[DepEdge],
) -> BuiltTaskContext {
    build_orb_task_context_with_budget(
        orb,
        all_orbs,
        edges,
        ContextBudget {
            max_chars: usize::MAX,
        },
    )
}

/// Builds bounded task context. Sections are selected in caller-supplied
/// priority order so a large low-priority sibling/child summary cannot crowd
/// out the current task or direct dependency evidence.
#[must_use]
pub fn build_orb_task_context_with_budget(
    orb: &Orb,
    all_orbs: &[Orb],
    edges: &[DepEdge],
    budget: ContextBudget,
) -> BuiltTaskContext {
    let mut out = String::from("## Orboros Task Context\n\n");
    let task_context_overhead_chars = char_count(&out);
    let current = rendered(|out| push_current_orb(out, orb));
    let parent = rendered(|out| push_parent_and_root(out, orb, all_orbs));
    let siblings = rendered(|out| push_siblings(out, orb, all_orbs));
    let children = rendered(|out| push_children(out, orb, all_orbs));
    let dependencies = rendered(|out| push_upstream_dependencies(out, orb, all_orbs, edges));
    let current_orb_chars = append_bounded(&mut out, &current, budget.max_chars);
    let upstream_dependency_chars = append_bounded(&mut out, &dependencies, budget.max_chars);
    let parent_and_root_chars = append_bounded(&mut out, &parent, budget.max_chars);
    let child_orbs_chars = append_bounded(&mut out, &children, budget.max_chars);
    let sibling_orbs_chars = append_bounded(&mut out, &siblings, budget.max_chars);
    let task_context_chars = char_count(&out);
    BuiltTaskContext {
        text: out,
        metrics: PromptContextMetrics {
            task_context_chars,
            task_context_overhead_chars,
            current_orb_chars,
            parent_and_root_chars,
            sibling_orbs_chars,
            child_orbs_chars,
            upstream_dependency_chars,
            ..PromptContextMetrics::default()
        },
    }
}

fn rendered(append: impl FnOnce(&mut String)) -> String {
    let mut text = String::new();
    append(&mut text);
    text
}

fn append_bounded(out: &mut String, section: &str, max_chars: usize) -> u32 {
    let used = out.chars().count();
    let remaining = max_chars.saturating_sub(used);
    if remaining == 0 || section.is_empty() {
        return 0;
    }
    if section.chars().count() <= remaining {
        out.push_str(section);
    } else {
        let marker = "\n[Orboros task context truncated by dispatch budget]\n";
        let marker_chars = marker.chars().count();
        if remaining <= marker_chars {
            out.extend(marker.chars().take(remaining));
        } else {
            out.extend(section.chars().take(remaining - marker_chars));
            out.push_str(marker);
        }
    }
    u32::try_from(out.chars().count().saturating_sub(used)).unwrap_or(u32::MAX)
}

fn char_count(value: &str) -> u32 {
    u32::try_from(value.chars().count()).unwrap_or(u32::MAX)
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
    fn context_metrics_attribute_each_injected_source() {
        let root = Orb::new("Root", "Root work").with_type(OrbType::Feature);
        let mut current = Orb::new("Current", "Current work").with_type(OrbType::Task);
        current.parent_id = Some(root.id.clone());
        current.root_id = Some(root.id.clone());
        let mut sibling = Orb::new("Sibling", "Sibling work").with_type(OrbType::Task);
        sibling.parent_id = Some(root.id.clone());
        let mut child = Orb::new("Child", "Child work").with_type(OrbType::Task);
        child.parent_id = Some(current.id.clone());
        child.result = Some("Child result".into());
        let dependency = Orb::new("Dependency", "Dependency work").with_type(OrbType::Task);
        let edge = DepEdge::new(
            current.id.clone(),
            dependency.id.clone(),
            EdgeType::DependsOn,
        );

        let context = build_orb_task_context_with_metrics(
            &current,
            &[root, current.clone(), sibling, child, dependency],
            &[edge],
        );

        assert!(context.metrics.current_orb_chars > 0);
        assert!(context.metrics.parent_and_root_chars > 0);
        assert!(context.metrics.sibling_orbs_chars > 0);
        assert!(context.metrics.child_orbs_chars > 0);
        assert!(context.metrics.upstream_dependency_chars > 0);
        assert_eq!(
            context.metrics.task_context_chars,
            u32::try_from(context.text.chars().count()).unwrap()
        );
    }

    #[test]
    fn append_context_keeps_base_prompt_first() {
        let combined = append_task_context("Do the task", "## Context");
        assert!(combined.starts_with("Do the task\n\n---"));
        assert!(combined.contains("## Context"));
    }

    #[test]
    fn bounded_context_keeps_current_and_dependency_before_large_child_results() {
        let mut parent = Orb::new("Parent", "Parent work").with_type(OrbType::Feature);
        parent.has_parent_final_work = true;
        let mut dependency = Orb::new("Dependency", "Required API").with_type(OrbType::Task);
        dependency.set_status(OrbStatus::Active).unwrap();
        dependency.set_status(OrbStatus::Done).unwrap();
        dependency.result = Some("verification passed".into());
        let mut child = Orb::new("Child", "Large implementation").with_type(OrbType::Task);
        child.parent_id = Some(parent.id.clone());
        child.result = Some("x".repeat(RESULT_MAX_CHARS));
        let edge = DepEdge::new(dependency.id.clone(), parent.id.clone(), EdgeType::Blocks);

        let context = build_orb_task_context_with_budget(
            &parent,
            &[parent.clone(), dependency, child],
            &[edge],
            ContextBudget { max_chars: 800 },
        );

        assert!(context.text.contains("### Current Orb"));
        assert!(context.text.contains("Dependency"), "{}", context.text);
        assert!(context.text.contains("truncated by dispatch budget"));
        assert!(usize::try_from(context.metrics.task_context_chars).unwrap() <= 800);
    }
}

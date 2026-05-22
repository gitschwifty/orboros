//! Hook matcher evaluation — given a `HookEvent`, an `Orb`, and ambient
//! context, return the hooks that should fire (in source order).
//!
//! Match fields are AND-combined: a hook with multiple constraints
//! matches only when every constraint passes. Unset (None) fields don't
//! constrain. Empty Some([]) lists are treated as "never matches" for
//! list-type filters — a defensive interpretation; the loader already
//! validates that lists are non-empty in practice.

use orbs::orb::Orb;

use crate::hooks::config::{HookEntry, HookMatch, HooksConfig};
use crate::hooks::event::HookEvent;

/// Ambient context that flows alongside the orb to give matchers
/// access to non-orb fields (worker type, model, etc.). Cheap to
/// build at each firing site.
#[derive(Debug, Default, Clone)]
pub struct MatcherCtx<'a> {
    pub orb: Option<&'a Orb>,
    pub worker_type: Option<&'a str>,
}

impl<'a> MatcherCtx<'a> {
    #[must_use]
    pub fn for_orb(orb: &'a Orb) -> Self {
        Self {
            orb: Some(orb),
            worker_type: None,
        }
    }

    #[must_use]
    pub fn with_worker_type(mut self, wt: &'a str) -> Self {
        self.worker_type = Some(wt);
        self
    }
}

impl HooksConfig {
    /// Returns the hooks targeting `event` whose match rules accept `ctx`,
    /// in their declared source order (global before project).
    pub fn matching<'a>(&'a self, event: HookEvent, ctx: &MatcherCtx<'_>) -> Vec<&'a HookEntry> {
        self.enabled_for(event)
            .filter(|h| matches(&h.match_rules, ctx))
            .collect()
    }
}

/// Evaluates a single matcher against the context. Top-level so the
/// logic is independently testable.
#[must_use]
pub fn matches(rule: &HookMatch, ctx: &MatcherCtx<'_>) -> bool {
    let Some(orb) = ctx.orb else {
        // No orb context: only hooks with no orb-specific constraints
        // can match. Pipeline-level events (on-pipeline-*, on-queue-tick)
        // hit this path.
        return is_orb_agnostic(rule);
    };

    if let Some(allowed) = &rule.orb_type {
        if !allowed.iter().any(|t| t == &orb.orb_type) {
            return false;
        }
    }

    if let Some(any_of) = &rule.labels_any {
        if !any_of.iter().any(|l| orb.labels.iter().any(|got| got == l)) {
            return false;
        }
    }

    if let Some(all_of) = &rule.labels_all {
        if !all_of.iter().all(|l| orb.labels.iter().any(|got| got == l)) {
            return false;
        }
    }

    if let Some(allowed) = &rule.status {
        match orb.status {
            Some(s) if allowed.iter().any(|x| x == &s) => {}
            _ => return false,
        }
    }

    if let Some(allowed) = &rule.phase {
        match orb.phase {
            Some(p) if allowed.iter().any(|x| x == &p) => {}
            _ => return false,
        }
    }

    if let Some(max) = rule.priority_max {
        if orb.priority > max {
            return false;
        }
    }
    if let Some(min) = rule.priority_min {
        if orb.priority < min {
            return false;
        }
    }

    if let Some(expected_wt) = &rule.worker_type {
        match ctx.worker_type {
            Some(got) if got == expected_wt => {}
            _ => return false,
        }
    }

    if let Some(re) = &rule.title_regex {
        if !re.is_match(&orb.title) {
            return false;
        }
    }
    if let Some(re) = &rule.description_regex {
        if !re.is_match(&orb.description) {
            return false;
        }
    }

    if let Some(required) = &rule.scope_includes {
        if !required
            .iter()
            .all(|s| orb.scope.iter().any(|got| got == s))
        {
            return false;
        }
    }

    true
}

/// True if `rule` doesn't reference any orb field. Pipeline-level
/// events evaluate matchers without an orb context.
fn is_orb_agnostic(rule: &HookMatch) -> bool {
    rule.orb_type.is_none()
        && rule.labels_any.is_none()
        && rule.labels_all.is_none()
        && rule.status.is_none()
        && rule.phase.is_none()
        && rule.priority_max.is_none()
        && rule.priority_min.is_none()
        && rule.title_regex.is_none()
        && rule.description_regex.is_none()
        && rule.scope_includes.is_none()
    // worker_type can apply orb-agnostically (pipeline event for a
    // specific worker type) — let it through.
}

#[cfg(test)]
mod tests {
    use super::*;
    use orbs::orb::{Orb, OrbType};
    use std::fs;
    use tempfile::TempDir;

    fn load_config(body: &str) -> (TempDir, HooksConfig) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.toml");
        fs::write(&path, body).unwrap();
        let cfg = HooksConfig::load(None, Some(&path)).unwrap();
        (dir, cfg)
    }

    fn make_orb() -> Orb {
        Orb::new("Reset auth tokens", "Rotate the JWT signing key")
            .with_type(OrbType::Task)
            .with_priority(2)
    }

    #[test]
    fn no_constraints_matches_every_orb() {
        let (_dir, cfg) = load_config(
            r#"
            [[hook]]
            on = "post-worker-complete"
            run = "x"
        "#,
        );
        let orb = make_orb();
        let m = cfg.matching(HookEvent::PostWorkerComplete, &MatcherCtx::for_orb(&orb));
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn orb_type_filter() {
        let (_dir, cfg) = load_config(
            r#"
            [[hook]]
            name = "task-only"
            on = "post-worker-complete"
            run = "x"
            match.orb_type = "task"

            [[hook]]
            name = "bug-only"
            on = "post-worker-complete"
            run = "x"
            match.orb_type = "bug"
        "#,
        );
        let orb = make_orb(); // type = Task
        let names: Vec<_> = cfg
            .matching(HookEvent::PostWorkerComplete, &MatcherCtx::for_orb(&orb))
            .iter()
            .map(|h| h.name.as_str())
            .collect();
        assert_eq!(names, vec!["task-only"]);
    }

    #[test]
    fn labels_any_matches_when_orb_has_one() {
        let (_dir, cfg) = load_config(
            r#"
            [[hook]]
            on = "post-worker-complete"
            run = "x"
            match.labels = ["db", "security"]
        "#,
        );
        let mut orb = make_orb();
        orb.labels = vec!["db".into()];
        let m = cfg.matching(HookEvent::PostWorkerComplete, &MatcherCtx::for_orb(&orb));
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn labels_any_rejects_when_no_overlap() {
        let (_dir, cfg) = load_config(
            r#"
            [[hook]]
            on = "post-worker-complete"
            run = "x"
            match.labels = ["db", "security"]
        "#,
        );
        let mut orb = make_orb();
        orb.labels = vec!["docs".into()];
        let m = cfg.matching(HookEvent::PostWorkerComplete, &MatcherCtx::for_orb(&orb));
        assert!(m.is_empty());
    }

    #[test]
    fn labels_all_requires_every_label() {
        let (_dir, cfg) = load_config(
            r#"
            [[hook]]
            on = "post-worker-complete"
            run = "x"
            match.labels_all = ["security", "external-input"]
        "#,
        );
        let mut orb = make_orb();
        orb.labels = vec!["security".into()];
        // Missing external-input → no match.
        assert!(cfg
            .matching(HookEvent::PostWorkerComplete, &MatcherCtx::for_orb(&orb))
            .is_empty());

        orb.labels.push("external-input".into());
        assert_eq!(
            cfg.matching(HookEvent::PostWorkerComplete, &MatcherCtx::for_orb(&orb))
                .len(),
            1
        );
    }

    #[test]
    fn priority_max_inclusive() {
        let (_dir, cfg) = load_config(
            r#"
            [[hook]]
            on = "post-worker-complete"
            run = "x"
            match.priority_max = 2
        "#,
        );
        let mut orb = make_orb();
        orb.priority = 2;
        assert_eq!(
            cfg.matching(HookEvent::PostWorkerComplete, &MatcherCtx::for_orb(&orb))
                .len(),
            1
        );
        orb.priority = 3;
        assert!(cfg
            .matching(HookEvent::PostWorkerComplete, &MatcherCtx::for_orb(&orb))
            .is_empty());
    }

    #[test]
    fn priority_min_inclusive() {
        let (_dir, cfg) = load_config(
            r#"
            [[hook]]
            on = "post-worker-complete"
            run = "x"
            match.priority_min = 4
        "#,
        );
        let mut orb = make_orb();
        orb.priority = 4;
        assert_eq!(
            cfg.matching(HookEvent::PostWorkerComplete, &MatcherCtx::for_orb(&orb))
                .len(),
            1
        );
        orb.priority = 3;
        assert!(cfg
            .matching(HookEvent::PostWorkerComplete, &MatcherCtx::for_orb(&orb))
            .is_empty());
    }

    #[test]
    fn worker_type_must_match_exactly() {
        let (_dir, cfg) = load_config(
            r#"
            [[hook]]
            on = "post-worker-complete"
            run = "x"
            match.worker_type = "edit"
        "#,
        );
        let orb = make_orb();
        let edit_ctx = MatcherCtx::for_orb(&orb).with_worker_type("edit");
        let review_ctx = MatcherCtx::for_orb(&orb).with_worker_type("review");
        let no_ctx = MatcherCtx::for_orb(&orb);

        assert_eq!(
            cfg.matching(HookEvent::PostWorkerComplete, &edit_ctx).len(),
            1
        );
        assert!(cfg
            .matching(HookEvent::PostWorkerComplete, &review_ctx)
            .is_empty());
        // No worker_type in ctx + worker_type in matcher → no match.
        assert!(cfg
            .matching(HookEvent::PostWorkerComplete, &no_ctx)
            .is_empty());
    }

    #[test]
    fn title_regex_matches() {
        let (_dir, cfg) = load_config(
            r#"
            [[hook]]
            on = "post-worker-complete"
            run = "x"
            match.title_regex = "(?i)migration"
        "#,
        );
        let mut orb = make_orb();
        orb.title = "DB Migration plan".into();
        assert_eq!(
            cfg.matching(HookEvent::PostWorkerComplete, &MatcherCtx::for_orb(&orb))
                .len(),
            1
        );
        orb.title = "rename a variable".into();
        assert!(cfg
            .matching(HookEvent::PostWorkerComplete, &MatcherCtx::for_orb(&orb))
            .is_empty());
    }

    #[test]
    fn scope_includes_requires_every_token() {
        let (_dir, cfg) = load_config(
            r#"
            [[hook]]
            on = "post-worker-complete"
            run = "x"
            match.scope_includes = ["auth", "jwt"]
        "#,
        );
        let mut orb = make_orb();
        orb.scope = vec!["auth".into()];
        assert!(cfg
            .matching(HookEvent::PostWorkerComplete, &MatcherCtx::for_orb(&orb))
            .is_empty());
        orb.scope.push("jwt".into());
        assert_eq!(
            cfg.matching(HookEvent::PostWorkerComplete, &MatcherCtx::for_orb(&orb))
                .len(),
            1
        );
    }

    #[test]
    fn multiple_constraints_are_anded() {
        let (_dir, cfg) = load_config(
            r#"
            [[hook]]
            on = "post-worker-complete"
            run = "x"
            match.orb_type = "task"
            match.labels = ["db"]
            match.priority_max = 2
        "#,
        );
        let mut orb = make_orb();
        orb.labels = vec!["db".into()];
        // type=Task, label=db, priority=2 ← matches all three.
        assert_eq!(
            cfg.matching(HookEvent::PostWorkerComplete, &MatcherCtx::for_orb(&orb))
                .len(),
            1
        );
        // Bumping priority above the max breaks the match.
        orb.priority = 4;
        assert!(cfg
            .matching(HookEvent::PostWorkerComplete, &MatcherCtx::for_orb(&orb))
            .is_empty());
    }

    #[test]
    fn matching_preserves_source_order() {
        let (_dir, cfg) = load_config(
            r#"
            [[hook]]
            name = "first"
            on = "post-worker-complete"
            run = "x"

            [[hook]]
            name = "second"
            on = "post-worker-complete"
            run = "y"

            [[hook]]
            name = "third"
            on = "post-worker-complete"
            run = "z"
        "#,
        );
        let orb = make_orb();
        let names: Vec<_> = cfg
            .matching(HookEvent::PostWorkerComplete, &MatcherCtx::for_orb(&orb))
            .iter()
            .map(|h| h.name.as_str())
            .collect();
        assert_eq!(names, vec!["first", "second", "third"]);
    }

    #[test]
    fn enabled_false_excluded_from_matching() {
        let (_dir, cfg) = load_config(
            r#"
            [[hook]]
            name = "off"
            on = "post-worker-complete"
            run = "x"
            enabled = false

            [[hook]]
            name = "on"
            on = "post-worker-complete"
            run = "y"
        "#,
        );
        let orb = make_orb();
        let names: Vec<_> = cfg
            .matching(HookEvent::PostWorkerComplete, &MatcherCtx::for_orb(&orb))
            .iter()
            .map(|h| h.name.as_str())
            .collect();
        assert_eq!(names, vec!["on"]);
    }

    #[test]
    fn pipeline_level_event_matches_without_orb_when_no_orb_constraints() {
        let (_dir, cfg) = load_config(
            r#"
            [[hook]]
            name = "tick-metrics"
            on = "on-queue-tick"
            run = "./metrics.sh"
        "#,
        );
        let ctx = MatcherCtx::default();
        let m = cfg.matching(HookEvent::OnQueueTick, &ctx);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].name, "tick-metrics");
    }

    #[test]
    fn pipeline_level_event_rejects_orb_constrained_hooks_without_orb() {
        let (_dir, cfg) = load_config(
            r#"
            [[hook]]
            name = "task-only-tick"
            on = "on-queue-tick"
            run = "x"
            match.orb_type = "task"
        "#,
        );
        let ctx = MatcherCtx::default();
        assert!(cfg.matching(HookEvent::OnQueueTick, &ctx).is_empty());
    }
}

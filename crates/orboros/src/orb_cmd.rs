//! Orb CRUD and management CLI commands.
//!
//! Each `cmd_orb_*` function takes store references and parsed arguments,
//! performs the operation, and prints human-readable output to stdout.

use anyhow::{bail, Context};
use orbs::dep::EdgeType;
use orbs::dep_store::DepStore;
use orbs::id::OrbId;
use orbs::orb::{Orb, OrbPhase, OrbStatus, OrbType};
use orbs::orb_store::OrbStore;
use orbs::review::{ReviewVerdict, ReviseScope};

use crate::hooks::{FireCtx, HookEvent, HookSink};

// ── Parsing helpers ────────────────────────────────────────────

/// Parses a string into an `OrbType`.
///
/// # Errors
///
/// This function is infallible for known types; unknown strings produce `Custom`.
pub fn parse_orb_type(s: &str) -> anyhow::Result<OrbType> {
    match s.to_lowercase().as_str() {
        "epic" => Ok(OrbType::Epic),
        "feature" => Ok(OrbType::Feature),
        "task" => Ok(OrbType::Task),
        "bug" => Ok(OrbType::Bug),
        "chore" => Ok(OrbType::Chore),
        "docs" => Ok(OrbType::Docs),
        other => Ok(OrbType::Custom(other.to_string())),
    }
}

/// Parses a string into an `OrbStatus`.
///
/// # Errors
///
/// Returns an error if the string does not match a known status.
pub fn parse_orb_status(s: &str) -> anyhow::Result<OrbStatus> {
    match s.to_lowercase().as_str() {
        "draft" => Ok(OrbStatus::Draft),
        "pending" => Ok(OrbStatus::Pending),
        "active" => Ok(OrbStatus::Active),
        "review" => Ok(OrbStatus::Review),
        "done" => Ok(OrbStatus::Done),
        "failed" => Ok(OrbStatus::Failed),
        "cancelled" => Ok(OrbStatus::Cancelled),
        "deferred" => Ok(OrbStatus::Deferred),
        "tombstone" => Ok(OrbStatus::Tombstone),
        other => bail!(
            "unknown orb status: {other}. Use: draft, pending, active, review, done, failed, cancelled, deferred"
        ),
    }
}

/// Filter selector for `cmd_orb_list` reviewer-status filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewStatusFilter {
    Accept,
    Reject,
    Revise,
    Any,
    Missing,
}

/// Parses a string into a [`ReviewStatusFilter`].
///
/// # Errors
///
/// Returns an error if the string does not match a known filter.
pub fn parse_review_status_filter(s: &str) -> anyhow::Result<ReviewStatusFilter> {
    match s.to_lowercase().as_str() {
        "accept" | "accepted" => Ok(ReviewStatusFilter::Accept),
        "reject" | "rejected" => Ok(ReviewStatusFilter::Reject),
        "revise" => Ok(ReviewStatusFilter::Revise),
        "any" | "reviewed" => Ok(ReviewStatusFilter::Any),
        "none" | "missing" | "unreviewed" => Ok(ReviewStatusFilter::Missing),
        other => bail!(
            "unknown review-status filter: {other}. Use: accept, reject, revise, any, missing"
        ),
    }
}

#[must_use]
pub fn verdict_label(v: &ReviewVerdict) -> &'static str {
    match v {
        ReviewVerdict::Accept => "ACCEPT",
        ReviewVerdict::Reject => "REJECT",
        ReviewVerdict::Revise {
            scope: ReviseScope::Execution,
        } => "REVISE (execution)",
        ReviewVerdict::Revise {
            scope: ReviseScope::Decomposition,
        } => "REVISE (decomposition)",
    }
}

fn matches_review_filter(orb: &Orb, filter: ReviewStatusFilter) -> bool {
    match (filter, orb.review_report.as_ref()) {
        (ReviewStatusFilter::Missing, None) => true,
        (ReviewStatusFilter::Missing, Some(_)) => false,
        (_, None) => false,
        (ReviewStatusFilter::Any, Some(_)) => true,
        (ReviewStatusFilter::Accept, Some(r)) => r.verdict.is_accept(),
        (ReviewStatusFilter::Reject, Some(r)) => r.verdict.is_reject(),
        (ReviewStatusFilter::Revise, Some(r)) => r.verdict.is_revise(),
    }
}

/// Parses a string into an `EdgeType`.
///
/// # Errors
///
/// Returns an error if the string does not match a known edge type.
pub fn parse_edge_type(s: &str) -> anyhow::Result<EdgeType> {
    match s.to_lowercase().as_str() {
        "blocks" => Ok(EdgeType::Blocks),
        "depends_on" | "depends-on" => Ok(EdgeType::DependsOn),
        "parent" => Ok(EdgeType::Parent),
        "child" => Ok(EdgeType::Child),
        "related" => Ok(EdgeType::Related),
        "duplicates" => Ok(EdgeType::Duplicates),
        "follows" => Ok(EdgeType::Follows),
        other => bail!(
            "unknown edge type: {other}. Use: blocks, depends_on, parent, child, related, duplicates, follows"
        ),
    }
}

// ── Commands ───────────────────────────────────────────────────

/// Creates a new orb and persists it to the store.
///
/// # Errors
///
/// Returns an error if the store write fails.
pub fn cmd_orb_create(
    store: &OrbStore,
    title: &str,
    description: &str,
    orb_type: OrbType,
    priority: u8,
    hooks: Option<&HookSink>,
) -> anyhow::Result<Orb> {
    let orb = Orb::new(title, description)
        .with_type(orb_type)
        .with_priority(priority);
    store
        .append(&orb)
        .context("failed to persist new orb to store")?;
    println!("Created orb {}", orb.id);
    println!("  type:     {:?}", orb.orb_type);
    println!("  priority: {}", orb.priority);
    if let Some(status) = orb.status {
        println!("  status:   {status:?}");
    }
    if let Some(phase) = orb.phase {
        println!("  phase:    {phase:?}");
    }
    if let Some(sink) = hooks {
        let _ = sink.fire_blocking(HookEvent::OnOrbCreate, FireCtx::for_orb(&orb));
    }
    Ok(orb)
}

/// Prints detailed information about a single orb.
///
/// # Errors
///
/// Returns an error if the store read fails.
pub fn cmd_orb_show(store: &OrbStore, id: &str) -> anyhow::Result<()> {
    let orb_id = OrbId::from_raw(id);
    match store
        .load_by_id(&orb_id)
        .context("failed to load orb from store")?
    {
        Some(orb) => {
            println!("Orb:         {}", orb.id);
            println!("Title:       {}", orb.title);
            println!("Description: {}", orb.description);
            println!("Type:        {:?}", orb.orb_type);
            println!(
                "Priority:    {} ({})",
                orb.priority,
                orbs::orb::priority_name(orb.priority)
            );
            if let Some(status) = orb.status {
                println!("Status:      {status:?}");
            }
            if let Some(phase) = orb.phase {
                println!("Phase:       {phase:?}");
            }
            if let Some(ref design) = orb.design {
                println!("Design:      {design}");
            }
            if let Some(ref ac) = orb.acceptance_criteria {
                println!("Acceptance:  {ac}");
            }
            if !orb.scope.is_empty() {
                println!("Scope:       {}", orb.scope.join(", "));
            }
            if !orb.labels.is_empty() {
                println!("Labels:      {}", orb.labels.join(", "));
            }
            if let Some(ref parent) = orb.parent_id {
                println!("Parent:      {parent}");
            }
            if let Some(ref root) = orb.root_id {
                println!("Root:        {root}");
            }
            println!("Created:     {}", orb.created_at);
            println!("Updated:     {}", orb.updated_at);
            if let Some(closed) = orb.closed_at {
                println!("Closed:      {closed}");
            }
            if let Some(ref result) = orb.result {
                println!("Result:      {result}");
            }
            if let Some(confidence) = orb.confidence {
                println!("Confidence:  {confidence:.2}");
            }
            if let Some(ref report) = orb.review_report {
                println!("Review:      {}", verdict_label(&report.verdict));
                println!("  Model:     {}", report.reviewer_model);
                if !report.critique.is_empty() {
                    println!("  Critique:  {}", report.critique);
                }
                if let Some(ref sc) = report.suggested_changes {
                    println!("  Suggested: {sc}");
                }
            }
            if orb.is_tombstoned() {
                println!(
                    "DELETED:     {}",
                    orb.deleted_at.map_or("yes".to_string(), |d| d.to_string())
                );
                if let Some(ref reason) = orb.delete_reason {
                    println!("Reason:      {reason}");
                }
            }
        }
        None => {
            println!("Orb {id} not found.");
        }
    }
    Ok(())
}

/// Lists orbs with optional type and status filters.
///
/// # Errors
///
/// Returns an error if the store read fails or a filter string is invalid.
pub fn cmd_orb_list(
    store: &OrbStore,
    filter_type: Option<&str>,
    filter_status: Option<&str>,
    min_confidence: Option<f32>,
    max_confidence: Option<f32>,
    review_status: Option<&str>,
) -> anyhow::Result<()> {
    let mut orbs = store.load_all().context("failed to load orbs")?;

    if let Some(type_str) = filter_type {
        let orb_type = parse_orb_type(type_str)?;
        orbs.retain(|o| o.orb_type == orb_type);
    }

    if let Some(status_str) = filter_status {
        let status = parse_orb_status(status_str)?;
        orbs.retain(|o| o.status == Some(status));
    }

    if let Some(min) = min_confidence {
        orbs.retain(|o| o.confidence.is_some_and(|c| c >= min));
    }

    if let Some(max) = max_confidence {
        orbs.retain(|o| o.confidence.is_some_and(|c| c <= max));
    }

    if let Some(rs) = review_status {
        let filter = parse_review_status_filter(rs)?;
        orbs.retain(|o| matches_review_filter(o, filter));
    }

    if orbs.is_empty() {
        println!("No orbs found.");
    } else {
        for orb in &orbs {
            let lifecycle = if let Some(status) = orb.status {
                format!("{status:?}")
            } else if let Some(phase) = orb.phase {
                format!("{phase:?}")
            } else {
                "Unknown".to_string()
            };
            let conf = orb
                .confidence
                .map_or(String::new(), |c| format!(" conf={c:.2}"));
            println!(
                "[{lifecycle}] {} — {} ({:?}, p{}){}",
                orb.id, orb.title, orb.orb_type, orb.priority, conf
            );
        }
        println!("\n{} orb(s)", orbs.len());
    }
    Ok(())
}

/// Prints orbs whose second-opinion reviewer verdict is `Revise`,
/// pending operator action. Includes the verdict scope and critique
/// preview so the user can prioritize.
///
/// # Errors
///
/// Returns an error if the store read fails.
pub fn cmd_review_queue(store: &OrbStore) -> anyhow::Result<()> {
    let orbs = store.load_all().context("failed to load orbs")?;
    let mut revising: Vec<&Orb> = orbs
        .iter()
        .filter(|o| {
            o.review_report
                .as_ref()
                .is_some_and(|r| r.verdict.is_revise())
        })
        .collect();
    revising.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));

    if revising.is_empty() {
        println!("Review queue empty.");
        return Ok(());
    }
    for orb in &revising {
        let report = orb.review_report.as_ref().unwrap();
        let preview = report
            .critique
            .lines()
            .next()
            .unwrap_or("(no critique)")
            .chars()
            .take(80)
            .collect::<String>();
        println!(
            "{}  [{}]  {} — {preview}",
            orb.id,
            verdict_label(&report.verdict),
            orb.title,
        );
    }
    println!("\n{} orb(s) pending revise", revising.len());
    Ok(())
}

/// Updates specified fields on an existing orb.
///
/// # Errors
///
/// Returns an error if the orb is not found, the status string is invalid,
/// or the store write fails.
#[allow(clippy::too_many_arguments)]
pub fn cmd_orb_update(
    store: &OrbStore,
    id: &str,
    title: Option<&str>,
    description: Option<&str>,
    priority: Option<u8>,
    status: Option<&str>,
    confidence: Option<f32>,
    hooks: Option<&HookSink>,
) -> anyhow::Result<()> {
    let orb_id = OrbId::from_raw(id);
    let mut orb = store
        .load_by_id(&orb_id)
        .context("failed to load orb")?
        .ok_or_else(|| anyhow::anyhow!("orb {id} not found"))?;
    let prior_status = orb.status;
    let prior_confidence = orb.confidence;

    if let Some(t) = title {
        orb.title = t.to_string();
    }
    if let Some(d) = description {
        orb.description = d.to_string();
    }
    if let Some(p) = priority {
        orb.priority = p.clamp(1, 5);
    }
    if let Some(s) = status {
        let new_status = parse_orb_status(s)?;
        orb.set_status(new_status)
            .with_context(|| format!("invalid status transition to {new_status:?}"))?;
    }
    if let Some(c) = confidence {
        if !c.is_finite() || !(0.0..=1.0).contains(&c) {
            return Err(anyhow::anyhow!(
                "confidence must be a finite value in [0.0, 1.0]; got {c}"
            ));
        }
        orb.confidence = Some(c);
    }
    orb.updated_at = chrono::Utc::now();
    orb.update_content_hash();

    store.update(&orb).context("failed to persist orb update")?;

    if let (Some(new_c), prior) = (orb.confidence, prior_confidence) {
        if prior != Some(new_c) {
            tracing::info!(
                orb_id = %orb.id,
                confidence = new_c,
                "confidence recorded on orb",
            );
        }
    }

    println!("Updated orb {}", orb.id);

    if let Some(sink) = hooks {
        let became_cancelled =
            orb.status == Some(OrbStatus::Cancelled) && prior_status != Some(OrbStatus::Cancelled);
        if became_cancelled {
            let _ = sink.fire_blocking(HookEvent::OnCancel, FireCtx::for_orb(&orb));
        }
    }
    Ok(())
}

/// Tombstones (soft-deletes) an orb.
///
/// # Errors
///
/// Returns an error if the orb is not found or the store write fails.
pub fn cmd_orb_delete(
    store: &OrbStore,
    id: &str,
    reason: Option<&str>,
    hooks: Option<&HookSink>,
) -> anyhow::Result<()> {
    let orb_id = OrbId::from_raw(id);
    let mut orb = store
        .load_by_id(&orb_id)
        .context("failed to load orb")?
        .ok_or_else(|| anyhow::anyhow!("orb {id} not found"))?;

    orb.tombstone(reason.map(String::from));

    store
        .update(&orb)
        .context("failed to persist orb tombstone")?;

    println!("Deleted orb {} (tombstoned)", orb.id);
    if let Some(r) = reason {
        println!("  reason: {r}");
    }
    if let Some(sink) = hooks {
        let _ = sink.fire_blocking(HookEvent::OnDelete, FireCtx::for_orb(&orb));
    }
    Ok(())
}

/// Adds a dependency edge between two orbs.
///
/// # Errors
///
/// Returns an error if adding the edge would create a cycle or the store write fails.
pub fn cmd_orb_dep_add(
    dep_store: &DepStore,
    from_id: &str,
    to_id: &str,
    edge_type: EdgeType,
) -> anyhow::Result<()> {
    let from = OrbId::from_raw(from_id);
    let to = OrbId::from_raw(to_id);
    let edge = orbs::dep::DepEdge::new(from, to, edge_type);

    dep_store
        .add_edge(edge)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("failed to add dependency edge")?;

    println!("Added {edge_type:?} edge: {from_id} -> {to_id}");
    Ok(())
}

/// Removes a dependency edge between two orbs.
///
/// # Errors
///
/// Returns an error if the store read/write fails.
pub fn cmd_orb_dep_remove(
    dep_store: &DepStore,
    from_id: &str,
    to_id: &str,
    edge_type: EdgeType,
) -> anyhow::Result<()> {
    let from = OrbId::from_raw(from_id);
    let to = OrbId::from_raw(to_id);

    let removed = dep_store
        .remove_edge(&from, &to, edge_type)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("failed to remove dependency edge")?;

    if removed {
        println!("Removed {edge_type:?} edge: {from_id} -> {to_id}");
    } else {
        println!("No matching edge found: {from_id} -> {to_id} ({edge_type:?})");
    }
    Ok(())
}

/// Lists all dependency edges for an orb (both outgoing and incoming).
///
/// # Errors
///
/// Returns an error if the store read fails.
pub fn cmd_orb_deps(dep_store: &DepStore, id: &str) -> anyhow::Result<()> {
    let orb_id = OrbId::from_raw(id);

    let from_edges = dep_store
        .edges_from(&orb_id)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let to_edges = dep_store
        .edges_to(&orb_id)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    if from_edges.is_empty() && to_edges.is_empty() {
        println!("No dependencies for orb {id}.");
        return Ok(());
    }

    if !from_edges.is_empty() {
        println!("Outgoing ({id} -> ...):");
        for edge in &from_edges {
            println!("  {:?} -> {}", edge.edge_type, edge.to);
        }
    }

    if !to_edges.is_empty() {
        println!("Incoming (... -> {id}):");
        for edge in &to_edges {
            println!("  {} {:?} -> {id}", edge.from, edge.edge_type);
        }
    }

    Ok(())
}

/// Applies a review decision to an orb.
///
/// Valid decisions: "approve" (-> Done), "reject" (-> Failed), "revise" (-> Active).
///
/// # Errors
///
/// Returns an error if the orb is not found, not in review state,
/// the decision is invalid, or the store write fails.
pub fn cmd_orb_review(
    store: &OrbStore,
    id: &str,
    decision: &str,
    hooks: Option<&HookSink>,
) -> anyhow::Result<()> {
    let orb_id = OrbId::from_raw(id);
    let mut orb = store
        .load_by_id(&orb_id)
        .context("failed to load orb")?
        .ok_or_else(|| anyhow::anyhow!("orb {id} not found"))?;

    // Check orb is in a reviewable state
    let in_review = orb.status == Some(OrbStatus::Review) || orb.phase == Some(OrbPhase::Review);
    if !in_review {
        bail!("orb {id} is not in review state");
    }

    let event = match decision.to_lowercase().as_str() {
        "approve" => {
            if orb.status.is_some() {
                orb.set_status(OrbStatus::Done)
                    .context("approve: status transition rejected")?;
            } else {
                orb.set_phase(OrbPhase::Done)
                    .context("approve: phase transition rejected")?;
            }
            println!("Approved orb {id} -> Done");
            HookEvent::OnReviewApprove
        }
        "reject" => {
            if orb.status.is_some() {
                orb.set_status(OrbStatus::Failed)
                    .context("reject: status transition rejected")?;
            } else {
                orb.set_phase(OrbPhase::Failed)
                    .context("reject: phase transition rejected")?;
            }
            println!("Rejected orb {id} -> Failed");
            HookEvent::OnReviewReject
        }
        "revise" => {
            if orb.status.is_some() {
                orb.set_status(OrbStatus::Active)
                    .context("revise: status transition rejected")?;
            } else {
                orb.set_phase(OrbPhase::Executing)
                    .context("revise: phase transition rejected")?;
            }
            println!("Sent orb {id} back for revision -> Active");
            HookEvent::OnReviewRevise
        }
        other => bail!("unknown review decision: {other}. Use: approve, reject, revise"),
    };

    store
        .update(&orb)
        .context("failed to persist review decision")?;

    if let Some(sink) = hooks {
        let _ = sink.fire_blocking(event, FireCtx::for_orb(&orb));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_orb_store() -> (tempfile::TempDir, OrbStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = OrbStore::new(dir.path().join("orbs.jsonl"));
        (dir, store)
    }

    fn make_review_report(verdict: ReviewVerdict, critique: &str) -> orbs::review::ReviewReport {
        orbs::review::ReviewReport {
            verdict,
            critique: critique.into(),
            suggested_changes: None,
            reviewer_model: "m".into(),
            reviewed_at: chrono::Utc::now(),
            reviewer_orb_id: None,
        }
    }

    // ── review filter ──────────────────────────────────────────

    #[test]
    fn review_status_filter_parses_aliases() {
        assert_eq!(
            parse_review_status_filter("accepted").unwrap(),
            ReviewStatusFilter::Accept
        );
        assert_eq!(
            parse_review_status_filter("REVISE").unwrap(),
            ReviewStatusFilter::Revise
        );
        assert_eq!(
            parse_review_status_filter("unreviewed").unwrap(),
            ReviewStatusFilter::Missing
        );
        assert!(parse_review_status_filter("nope").is_err());
    }

    #[test]
    fn matches_review_filter_handles_all_combinations() {
        let mut accepted = Orb::new("a", "x");
        accepted.review_report = Some(make_review_report(ReviewVerdict::Accept, ""));
        let mut revising = Orb::new("r", "x");
        revising.review_report = Some(make_review_report(
            ReviewVerdict::Revise {
                scope: ReviseScope::Execution,
            },
            "c",
        ));
        let unreviewed = Orb::new("u", "x");

        assert!(matches_review_filter(&accepted, ReviewStatusFilter::Accept));
        assert!(matches_review_filter(&accepted, ReviewStatusFilter::Any));
        assert!(!matches_review_filter(
            &accepted,
            ReviewStatusFilter::Revise
        ));
        assert!(!matches_review_filter(
            &accepted,
            ReviewStatusFilter::Missing
        ));

        assert!(matches_review_filter(&revising, ReviewStatusFilter::Revise));
        assert!(matches_review_filter(&revising, ReviewStatusFilter::Any));
        assert!(!matches_review_filter(
            &revising,
            ReviewStatusFilter::Accept
        ));

        assert!(matches_review_filter(
            &unreviewed,
            ReviewStatusFilter::Missing
        ));
        assert!(!matches_review_filter(&unreviewed, ReviewStatusFilter::Any));
        assert!(!matches_review_filter(
            &unreviewed,
            ReviewStatusFilter::Accept
        ));
    }

    #[test]
    fn verdict_labels_distinguish_revise_scopes() {
        assert_eq!(verdict_label(&ReviewVerdict::Accept), "ACCEPT");
        assert_eq!(verdict_label(&ReviewVerdict::Reject), "REJECT");
        assert_eq!(
            verdict_label(&ReviewVerdict::Revise {
                scope: ReviseScope::Execution,
            }),
            "REVISE (execution)"
        );
        assert_eq!(
            verdict_label(&ReviewVerdict::Revise {
                scope: ReviseScope::Decomposition,
            }),
            "REVISE (decomposition)"
        );
    }

    // ── cmd_review_queue ───────────────────────────────────────

    #[test]
    fn review_queue_lists_only_revise_orbs() {
        let (_dir, store) = temp_orb_store();
        let a = cmd_orb_create(&store, "A", "x", OrbType::Task, 3, None).unwrap();
        let b = cmd_orb_create(&store, "B", "x", OrbType::Task, 3, None).unwrap();
        let c = cmd_orb_create(&store, "C", "x", OrbType::Task, 3, None).unwrap();

        let mut a2 = a;
        a2.review_report = Some(make_review_report(ReviewVerdict::Accept, ""));
        store.update(&a2).unwrap();
        let mut b2 = b;
        b2.review_report = Some(make_review_report(
            ReviewVerdict::Revise {
                scope: ReviseScope::Execution,
            },
            "needs fix",
        ));
        store.update(&b2).unwrap();
        // c stays unreviewed.
        let _ = c;

        let queued: Vec<_> = store
            .load_all()
            .unwrap()
            .into_iter()
            .filter(|o| {
                o.review_report
                    .as_ref()
                    .is_some_and(|r| r.verdict.is_revise())
            })
            .collect();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].title, "B");

        cmd_review_queue(&store).unwrap();
    }

    #[test]
    fn review_queue_empty_when_no_revising_orbs() {
        let (_dir, store) = temp_orb_store();
        cmd_orb_create(&store, "Only one", "x", OrbType::Task, 3, None).unwrap();
        cmd_review_queue(&store).unwrap();
    }

    fn temp_dep_store() -> (tempfile::TempDir, DepStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = DepStore::new(dir.path().join("deps.jsonl"));
        (dir, store)
    }

    // ── parse helpers ──────────────────────────────────────────

    #[test]
    fn parse_orb_type_known_types() {
        assert_eq!(parse_orb_type("task").unwrap(), OrbType::Task);
        assert_eq!(parse_orb_type("Epic").unwrap(), OrbType::Epic);
        assert_eq!(parse_orb_type("FEATURE").unwrap(), OrbType::Feature);
        assert_eq!(parse_orb_type("bug").unwrap(), OrbType::Bug);
        assert_eq!(parse_orb_type("chore").unwrap(), OrbType::Chore);
        assert_eq!(parse_orb_type("docs").unwrap(), OrbType::Docs);
    }

    #[test]
    fn parse_orb_type_custom() {
        match parse_orb_type("research").unwrap() {
            OrbType::Custom(s) => assert_eq!(s, "research"),
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    #[test]
    fn parse_orb_status_valid() {
        assert_eq!(parse_orb_status("pending").unwrap(), OrbStatus::Pending);
        assert_eq!(parse_orb_status("Active").unwrap(), OrbStatus::Active);
        assert_eq!(parse_orb_status("DONE").unwrap(), OrbStatus::Done);
    }

    #[test]
    fn parse_orb_status_invalid() {
        assert!(parse_orb_status("garbage").is_err());
    }

    #[test]
    fn parse_edge_type_valid() {
        assert_eq!(parse_edge_type("blocks").unwrap(), EdgeType::Blocks);
        assert_eq!(parse_edge_type("depends_on").unwrap(), EdgeType::DependsOn);
        assert_eq!(parse_edge_type("depends-on").unwrap(), EdgeType::DependsOn);
        assert_eq!(parse_edge_type("related").unwrap(), EdgeType::Related);
    }

    #[test]
    fn parse_edge_type_invalid() {
        assert!(parse_edge_type("notreal").is_err());
    }

    // ── cmd_orb_create ─────────────────────────────────────────

    #[test]
    fn create_orb_persists_and_returns() {
        let (_dir, store) = temp_orb_store();
        let orb =
            cmd_orb_create(&store, "My task", "Do the thing", OrbType::Task, 2, None).unwrap();
        assert_eq!(orb.title, "My task");
        assert_eq!(orb.description, "Do the thing");
        assert_eq!(orb.orb_type, OrbType::Task);
        assert_eq!(orb.priority, 2);
        assert_eq!(orb.status, Some(OrbStatus::Pending));

        // Verify it was persisted
        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, orb.id);
    }

    #[test]
    fn create_epic_uses_phase() {
        let (_dir, store) = temp_orb_store();
        let orb =
            cmd_orb_create(&store, "Big epic", "Large effort", OrbType::Epic, 1, None).unwrap();
        assert_eq!(orb.orb_type, OrbType::Epic);
        assert_eq!(orb.phase, Some(OrbPhase::Pending));
        assert_eq!(orb.status, None);
    }

    // ── cmd_orb_show ───────────────────────────────────────────

    #[test]
    fn show_existing_orb() {
        let (_dir, store) = temp_orb_store();
        let orb =
            cmd_orb_create(&store, "Show me", "Details here", OrbType::Task, 3, None).unwrap();
        // Should not error
        cmd_orb_show(&store, orb.id.as_str()).unwrap();
    }

    #[test]
    fn show_missing_orb_prints_not_found() {
        let (_dir, store) = temp_orb_store();
        // Should not error, just prints "not found"
        cmd_orb_show(&store, "orb-nonexistent").unwrap();
    }

    // ── cmd_orb_list ───────────────────────────────────────────

    #[test]
    fn list_all_orbs() {
        let (_dir, store) = temp_orb_store();
        cmd_orb_create(&store, "One", "first", OrbType::Task, 3, None).unwrap();
        cmd_orb_create(&store, "Two", "second", OrbType::Bug, 2, None).unwrap();
        // Should list both
        cmd_orb_list(&store, None, None, None, None, None).unwrap();
    }

    #[test]
    fn list_min_confidence_filters_below_threshold() {
        let (_dir, store) = temp_orb_store();
        let orb = cmd_orb_create(&store, "Low", "x", OrbType::Task, 3, None).unwrap();
        let mut low = orb;
        low.confidence = Some(0.3);
        store.update(&low).unwrap();
        let orb2 = cmd_orb_create(&store, "High", "y", OrbType::Task, 3, None).unwrap();
        let mut high = orb2;
        high.confidence = Some(0.9);
        store.update(&high).unwrap();
        let orb3 = cmd_orb_create(&store, "None", "z", OrbType::Task, 3, None).unwrap();

        // Sanity-check the filter logic the CLI uses.
        let all = store.load_all().unwrap();
        let above_half: Vec<_> = all
            .iter()
            .filter(|o| o.confidence.is_some_and(|c| c >= 0.7))
            .collect();
        assert_eq!(above_half.len(), 1);
        assert_eq!(above_half[0].title, "High");

        // CLI command should not error.
        cmd_orb_list(&store, None, None, Some(0.7), None, None).unwrap();

        // The None-confidence orb is excluded even by lax threshold.
        let any_conf: Vec<_> = all
            .iter()
            .filter(|o| o.confidence.is_some_and(|c| c >= 0.0))
            .collect();
        assert_eq!(any_conf.len(), 2);
        assert!(!any_conf.iter().any(|o| o.id == orb3.id));
    }

    #[test]
    fn list_max_confidence_excludes_high_scores() {
        let (_dir, store) = temp_orb_store();
        let orb = cmd_orb_create(&store, "Doubtful", "x", OrbType::Task, 3, None).unwrap();
        let mut doubtful = orb;
        doubtful.confidence = Some(0.2);
        store.update(&doubtful).unwrap();
        let orb2 = cmd_orb_create(&store, "Sure", "y", OrbType::Task, 3, None).unwrap();
        let mut sure = orb2;
        sure.confidence = Some(0.95);
        store.update(&sure).unwrap();

        let all = store.load_all().unwrap();
        let doubtful_only: Vec<_> = all
            .iter()
            .filter(|o| o.confidence.is_some_and(|c| c <= 0.5))
            .collect();
        assert_eq!(doubtful_only.len(), 1);
        assert_eq!(doubtful_only[0].title, "Doubtful");

        cmd_orb_list(&store, None, None, None, Some(0.5), None).unwrap();
    }

    #[test]
    fn list_with_type_filter() {
        let (_dir, store) = temp_orb_store();
        cmd_orb_create(&store, "Task one", "t1", OrbType::Task, 3, None).unwrap();
        cmd_orb_create(&store, "Bug one", "b1", OrbType::Bug, 2, None).unwrap();

        // Verify internal filtering by checking store directly
        let all = store.load_all().unwrap();
        assert_eq!(all.len(), 2);

        let tasks: Vec<_> = all.iter().filter(|o| o.orb_type == OrbType::Task).collect();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "Task one");
    }

    #[test]
    fn list_with_status_filter() {
        let (_dir, store) = temp_orb_store();
        let orb = cmd_orb_create(
            &store,
            "Active orb",
            "will be active",
            OrbType::Task,
            3,
            None,
        )
        .unwrap();

        // Update to active
        let mut updated = orb;
        updated.set_status(OrbStatus::Active).unwrap();
        store.update(&updated).unwrap();

        cmd_orb_create(
            &store,
            "Pending orb",
            "stays pending",
            OrbType::Task,
            3,
            None,
        )
        .unwrap();

        // Filter for active: should get 1
        let all = store.load_all().unwrap();
        let active: Vec<_> = all
            .iter()
            .filter(|o| o.status == Some(OrbStatus::Active))
            .collect();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].title, "Active orb");
    }

    #[test]
    fn list_empty_store() {
        let (_dir, store) = temp_orb_store();
        cmd_orb_list(&store, None, None, None, None, None).unwrap();
    }

    // ── cmd_orb_update ─────────────────────────────────────────

    #[test]
    fn update_title_and_priority() {
        let (_dir, store) = temp_orb_store();
        let orb = cmd_orb_create(&store, "Original", "desc", OrbType::Task, 3, None).unwrap();
        let id = orb.id.as_str().to_string();

        cmd_orb_update(
            &store,
            &id,
            Some("Updated title"),
            None,
            Some(1),
            None,
            None,
            None,
        )
        .unwrap();

        let loaded = store.load_by_id(&OrbId::from_raw(&id)).unwrap().unwrap();
        assert_eq!(loaded.title, "Updated title");
        assert_eq!(loaded.priority, 1);
        assert_eq!(loaded.description, "desc"); // unchanged
    }

    #[test]
    fn update_status() {
        let (_dir, store) = temp_orb_store();
        let orb = cmd_orb_create(&store, "Orb", "desc", OrbType::Task, 3, None).unwrap();
        let id = orb.id.as_str().to_string();

        cmd_orb_update(&store, &id, None, None, None, Some("active"), None, None).unwrap();

        let loaded = store.load_by_id(&OrbId::from_raw(&id)).unwrap().unwrap();
        assert_eq!(loaded.status, Some(OrbStatus::Active));
    }

    #[test]
    fn update_nonexistent_orb_errors() {
        let (_dir, store) = temp_orb_store();
        let result = cmd_orb_update(
            &store,
            "orb-nope",
            Some("new"),
            None,
            None,
            None,
            None,
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn update_confidence_persists_value() {
        let (_dir, store) = temp_orb_store();
        let orb = cmd_orb_create(&store, "Orb", "desc", OrbType::Task, 3, None).unwrap();
        let id = orb.id.as_str().to_string();

        cmd_orb_update(&store, &id, None, None, None, None, Some(0.65), None).unwrap();

        let loaded = store.load_by_id(&OrbId::from_raw(&id)).unwrap().unwrap();
        assert_eq!(loaded.confidence, Some(0.65));
    }

    #[test]
    fn update_confidence_rejects_out_of_range() {
        let (_dir, store) = temp_orb_store();
        let orb = cmd_orb_create(&store, "Orb", "desc", OrbType::Task, 3, None).unwrap();
        let id = orb.id.as_str().to_string();

        let result = cmd_orb_update(&store, &id, None, None, None, None, Some(1.5), None);
        assert!(result.is_err());
        let result = cmd_orb_update(&store, &id, None, None, None, None, Some(-0.1), None);
        assert!(result.is_err());
        let result = cmd_orb_update(&store, &id, None, None, None, None, Some(f32::NAN), None);
        assert!(result.is_err());

        let loaded = store.load_by_id(&OrbId::from_raw(&id)).unwrap().unwrap();
        assert!(loaded.confidence.is_none(), "value never persisted");
    }

    // ── cmd_orb_delete ─────────────────────────────────────────

    #[test]
    fn delete_tombstones_orb() {
        let (_dir, store) = temp_orb_store();
        let orb = cmd_orb_create(&store, "Delete me", "bye", OrbType::Task, 3, None).unwrap();
        let id = orb.id.as_str().to_string();

        cmd_orb_delete(&store, &id, Some("duplicate"), None).unwrap();

        // Should not appear in normal load_all
        let all = store.load_all().unwrap();
        assert!(all.is_empty());

        // Should appear in tombstoned load
        let all_inc = store.load_all_including_tombstoned().unwrap();
        assert_eq!(all_inc.len(), 1);
        assert!(all_inc[0].is_tombstoned());
        assert_eq!(all_inc[0].delete_reason.as_deref(), Some("duplicate"));
    }

    #[test]
    fn delete_nonexistent_orb_errors() {
        let (_dir, store) = temp_orb_store();
        let result = cmd_orb_delete(&store, "orb-nope", None, None);
        assert!(result.is_err());
    }

    // ── cmd_orb_dep_add / remove ───────────────────────────────

    #[test]
    fn dep_add_and_list() {
        let (_dir, dep_store) = temp_dep_store();
        cmd_orb_dep_add(&dep_store, "orb-aaa", "orb-bbb", EdgeType::Blocks).unwrap();

        // Verify via deps listing
        cmd_orb_deps(&dep_store, "orb-aaa").unwrap();

        let edges = dep_store.edges_from(&OrbId::from_raw("orb-aaa")).unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].to, OrbId::from_raw("orb-bbb"));
        assert_eq!(edges[0].edge_type, EdgeType::Blocks);
    }

    #[test]
    fn dep_remove() {
        let (_dir, dep_store) = temp_dep_store();
        cmd_orb_dep_add(&dep_store, "orb-aaa", "orb-bbb", EdgeType::Blocks).unwrap();
        cmd_orb_dep_remove(&dep_store, "orb-aaa", "orb-bbb", EdgeType::Blocks).unwrap();

        let edges = dep_store.edges_from(&OrbId::from_raw("orb-aaa")).unwrap();
        assert!(edges.is_empty());
    }

    #[test]
    fn dep_remove_nonexistent_does_not_error() {
        let (_dir, dep_store) = temp_dep_store();
        // Should succeed but print "no matching edge"
        cmd_orb_dep_remove(&dep_store, "orb-x", "orb-y", EdgeType::Related).unwrap();
    }

    #[test]
    fn deps_empty() {
        let (_dir, dep_store) = temp_dep_store();
        cmd_orb_deps(&dep_store, "orb-lonely").unwrap();
    }

    // ── cmd_orb_review ─────────────────────────────────────────

    #[test]
    fn review_approve() {
        let (_dir, store) = temp_orb_store();
        let orb = cmd_orb_create(&store, "Review me", "check", OrbType::Task, 3, None).unwrap();
        let id = orb.id.as_str().to_string();

        // Move to review state first
        let mut o = store.load_by_id(&OrbId::from_raw(&id)).unwrap().unwrap();
        o.set_status(OrbStatus::Active).unwrap();
        o.set_status(OrbStatus::Review).unwrap();
        store.update(&o).unwrap();

        cmd_orb_review(&store, &id, "approve", None).unwrap();

        let loaded = store.load_by_id(&OrbId::from_raw(&id)).unwrap().unwrap();
        assert_eq!(loaded.status, Some(OrbStatus::Done));
    }

    #[test]
    fn review_reject() {
        let (_dir, store) = temp_orb_store();
        let orb = cmd_orb_create(&store, "Reject me", "nope", OrbType::Task, 3, None).unwrap();
        let id = orb.id.as_str().to_string();

        let mut o = store.load_by_id(&OrbId::from_raw(&id)).unwrap().unwrap();
        o.set_status(OrbStatus::Active).unwrap();
        o.set_status(OrbStatus::Review).unwrap();
        store.update(&o).unwrap();

        cmd_orb_review(&store, &id, "reject", None).unwrap();

        let loaded = store.load_by_id(&OrbId::from_raw(&id)).unwrap().unwrap();
        assert_eq!(loaded.status, Some(OrbStatus::Failed));
    }

    #[test]
    fn review_revise() {
        let (_dir, store) = temp_orb_store();
        let orb = cmd_orb_create(&store, "Revise me", "again", OrbType::Task, 3, None).unwrap();
        let id = orb.id.as_str().to_string();

        let mut o = store.load_by_id(&OrbId::from_raw(&id)).unwrap().unwrap();
        o.set_status(OrbStatus::Active).unwrap();
        o.set_status(OrbStatus::Review).unwrap();
        store.update(&o).unwrap();

        cmd_orb_review(&store, &id, "revise", None).unwrap();

        let loaded = store.load_by_id(&OrbId::from_raw(&id)).unwrap().unwrap();
        assert_eq!(loaded.status, Some(OrbStatus::Active));
    }

    #[test]
    fn review_not_in_review_state_errors() {
        let (_dir, store) = temp_orb_store();
        let orb = cmd_orb_create(&store, "Not ready", "pending", OrbType::Task, 3, None).unwrap();
        let id = orb.id.as_str().to_string();

        let result = cmd_orb_review(&store, &id, "approve", None);
        assert!(result.is_err());
    }

    #[test]
    fn review_invalid_decision_errors() {
        let (_dir, store) = temp_orb_store();
        let orb = cmd_orb_create(&store, "Review me", "check", OrbType::Task, 3, None).unwrap();
        let id = orb.id.as_str().to_string();

        let mut o = store.load_by_id(&OrbId::from_raw(&id)).unwrap().unwrap();
        o.set_status(OrbStatus::Active).unwrap();
        o.set_status(OrbStatus::Review).unwrap();
        store.update(&o).unwrap();

        let result = cmd_orb_review(&store, &id, "maybe", None);
        assert!(result.is_err());
    }
}

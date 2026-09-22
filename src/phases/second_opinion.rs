//! Second-opinion reviewer worker (task 58).
//!
//! Runs after an orb hits `Done`. Spawns a reviewer worker with the
//! orb description + final result (but NOT the executing worker's
//! reasoning trace, so it brings fresh perspective) and parses a
//! structured verdict out of the response.
//!
//! Distinct from `phases::review`, which is the human-in-the-loop
//! review checkpoint — that's a status the orb sits in waiting for a
//! human decision. This module's reviewer is an automated agent.

use std::fmt::Write as _;
use std::time::Instant;

use chrono::Utc;
use orbs::orb::Orb;
use orbs::review::{ReviewReport, ReviewVerdict, ReviseScope};
use tracing::{info, instrument, warn};

use crate::config::SecondOpinionConfig;
use crate::phases::prompt_util::extract_fenced_json;
use crate::worker::process::{Worker, WorkerConfig};

pub const REFINEMENT_REVIEWER_SYSTEM_PROMPT: &str = "You are an independent specification-quality reviewer. Evaluate the current task specification for completeness, internal consistency, concrete deliverables, and verifiable acceptance criteria. Do not approve a merely parseable or empty refinement. An accept must omit critique and suggested_changes entirely. A revise must include nonempty actionable critique; observations alone are not a revision request. Respond with exactly one JSON object, no prose: {\"verdict\": \"accept\"} or {\"verdict\": {\"revise\": {\"scope\": \"decomposition\"}}, \"critique\": \"...\"} or {\"verdict\": \"reject\", \"critique\": \"...\"}.";

/// Errors from the reviewer worker.
#[derive(Debug, thiserror::Error)]
pub enum ReviewerError {
    #[error("could not resolve reviewer model: {0}")]
    ModelResolution(String),
    #[error("orb has no result to review (orb={orb_id})")]
    NoResult { orb_id: String },
    #[error("worker spawn failed: {0}")]
    WorkerSpawn(String),
    #[error("worker send failed: {0}")]
    WorkerSend(String),
    #[error("worker returned empty response")]
    EmptyResponse,
    #[error("could not parse verdict JSON from response: {0}")]
    ParseFailed(String),
}

/// Builds the system + user prompts the reviewer worker sees.
///
/// The system prompt locks the output format to a single JSON object;
/// the user prompt carries the orb's task and the candidate result.
/// Intentionally avoids the worker's reasoning trace — the reviewer
/// should be evaluating the artifact, not approving the path.
#[must_use]
pub fn build_reviewer_prompts(orb: &Orb) -> (String, String) {
    let system = format!(
        "You are an independent reviewer. You will be given a task description \
and a candidate result. Judge ONLY the candidate result against the task. \
Do not consider the reasoning that produced it — you do not have access to \
that. Respond with exactly one JSON object, no surrounding prose, no code \
fences, in this shape:\n\
  {{\"verdict\": \"accept\"}}                                  // result meets the bar\n\
  {{\"verdict\": \"reject\", \"critique\": \"...\"}}            // unrecoverable\n\
  {{\"verdict\": {{\"revise\": {{\"scope\": \"execution\"}}}}, \"critique\": \"...\"}}      // re-execute same plan\n\
  {{\"verdict\": {{\"revise\": {{\"scope\": \"decomposition\"}}}}, \"critique\": \"...\"}}  // re-plan\n\
Use \"execution\" when the plan was sound but the run went wrong (sloppy \
output, missed an edge case). Use \"decomposition\" when the plan itself \
was wrong (missing step, wrong approach). Always include \"critique\" \
unless verdict is \"accept\". {addendum}",
        addendum = crate::worker::process::CONFIDENCE_PROMPT_ADDENDUM,
    );

    let mut user = format!("Task description:\n{}\n\n", orb.description);
    if let Some(ref ac) = orb.acceptance_criteria {
        let _ = write!(user, "Acceptance criteria:\n{ac}\n\n");
    }
    if let Some(ref result) = orb.result {
        let _ = writeln!(user, "Candidate result:\n{result}");
    }
    (system, user)
}

/// Parses a `ReviewVerdict` out of a worker response. Tries strict
/// JSON parse first, then falls back to scanning for a fenced
/// ```json``` block, then any `{...}` block that contains the
/// `verdict` key.
///
/// # Errors
///
/// Returns a `ReviewerError::ParseFailed` if no parseable shape is
/// found in `text`.
pub fn parse_verdict(text: &str) -> Result<ReviewVerdict, ReviewerError> {
    #[derive(serde::Deserialize)]
    struct Wrap {
        verdict: ReviewVerdict,
    }

    if let Ok(parsed) = serde_json::from_str::<Wrap>(text.trim()) {
        return Ok(parsed.verdict);
    }
    if let Some(inner) = extract_fenced_json(text) {
        if let Ok(parsed) = serde_json::from_str::<Wrap>(inner.trim()) {
            return Ok(parsed.verdict);
        }
    }
    if let Some(inner) = extract_first_object_with_verdict(text) {
        if let Ok(parsed) = serde_json::from_str::<Wrap>(&inner) {
            return Ok(parsed.verdict);
        }
    }
    Err(ReviewerError::ParseFailed(text.chars().take(200).collect()))
}

/// Validate newly generated refinement decisions without changing historical reports.
fn parse_refinement_verdict(text: &str) -> Result<ReviewVerdict, ReviewerError> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|_| {
        ReviewerError::ParseFailed("refinement review must be one JSON object".into())
    })?;
    let verdict: ReviewVerdict = serde_json::from_value(
        value.get("verdict").cloned().unwrap_or(serde_json::Value::Null),
    ).map_err(|_| ReviewerError::ParseFailed("unsupported refinement verdict".into()))?;
    if verdict.is_accept() {
        if value.get("critique").is_some() || value.get("suggested_changes").is_some()
            || value.get("notes").is_some()
        {
            return Err(ReviewerError::ParseFailed(
                "accept must omit critique, suggested_changes and notes".into(),
            ));
        }
    } else if value.get("critique").and_then(serde_json::Value::as_str)
        .is_none_or(|critique| critique.trim().is_empty())
    {
        return Err(ReviewerError::ParseFailed(
            "revision or rejection requires actionable critique".into(),
        ));
    }
    Ok(verdict)
}

/// Walks the text for the first `{` whose enclosing object contains
/// `"verdict"` as a key. Returns the matched substring (including the
/// braces). Naive bracket-matching — fine for reviewer output, which
/// is short, since the reviewer is instructed to produce a single
/// object.
fn extract_first_object_with_verdict(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            let mut depth: i32 = 0;
            let mut j = i;
            let mut in_string = false;
            let mut escape = false;
            while j < bytes.len() {
                let c = bytes[j];
                if escape {
                    escape = false;
                } else if in_string {
                    if c == b'\\' {
                        escape = true;
                    } else if c == b'"' {
                        in_string = false;
                    }
                } else {
                    match c {
                        b'"' => in_string = true,
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                let candidate = &text[i..=j];
                                if candidate.contains("\"verdict\"") {
                                    return Some(candidate.to_string());
                                }
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                j += 1;
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    None
}

/// Runs the reviewer worker against a completed orb and produces a
/// `ReviewReport`. The reviewer's response is not stored anywhere
/// outside the report — callers persist the report on the orb.
///
/// # Errors
///
/// Returns a `ReviewerError` if the worker fails, the response is
/// empty, or the verdict can't be parsed out of it.
#[instrument(name = "second_opinion.run", level = "debug", skip_all, fields(orb = %orb.id))]
pub async fn run_reviewer(
    orb: &Orb,
    cfg: &SecondOpinionConfig,
    base_worker_config: &WorkerConfig,
) -> Result<ReviewReport, ReviewerError> {
    if orb.result.is_none() {
        return Err(ReviewerError::NoResult {
            orb_id: orb.id.to_string(),
        });
    }
    let (system, user) = build_reviewer_prompts(orb);
    let model = cfg
        .reviewer_model
        .clone()
        .unwrap_or_else(|| base_worker_config.model.clone());

    let mut wc = base_worker_config.clone();
    wc.model = model.clone();
    wc.system_prompt = system;
    wc.max_iterations = Some(1);

    run_reviewer_with_config(orb, model, wc, user, false).await
}

/// Runs the mandatory post-refinement quality reviewer. Unlike the completion
/// reviewer, it evaluates the assembled task specification itself before any
/// child work can be released.
pub async fn run_refinement_reviewer(
    orb: &Orb,
    model_config: &crate::config::OrbConfig,
    base_worker_config: &WorkerConfig,
    system_prompt: &str,
) -> Result<ReviewReport, ReviewerError> {
    let system = crate::prompt::with_confidence_addendum(system_prompt);
    let mut user = format!(
        "Title:\n{}\n\nDescription:\n{}\n",
        orb.title, orb.description
    );
    if let Some(design) = &orb.design {
        let _ = write!(user, "\nDesign:\n{design}\n");
    }
    if let Some(criteria) = &orb.acceptance_criteria {
        let _ = write!(user, "\nAcceptance criteria:\n{criteria}\n");
    }
    let resolved = model_config
        .model_resolver()
        .resolve(crate::config::ModelRole::Reviewer)
        .map_err(|error| ReviewerError::ModelResolution(error.to_string()))?;
    let mut worker_config = base_worker_config.clone();
    crate::worker::dispatcher::apply_resolved_model(&mut worker_config, model_config, &resolved);
    worker_config.system_prompt = system;
    worker_config.max_iterations = Some(1);
    tracing::info!(
        orb = %orb.id,
        model = %resolved.model,
        router = resolved.router.as_deref().unwrap_or("default"),
        "starting refinement quality review"
    );
    run_reviewer_with_config(orb, resolved.model, worker_config, user, true).await
}

async fn run_reviewer_with_config(
    orb: &Orb,
    model: String,
    worker_config: WorkerConfig,
    user: String,
    refinement: bool,
) -> Result<ReviewReport, ReviewerError> {
    let started = Instant::now();
    let mut worker = Worker::spawn(&worker_config)
        .await
        .map_err(|e| ReviewerError::WorkerSpawn(e.to_string()))?;

    let outcome = worker
        .send(&format!("review-{}", orb.id), &user)
        .await
        .map_err(|e| ReviewerError::WorkerSend(e.to_string()))?;
    let _ = worker.shutdown().await;

    let body = outcome.response.as_deref().unwrap_or("").trim();
    if body.is_empty() {
        return Err(ReviewerError::EmptyResponse);
    }
    let verdict = if refinement {
        parse_refinement_verdict(body)?
    } else {
        parse_verdict(body)?
    };
    let critique = extract_critique(body).unwrap_or_default();
    let suggested_changes = extract_suggested_changes(body);
    let usage = outcome.effective_usage();

    info!(
        orb = %orb.id,
        model = %model,
        verdict = ?short_verdict(&verdict),
        elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        tokens_in = %usage.as_ref().map_or_else(|| "none".to_owned(), |usage| usage.prompt_tokens.to_string()),
        tokens_out = %usage.as_ref().map_or_else(|| "none".to_owned(), |usage| usage.completion_tokens.to_string()),
        total_tokens = %usage.as_ref().map_or_else(|| "none".to_owned(), |usage| usage.total_tokens.to_string()),
        cache_read_tokens = usage.as_ref().and_then(|usage| usage.cached_tokens).unwrap_or(0),
        cache_write_tokens = usage.as_ref().and_then(|usage| usage.cache_write_tokens).unwrap_or(0),
        cost_usd = %crate::execution::format_cost_usd(usage.as_ref().and_then(|usage| usage.cost_micros)),
        critique_len = critique.len(),
        "refinement quality review completed"
    );
    if verdict.is_accept() && !critique.is_empty() {
        warn!("reviewer accepted but also provided a critique; using verdict");
    }

    Ok(ReviewReport {
        verdict,
        critique,
        suggested_changes,
        reviewer_model: model,
        reviewed_at: Utc::now(),
        reviewer_orb_id: None,
    })
}

fn short_verdict(v: &ReviewVerdict) -> &'static str {
    match v {
        ReviewVerdict::Accept => "accept",
        ReviewVerdict::Reject => "reject",
        ReviewVerdict::Revise {
            scope: ReviseScope::Execution,
        } => "revise/execution",
        ReviewVerdict::Revise {
            scope: ReviseScope::Decomposition,
        } => "revise/decomposition",
    }
}

fn extract_critique(text: &str) -> Option<String> {
    extract_string_field(text, "critique")
}

fn extract_suggested_changes(text: &str) -> Option<String> {
    extract_string_field(text, "suggested_changes")
}

/// Pulls a string-valued field out of any JSON object found in `text`.
/// Cheap and forgiving — accepts the reviewer including extra noise
/// around the JSON.
fn extract_string_field(text: &str, field: &str) -> Option<String> {
    let raw = extract_first_object_with_verdict(text)
        .or_else(|| extract_fenced_json(text))
        .unwrap_or_else(|| text.to_string());
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orbs::orb::OrbType;

    fn make_done_orb(result: &str) -> Orb {
        let mut orb = Orb::new("Test task", "Implement feature X").with_type(OrbType::Task);
        orb.result = Some(result.into());
        orb.acceptance_criteria = Some("Should do X correctly".into());
        orb
    }

    // ── prompt builder ────────────────────────────────────────

    #[test]
    fn prompts_include_description_and_result() {
        let orb = make_done_orb("did the thing");
        let (system, user) = build_reviewer_prompts(&orb);
        assert!(system.contains("independent reviewer"));
        assert!(system.contains("JSON"));
        assert!(user.contains("Implement feature X"));
        assert!(user.contains("Should do X correctly"));
        assert!(user.contains("did the thing"));
    }

    #[test]
    fn prompts_skip_acceptance_when_absent() {
        let mut orb = make_done_orb("result");
        orb.acceptance_criteria = None;
        let (_system, user) = build_reviewer_prompts(&orb);
        assert!(!user.contains("Acceptance"));
    }

    // ── verdict parser — strict JSON ──────────────────────────

    #[test]
    fn parse_accept_from_strict_json() {
        let v = parse_verdict(r#"{"verdict": "accept"}"#).unwrap();
        assert!(v.is_accept());
    }

    #[test]
    fn parse_reject_from_strict_json() {
        let v = parse_verdict(r#"{"verdict": "reject", "critique": "bad"}"#).unwrap();
        assert!(v.is_reject());
    }

    #[test]
    fn parse_revise_with_explicit_execution_scope() {
        let v = parse_verdict(
            r#"{"verdict": {"revise": {"scope": "execution"}}, "critique": "noise"}"#,
        )
        .unwrap();
        assert_eq!(
            v,
            ReviewVerdict::Revise {
                scope: ReviseScope::Execution
            }
        );
    }

    #[test]
    fn parse_revise_with_decomposition_scope() {
        let v = parse_verdict(
            r#"{"verdict": {"revise": {"scope": "decomposition"}}, "critique": "missed step"}"#,
        )
        .unwrap();
        assert_eq!(
            v,
            ReviewVerdict::Revise {
                scope: ReviseScope::Decomposition
            }
        );
    }

    #[test]
    fn parse_revise_defaults_scope_when_empty_object() {
        let v = parse_verdict(r#"{"verdict": {"revise": {}}, "critique": "x"}"#).unwrap();
        assert_eq!(
            v,
            ReviewVerdict::Revise {
                scope: ReviseScope::Execution
            }
        );
    }

    // ── verdict parser — fenced block fallback ────────────────

    #[test]
    fn parse_from_fenced_json_block() {
        let text = "Here is my verdict:\n```json\n{\"verdict\": \"accept\"}\n```\nThanks.";
        let v = parse_verdict(text).unwrap();
        assert!(v.is_accept());
    }

    #[test]
    fn parse_from_fenced_block_without_lang_tag() {
        let text = "```\n{\"verdict\": \"reject\", \"critique\": \"nope\"}\n```";
        let v = parse_verdict(text).unwrap();
        assert!(v.is_reject());
    }

    // ── verdict parser — surrounding-text fallback ────────────

    #[test]
    fn parse_finds_object_amid_surrounding_text() {
        let text = "After reviewing, I concluded: {\"verdict\": \"accept\"} — that's my answer.";
        let v = parse_verdict(text).unwrap();
        assert!(v.is_accept());
    }

    #[test]
    fn parse_picks_object_with_verdict_when_multiple_objects() {
        let text = r#"Stats: {"latency": 100} Verdict: {"verdict": "reject", "critique": "no"}"#;
        let v = parse_verdict(text).unwrap();
        assert!(v.is_reject());
    }

    #[test]
    fn parse_fails_when_no_verdict_anywhere() {
        let err = parse_verdict("No JSON here, just words.").unwrap_err();
        assert!(matches!(err, ReviewerError::ParseFailed(_)));
    }

    #[test]
    fn parse_handles_quoted_braces_inside_strings() {
        // The critique contains a `{` — bracket matcher must not be
        // fooled by characters inside strings.
        let text = r#"{"verdict": "reject", "critique": "missing { in output"}"#;
        let v = parse_verdict(text).unwrap();
        assert!(v.is_reject());
    }

    // ── critique extraction ───────────────────────────────────

    #[test]
    fn extract_critique_from_object() {
        let text = r#"{"verdict": "reject", "critique": "off topic"}"#;
        assert_eq!(extract_critique(text).as_deref(), Some("off topic"));
    }

    #[test]
    fn extract_critique_returns_none_when_field_absent() {
        let text = r#"{"verdict": "accept"}"#;
        assert!(extract_critique(text).is_none());
    }

    #[test]
    fn extract_suggested_changes_when_present() {
        let text = r#"{"verdict": "reject", "critique": "x", "suggested_changes": "do Y"}"#;
        assert_eq!(extract_suggested_changes(text).as_deref(), Some("do Y"));
    }
}

#[cfg(test)]
mod refinement_verdict_tests {
    use super::*;

    #[test]
    fn acceptance_cannot_carry_revision_feedback() {
        assert!(parse_refinement_verdict(r#"{"verdict":"accept"}"#).is_ok());
        assert!(parse_refinement_verdict(r#"{"verdict":"accept","critique":"Fix API"}"#).is_err());
        assert!(parse_refinement_verdict(r#"{"verdict":"accept","suggested_changes":"Fix API"}"#).is_err());
        // The permissive historical/general reader remains compatible.
        assert!(parse_verdict(r#"{"verdict":"accept","critique":"Looks good"}"#).is_ok());
    }

    #[test]
    fn revision_requires_actionable_feedback() {
        assert!(parse_refinement_verdict(r#"{"verdict":{"revise":{"scope":"decomposition"}},"critique":"Add API acceptance criteria"}"#).is_ok());
        assert!(parse_refinement_verdict(r#"{"verdict":{"revise":{"scope":"decomposition"}},"critique":" "}"#).is_err());
    }
}

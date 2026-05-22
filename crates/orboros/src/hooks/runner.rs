//! Hook execution engine. Spawns hook commands with the env overlay
//! and JSON-payload stdin specified in the design doc §4. Routes
//! exit codes (0/1/2) into the abort-or-continue decision the
//! caller needs.
//!
//! Async (`sync=false`) hooks are fire-and-forget: spawn, detach,
//! reap on a `tokio::spawn`'d waiter. Sync (`sync=true`) hooks block
//! until exit or timeout; exit 2 (or timeout-with-`timeout_aborts`)
//! short-circuits the chain.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use chrono::{DateTime, Utc};
use orbs::orb::Orb;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{debug, instrument, warn};

use crate::hooks::config::{HookEntry, HooksConfig};
use crate::hooks::event::HookEvent;
use crate::hooks::matcher::MatcherCtx;

const STDIO_CAPTURE_LIMIT: usize = 16 * 1024;

/// Outcome of firing all hooks for one event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FireOutcome {
    /// All sync hooks returned 0 (or there were no matching hooks).
    Ok,
    /// At least one sync hook returned exit 1 (soft fail). Records
    /// which one for diagnostics; continues the gated action.
    SoftFail { hook_name: String, exit_code: i32 },
    /// A sync hook returned exit 2 (or timed out with `timeout_aborts`).
    /// The caller should abort the gated action.
    Aborted { hook_name: String, exit_code: i32 },
}

impl FireOutcome {
    #[must_use]
    pub fn is_aborted(&self) -> bool {
        matches!(self, FireOutcome::Aborted { .. })
    }
}

/// Per-invocation record. One per hook fired; appended to the audit
/// log by the caller (audit integration is sub-task 6).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookInvocation {
    pub hook_name: String,
    pub event: String,
    pub orb_id: Option<String>,
    pub sync: bool,
    pub started_at: DateTime<Utc>,
    pub duration_ms: u128,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub aborted: bool,
    pub stdout_truncated: String,
    pub stderr_truncated: String,
    pub error: Option<String>,
}

/// JSON payload streamed to the hook's stdin.
#[derive(Debug, Serialize)]
struct HookPayload<'a> {
    event: String,
    fired_at: DateTime<Utc>,
    orb: Option<&'a Orb>,
    transition: Option<TransitionPayload<'a>>,
    worker: Option<WorkerPayload<'a>>,
    pipeline_run_id: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct TransitionPayload<'a> {
    from: Option<&'a str>,
    to: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct WorkerPayload<'a> {
    id: Option<&'a str>,
    session_id: Option<&'a str>,
    model: Option<&'a str>,
}

/// Inputs for a single firing. Distinct from `MatcherCtx` because
/// firing needs more than matching needs (timestamps, IDs that go
/// into the JSON payload).
#[derive(Debug, Default, Clone)]
pub struct FireCtx<'a> {
    pub orb: Option<&'a Orb>,
    pub worker_type: Option<&'a str>,
    pub from_status: Option<&'a str>,
    pub to_status: Option<&'a str>,
    pub worker_id: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub model: Option<&'a str>,
    pub pipeline_run_id: Option<&'a str>,
    /// Set true to skip running the hook (still records an invocation
    /// with `dry_run` env=1). For `orboros hooks check` / dry-run flows.
    pub dry_run: bool,
}

impl<'a> FireCtx<'a> {
    #[must_use]
    pub fn for_orb(orb: &'a Orb) -> Self {
        Self {
            orb: Some(orb),
            ..Self::default()
        }
    }

    fn to_matcher_ctx(&self) -> MatcherCtx<'a> {
        MatcherCtx {
            orb: self.orb,
            worker_type: self.worker_type,
        }
    }
}

/// Top-level firing entry point. Caller passes the config (already
/// loaded), the event, the firing context, and the project cwd that
/// hooks should run in. Returns the aggregate outcome plus a record
/// per invocation.
///
/// # Errors
///
/// Never errors — failures from individual hooks are captured in
/// the per-invocation records. The function only returns `Ok` so
/// callers don't conflate "infra error" with "hook said no."
#[instrument(
    name = "hooks.fire",
    skip(config, ctx),
    fields(
        event = %event,
        orb_id = tracing::field::Empty,
        sync_count = tracing::field::Empty,
        async_count = tracing::field::Empty,
    )
)]
pub async fn fire(
    config: &HooksConfig,
    event: HookEvent,
    ctx: FireCtx<'_>,
    project_cwd: &Path,
) -> (FireOutcome, Vec<HookInvocation>) {
    if let Some(orb) = ctx.orb {
        tracing::Span::current().record("orb_id", tracing::field::display(&orb.id));
    }

    let matcher_ctx = ctx.to_matcher_ctx();
    let matched: Vec<&HookEntry> = config.matching(event, &matcher_ctx);
    if matched.is_empty() {
        return (FireOutcome::Ok, Vec::new());
    }

    let sync_count = matched.iter().filter(|h| h.sync).count();
    let async_count = matched.len() - sync_count;
    tracing::Span::current().record("sync_count", sync_count);
    tracing::Span::current().record("async_count", async_count);

    let mut invocations = Vec::with_capacity(matched.len());
    let mut outcome = FireOutcome::Ok;

    for hook in matched {
        let payload = build_payload(event, hook, &ctx);
        if hook.sync {
            let inv = run_sync(hook, &payload, project_cwd, &ctx).await;
            let res = classify_sync_result(&inv, hook);
            invocations.push(inv);
            match res {
                FireOutcome::Ok => {}
                FireOutcome::SoftFail { .. } => {
                    if matches!(outcome, FireOutcome::Ok) {
                        outcome = res;
                    }
                }
                FireOutcome::Aborted { .. } => {
                    outcome = res;
                    break; // short-circuit: don't run remaining hooks
                }
            }
        } else {
            let inv = spawn_async(hook, &payload, project_cwd, &ctx);
            invocations.push(inv);
        }
    }

    (outcome, invocations)
}

fn classify_sync_result(inv: &HookInvocation, hook: &HookEntry) -> FireOutcome {
    let aborts = inv.exit_code == Some(2) || (inv.timed_out && hook.timeout_aborts) || inv.aborted;
    if aborts {
        return FireOutcome::Aborted {
            hook_name: hook.name.clone(),
            exit_code: inv.exit_code.unwrap_or(-1),
        };
    }
    let soft =
        inv.error.is_some() || inv.timed_out || matches!(inv.exit_code, Some(code) if code != 0);
    if soft {
        FireOutcome::SoftFail {
            hook_name: hook.name.clone(),
            exit_code: inv.exit_code.unwrap_or(-1),
        }
    } else {
        FireOutcome::Ok
    }
}

fn build_payload<'a>(event: HookEvent, _hook: &HookEntry, ctx: &FireCtx<'a>) -> HookPayload<'a> {
    HookPayload {
        event: event.to_string(),
        fired_at: Utc::now(),
        orb: ctx.orb,
        transition: if ctx.from_status.is_some() || ctx.to_status.is_some() {
            Some(TransitionPayload {
                from: ctx.from_status,
                to: ctx.to_status,
            })
        } else {
            None
        },
        worker: if ctx.worker_id.is_some() || ctx.session_id.is_some() || ctx.model.is_some() {
            Some(WorkerPayload {
                id: ctx.worker_id,
                session_id: ctx.session_id,
                model: ctx.model,
            })
        } else {
            None
        },
        pipeline_run_id: ctx.pipeline_run_id,
    }
}

fn env_overlay(event: HookEvent, ctx: &FireCtx<'_>, state_dir: &Path) -> Vec<(String, String)> {
    let mut env = Vec::with_capacity(10);
    env.push(("ORBOROS_EVENT".into(), event.to_string()));
    if let Some(orb) = ctx.orb {
        env.push(("ORBOROS_ORB_ID".into(), orb.id.to_string()));
        env.push(("ORBOROS_ORB_TYPE".into(), orb_type_token(&orb.orb_type)));
        env.push((
            "ORBOROS_ORB_TITLE".into(),
            orb.title.chars().take(200).collect::<String>(),
        ));
    }
    if let Some(f) = ctx.from_status {
        env.push(("ORBOROS_FROM_STATUS".into(), f.into()));
    }
    if let Some(t) = ctx.to_status {
        env.push(("ORBOROS_TO_STATUS".into(), t.into()));
    }
    if let Some(w) = ctx.worker_id {
        env.push(("ORBOROS_WORKER_ID".into(), w.into()));
    }
    if let Some(s) = ctx.session_id {
        env.push(("ORBOROS_SESSION_ID".into(), s.into()));
    }
    if let Some(p) = ctx.pipeline_run_id {
        env.push(("ORBOROS_PIPELINE_RUN_ID".into(), p.into()));
    }
    env.push((
        "ORBOROS_STATE_DIR".into(),
        state_dir.to_string_lossy().into_owned(),
    ));
    if ctx.dry_run {
        env.push(("ORBOROS_DRY_RUN".into(), "1".into()));
    }
    env
}

fn orb_type_token(t: &orbs::orb::OrbType) -> String {
    match t {
        orbs::orb::OrbType::Epic => "epic".into(),
        orbs::orb::OrbType::Feature => "feature".into(),
        orbs::orb::OrbType::Task => "task".into(),
        orbs::orb::OrbType::Bug => "bug".into(),
        orbs::orb::OrbType::Chore => "chore".into(),
        orbs::orb::OrbType::Docs => "docs".into(),
        orbs::orb::OrbType::Custom(name) => format!("custom:{name}"),
    }
}

fn split_command(cmd: &str) -> Result<(String, Vec<String>), shell_words::ParseError> {
    let parts = shell_words::split(cmd)?;
    if parts.is_empty() {
        return Err(shell_words::ParseError);
    }
    let mut iter = parts.into_iter();
    let head = iter.next().unwrap();
    Ok((head, iter.collect()))
}

async fn run_sync(
    hook: &HookEntry,
    payload: &HookPayload<'_>,
    project_cwd: &Path,
    ctx: &FireCtx<'_>,
) -> HookInvocation {
    let started_at = Utc::now();
    let started = std::time::Instant::now();
    let mut inv = HookInvocation {
        hook_name: hook.name.clone(),
        event: payload.event.clone(),
        orb_id: ctx.orb.map(|o| o.id.to_string()),
        sync: true,
        started_at,
        duration_ms: 0,
        exit_code: None,
        timed_out: false,
        aborted: false,
        stdout_truncated: String::new(),
        stderr_truncated: String::new(),
        error: None,
    };
    if ctx.dry_run {
        debug!(hook = %hook.name, "dry_run; skipping spawn");
        inv.duration_ms = started.elapsed().as_millis();
        return inv;
    }

    let Ok((head, args)) = split_command(&hook.run) else {
        inv.error = Some(format!("could not parse hook run command: {}", hook.run));
        inv.duration_ms = started.elapsed().as_millis();
        return inv;
    };
    let envs = env_overlay(payload_event(payload), ctx, project_cwd);
    let payload_json = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());

    let mut cmd = Command::new(&head);
    cmd.args(&args)
        .current_dir(project_cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (k, v) in &envs {
        cmd.env(k, v);
    }

    let spawn_result = cmd.spawn();
    let mut child = match spawn_result {
        Ok(c) => c,
        Err(e) => {
            inv.error = Some(format!("spawn failed: {e}"));
            inv.duration_ms = started.elapsed().as_millis();
            return inv;
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload_json.as_bytes()).await;
        let _ = stdin.shutdown().await;
    }

    let timeout = Duration::from_millis(hook.timeout_ms);
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            inv.exit_code = output.status.code();
            inv.stdout_truncated = truncate_bytes(&output.stdout);
            inv.stderr_truncated = truncate_bytes(&output.stderr);
        }
        Ok(Err(e)) => {
            inv.error = Some(format!("wait failed: {e}"));
        }
        Err(_) => {
            inv.timed_out = true;
            inv.error = Some(format!("timed out after {timeout:?}"));
        }
    }
    inv.duration_ms = started.elapsed().as_millis();
    inv.aborted = inv.exit_code == Some(2) || (inv.timed_out && hook.timeout_aborts);
    if inv.exit_code == Some(0) {
        debug!(hook = %hook.name, "hook OK");
    } else {
        warn!(
            hook = %hook.name,
            exit = ?inv.exit_code,
            timed_out = inv.timed_out,
            "hook non-zero result"
        );
    }
    inv
}

fn spawn_async(
    hook: &HookEntry,
    payload: &HookPayload<'_>,
    project_cwd: &Path,
    ctx: &FireCtx<'_>,
) -> HookInvocation {
    // For async hooks we don't await completion. Record the dispatch
    // and let a background task reap the child.
    let started_at = Utc::now();
    let mut inv = HookInvocation {
        hook_name: hook.name.clone(),
        event: payload.event.clone(),
        orb_id: ctx.orb.map(|o| o.id.to_string()),
        sync: false,
        started_at,
        duration_ms: 0,
        exit_code: None,
        timed_out: false,
        aborted: false,
        stdout_truncated: String::new(),
        stderr_truncated: String::new(),
        error: None,
    };
    if ctx.dry_run {
        debug!(hook = %hook.name, "dry_run; skipping async spawn");
        return inv;
    }
    let Ok((head, args)) = split_command(&hook.run) else {
        inv.error = Some(format!("could not parse hook run command: {}", hook.run));
        return inv;
    };
    let envs = env_overlay(payload_event(payload), ctx, project_cwd);
    let payload_json = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());

    let mut cmd = Command::new(&head);
    cmd.args(&args)
        .current_dir(project_cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Note: deliberately NOT kill_on_drop — async hooks outlive the
    // span that fired them. The user can `orboros hooks list --running`
    // (future CLI work) to inspect.
    for (k, v) in &envs {
        cmd.env(k, v);
    }

    match cmd.spawn() {
        Ok(mut child) => {
            if let Some(mut stdin) = child.stdin.take() {
                let buf = payload_json.into_bytes();
                tokio::spawn(async move {
                    let _ = stdin.write_all(&buf).await;
                    let _ = stdin.shutdown().await;
                });
            }
            let hook_name = hook.name.clone();
            let parent = tracing::Span::current();
            tokio::spawn(async move {
                let waited = child.wait().await;
                let span = tracing::info_span!("hooks.async_complete", hook = %hook_name);
                span.follows_from(parent);
                let _enter = span.enter();
                match waited {
                    Ok(status) => debug!(exit = ?status.code(), "async hook exited"),
                    Err(e) => warn!(error = %e, "async hook wait failed"),
                }
            });
        }
        Err(e) => {
            inv.error = Some(format!("spawn failed: {e}"));
        }
    }
    inv
}

fn payload_event(p: &HookPayload<'_>) -> HookEvent {
    p.event.parse().unwrap_or(HookEvent::OnQueueTick)
}

fn truncate_bytes(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let limit = bytes.len().min(STDIO_CAPTURE_LIMIT);
    String::from_utf8_lossy(&bytes[..limit]).into_owned()
}

/// Light-weight wrapper around the firing function used by callers
/// that just want to find which hooks WOULD fire and produce env
/// strings — e.g. `orboros hooks check`.
#[must_use]
pub fn preview(
    config: &HooksConfig,
    event: HookEvent,
    ctx: &FireCtx<'_>,
    state_dir: &Path,
) -> Vec<HookPreview> {
    let matcher_ctx = ctx.to_matcher_ctx();
    config
        .matching(event, &matcher_ctx)
        .into_iter()
        .map(|hook| HookPreview {
            name: hook.name.clone(),
            run: hook.run.clone(),
            sync: hook.sync,
            timeout_ms: hook.timeout_ms,
            env_overlay: env_overlay(event, ctx, state_dir)
                .into_iter()
                .collect::<HashMap<_, _>>(),
        })
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct HookPreview {
    pub name: String,
    pub run: String,
    pub sync: bool,
    pub timeout_ms: u64,
    pub env_overlay: HashMap<String, String>,
}

/// Parses the JSON payload structure for downstream tests / consumers
/// that want to inspect what a hook would receive on stdin without
/// actually firing one.
///
/// # Errors
///
/// Returns serde error on invalid JSON.
pub fn parse_payload_for_test(s: &str) -> Result<JsonValue, serde_json::Error> {
    serde_json::from_str(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::config::HooksConfig;
    use orbs::orb::{Orb, OrbType};
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command as StdCommand;

    fn write_config(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("hooks.toml");
        fs::write(&path, body).unwrap();
        path
    }

    fn make_executable(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(path, perms).unwrap();
        }
        #[cfg(not(unix))]
        let _ = path;
    }

    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        make_executable(&path);
        path
    }

    fn make_orb() -> Orb {
        Orb::new("Test orb", "do the thing").with_type(OrbType::Task)
    }

    fn make_ctx(orb: &Orb) -> FireCtx<'_> {
        FireCtx::for_orb(orb)
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn sync_hook_exit_0_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        write_script(dir.path(), "hook.sh", "#!/bin/sh\nexit 0\n");
        write_config(
            dir.path(),
            r#"
            [[hook]]
            name = "ok"
            on = "post-worker-complete"
            run = "./hook.sh"
            sync = true
        "#,
        );
        let mut config = HooksConfig::load(None, Some(&dir.path().join("hooks.toml"))).unwrap();
        for h in &mut config.hooks {
            h.timeout_ms = 5_000;
        }
        let orb = make_orb();
        let ctx = make_ctx(&orb);
        let (outcome, invs) = fire(&config, HookEvent::PostWorkerComplete, ctx, dir.path()).await;
        assert_eq!(outcome, FireOutcome::Ok);
        assert_eq!(invs.len(), 1);
        assert_eq!(invs[0].exit_code, Some(0));
        assert!(!invs[0].timed_out);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn sync_hook_exit_1_is_soft_fail() {
        let dir = tempfile::tempdir().unwrap();
        write_script(dir.path(), "hook.sh", "#!/bin/sh\nexit 1\n");
        write_config(
            dir.path(),
            r#"
            [[hook]]
            name = "soft"
            on = "post-worker-complete"
            run = "./hook.sh"
            sync = true
        "#,
        );
        let config = HooksConfig::load(None, Some(&dir.path().join("hooks.toml"))).unwrap();
        let orb = make_orb();
        let (outcome, invs) = fire(
            &config,
            HookEvent::PostWorkerComplete,
            make_ctx(&orb),
            dir.path(),
        )
        .await;
        assert!(matches!(outcome, FireOutcome::SoftFail { .. }));
        assert_eq!(invs[0].exit_code, Some(1));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn sync_hook_exit_2_aborts_and_short_circuits() {
        let dir = tempfile::tempdir().unwrap();
        write_script(dir.path(), "abort.sh", "#!/bin/sh\nexit 2\n");
        write_script(dir.path(), "never.sh", "#!/bin/sh\necho NEVER\nexit 0\n");
        write_config(
            dir.path(),
            r#"
            [[hook]]
            name = "blocker"
            on = "post-worker-complete"
            run = "./abort.sh"
            sync = true

            [[hook]]
            name = "should-not-run"
            on = "post-worker-complete"
            run = "./never.sh"
            sync = true
        "#,
        );
        let config = HooksConfig::load(None, Some(&dir.path().join("hooks.toml"))).unwrap();
        let orb = make_orb();
        let (outcome, invs) = fire(
            &config,
            HookEvent::PostWorkerComplete,
            make_ctx(&orb),
            dir.path(),
        )
        .await;
        match outcome {
            FireOutcome::Aborted {
                hook_name,
                exit_code,
            } => {
                assert_eq!(hook_name, "blocker");
                assert_eq!(exit_code, 2);
            }
            other => panic!("expected Aborted, got {other:?}"),
        }
        // Only the blocker was recorded; second hook never ran.
        assert_eq!(invs.len(), 1);
        assert_eq!(invs[0].hook_name, "blocker");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn sync_hook_timeout_is_soft_fail_by_default() {
        let dir = tempfile::tempdir().unwrap();
        write_script(dir.path(), "slow.sh", "#!/bin/sh\nsleep 5\n");
        write_config(
            dir.path(),
            r#"
            [[hook]]
            name = "slow"
            on = "post-worker-complete"
            run = "./slow.sh"
            sync = true
            timeout_ms = 150
        "#,
        );
        let config = HooksConfig::load(None, Some(&dir.path().join("hooks.toml"))).unwrap();
        let orb = make_orb();
        let (outcome, invs) = fire(
            &config,
            HookEvent::PostWorkerComplete,
            make_ctx(&orb),
            dir.path(),
        )
        .await;
        assert!(invs[0].timed_out);
        // timeout_aborts not set → soft fail, not abort.
        assert!(matches!(outcome, FireOutcome::SoftFail { .. }));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn sync_hook_timeout_aborts_when_flag_set() {
        let dir = tempfile::tempdir().unwrap();
        write_script(dir.path(), "slow.sh", "#!/bin/sh\nsleep 5\n");
        write_config(
            dir.path(),
            r#"
            [[hook]]
            name = "slow-blocker"
            on = "post-worker-complete"
            run = "./slow.sh"
            sync = true
            timeout_ms = 150
            timeout_aborts = true
        "#,
        );
        let config = HooksConfig::load(None, Some(&dir.path().join("hooks.toml"))).unwrap();
        let orb = make_orb();
        let (outcome, _invs) = fire(
            &config,
            HookEvent::PostWorkerComplete,
            make_ctx(&orb),
            dir.path(),
        )
        .await;
        assert!(matches!(outcome, FireOutcome::Aborted { .. }));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn env_overlay_is_visible_to_hook() {
        let dir = tempfile::tempdir().unwrap();
        write_script(
            dir.path(),
            "env-check.sh",
            "#!/bin/sh\n\
             [ -n \"$ORBOROS_EVENT\" ] || exit 2\n\
             [ -n \"$ORBOROS_ORB_ID\" ] || exit 2\n\
             [ \"$ORBOROS_ORB_TYPE\" = \"task\" ] || exit 2\n\
             exit 0\n",
        );
        write_config(
            dir.path(),
            r#"
            [[hook]]
            name = "env-check"
            on = "post-worker-complete"
            run = "./env-check.sh"
            sync = true
        "#,
        );
        let config = HooksConfig::load(None, Some(&dir.path().join("hooks.toml"))).unwrap();
        let orb = make_orb();
        let (outcome, _invs) = fire(
            &config,
            HookEvent::PostWorkerComplete,
            make_ctx(&orb),
            dir.path(),
        )
        .await;
        assert_eq!(outcome, FireOutcome::Ok);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn stdin_payload_is_json_with_event_and_orb() {
        let dir = tempfile::tempdir().unwrap();
        let captured = dir.path().join("payload.json");
        let script = format!(
            "#!/bin/sh\ncat > {captured}\nexit 0\n",
            captured = captured.display()
        );
        write_script(dir.path(), "capture.sh", &script);
        write_config(
            dir.path(),
            r#"
            [[hook]]
            name = "capture"
            on = "post-worker-complete"
            run = "./capture.sh"
            sync = true
        "#,
        );
        let config = HooksConfig::load(None, Some(&dir.path().join("hooks.toml"))).unwrap();
        let orb = make_orb();
        let _ = fire(
            &config,
            HookEvent::PostWorkerComplete,
            make_ctx(&orb),
            dir.path(),
        )
        .await;

        let raw = fs::read_to_string(&captured).unwrap();
        let payload: JsonValue = parse_payload_for_test(&raw).unwrap();
        assert_eq!(payload["event"], "post-worker-complete");
        assert_eq!(payload["orb"]["id"], orb.id.to_string());
        assert!(payload.get("fired_at").is_some());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn async_hook_does_not_block_outcome() {
        let dir = tempfile::tempdir().unwrap();
        write_script(dir.path(), "slow.sh", "#!/bin/sh\nsleep 1\n");
        write_config(
            dir.path(),
            r#"
            [[hook]]
            name = "fire-and-forget"
            on = "post-worker-complete"
            run = "./slow.sh"
            sync = false
        "#,
        );
        let config = HooksConfig::load(None, Some(&dir.path().join("hooks.toml"))).unwrap();
        let orb = make_orb();
        let started = std::time::Instant::now();
        let (outcome, invs) = fire(
            &config,
            HookEvent::PostWorkerComplete,
            make_ctx(&orb),
            dir.path(),
        )
        .await;
        let elapsed = started.elapsed();
        assert_eq!(outcome, FireOutcome::Ok);
        // We spawned the async hook but should not have waited the full second.
        assert!(elapsed < Duration::from_millis(500), "took {elapsed:?}");
        assert_eq!(invs.len(), 1);
        assert!(!invs[0].sync);
        // exit_code is None for async — we didn't wait.
        assert_eq!(invs[0].exit_code, None);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn dry_run_skips_spawn_but_records_invocation() {
        let dir = tempfile::tempdir().unwrap();
        // No script needed; dry-run should not invoke it.
        write_config(
            dir.path(),
            r#"
            [[hook]]
            name = "dry"
            on = "post-worker-complete"
            run = "./does-not-exist.sh"
            sync = true
        "#,
        );
        let config = HooksConfig::load(None, Some(&dir.path().join("hooks.toml"))).unwrap();
        let orb = make_orb();
        let mut ctx = make_ctx(&orb);
        ctx.dry_run = true;
        let (outcome, invs) = fire(&config, HookEvent::PostWorkerComplete, ctx, dir.path()).await;
        assert_eq!(outcome, FireOutcome::Ok);
        assert_eq!(invs.len(), 1);
        assert_eq!(invs[0].exit_code, None);
        assert!(invs[0].error.is_none());
    }

    #[tokio::test]
    async fn fire_with_no_matching_hooks_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let config = HooksConfig::default();
        let orb = make_orb();
        let (outcome, invs) = fire(
            &config,
            HookEvent::PostWorkerComplete,
            make_ctx(&orb),
            dir.path(),
        )
        .await;
        assert_eq!(outcome, FireOutcome::Ok);
        assert!(invs.is_empty());
    }

    #[test]
    fn preview_returns_env_overlay() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            r#"
            [[hook]]
            name = "p"
            on = "on-orb-create"
            run = "echo hi"
        "#,
        );
        let config = HooksConfig::load(None, Some(&dir.path().join("hooks.toml"))).unwrap();
        let orb = make_orb();
        let ctx = make_ctx(&orb);
        let previews = preview(&config, HookEvent::OnOrbCreate, &ctx, dir.path());
        assert_eq!(previews.len(), 1);
        assert_eq!(
            previews[0].env_overlay.get("ORBOROS_EVENT"),
            Some(&"on-orb-create".to_string())
        );
        assert!(previews[0].env_overlay.contains_key("ORBOROS_ORB_ID"));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn malformed_run_command_is_soft_failure_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        // shell-words rejects unterminated quotes.
        write_config(
            dir.path(),
            r#"
            [[hook]]
            name = "bad"
            on = "post-worker-complete"
            run = "echo 'unterminated"
            sync = true
        "#,
        );
        let config = HooksConfig::load(None, Some(&dir.path().join("hooks.toml"))).unwrap();
        let orb = make_orb();
        let (outcome, invs) = fire(
            &config,
            HookEvent::PostWorkerComplete,
            make_ctx(&orb),
            dir.path(),
        )
        .await;
        assert!(matches!(outcome, FireOutcome::SoftFail { .. }));
        assert!(invs[0].error.as_deref().unwrap().contains("parse"));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn missing_binary_is_soft_failure() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            r#"
            [[hook]]
            name = "missing"
            on = "post-worker-complete"
            run = "./does-not-exist"
            sync = true
        "#,
        );
        let config = HooksConfig::load(None, Some(&dir.path().join("hooks.toml"))).unwrap();
        let orb = make_orb();
        let (outcome, invs) = fire(
            &config,
            HookEvent::PostWorkerComplete,
            make_ctx(&orb),
            dir.path(),
        )
        .await;
        assert!(matches!(outcome, FireOutcome::SoftFail { .. }));
        assert!(invs[0].error.as_deref().unwrap().contains("spawn"));
    }

    // Sanity check that `sh` is on PATH where these tests run; the rest
    // of the suite assumes it. If it isn't, fail loudly here rather than
    // silently elsewhere.
    #[test]
    #[cfg(unix)]
    fn sh_is_available_for_tests() {
        let out = StdCommand::new("sh").arg("-c").arg("exit 0").status();
        assert!(out.is_ok());
    }
}

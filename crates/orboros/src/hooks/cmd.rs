//! `orboros hooks` subcommand handlers.
//!
//! `list`  — prints the loaded hooks in source order with their
//!           event, source layer, sync mode, and any match summary.
//! `check` — parses the config + reports a one-line OK/ERROR
//!           per layer. Useful from a pre-commit hook on the
//!           hooks.toml itself.
//! `run`   — manually fires a single hook by name against a chosen
//!           orb. Dry-run mode shows what would be emitted without
//!           spawning anything.

use std::path::Path;

use anyhow::Context;
use orbs::id::OrbId;
use orbs::orb_store::OrbStore;

use crate::hooks::config::{default_paths, ConfigLayer, HookEntry, HooksConfig};
use crate::hooks::log::HookLog;
use crate::hooks::runner::FireCtx;
use crate::hooks::sink::HookSink;

/// `orboros hooks list`. Prints every loaded hook in source order.
///
/// # Errors
///
/// Returns an error if the config fails to load.
pub fn cmd_hooks_list(state_dir: &Path) -> anyhow::Result<()> {
    let (global, project) = default_paths(state_dir);
    let config = HooksConfig::load(global.as_deref(), project.as_deref())
        .context("failed to load hooks config")?;

    if config.hooks.is_empty() {
        println!("(no hooks)");
        if let Some(p) = &global {
            println!("  global path: {} ({})", p.display(), exist_label(p));
        }
        if let Some(p) = &project {
            println!("  project path: {} ({})", p.display(), exist_label(p));
        }
        return Ok(());
    }

    for hook in &config.hooks {
        print_hook(hook);
    }
    println!("\n{} hook(s) loaded.", config.hooks.len());
    Ok(())
}

/// `orboros hooks check`. Loads the config and reports OK / per-layer
/// errors. Returns non-zero on parse error so this is usable in
/// pre-commit hooks against hooks.toml itself.
///
/// # Errors
///
/// Returns an error if either layer fails to parse.
pub fn cmd_hooks_check(state_dir: &Path) -> anyhow::Result<()> {
    let (global, project) = default_paths(state_dir);

    for (layer, path) in [
        ("global", global.as_deref()),
        ("project", project.as_deref()),
    ] {
        match path {
            None => println!("[{layer}] (no default path)"),
            Some(p) if !p.exists() => println!("[{layer}] {} — not present", p.display()),
            Some(p) => {
                match HooksConfig::load(
                    if layer == "global" { Some(p) } else { None },
                    if layer == "project" { Some(p) } else { None },
                ) {
                    Ok(cfg) => {
                        println!("[{layer}] {} — OK ({} hooks)", p.display(), cfg.hooks.len());
                    }
                    Err(e) => println!("[{layer}] {} — ERROR: {e}", p.display()),
                }
            }
        }
    }

    // Final consolidated load to surface duplicate-name warnings via tracing.
    let _ = HooksConfig::load(global.as_deref(), project.as_deref())?;
    Ok(())
}

/// `orboros hooks run <name> --orb <id>`. Manually fires a named
/// hook against a chosen orb. Useful for testing scripts in
/// isolation. Honors `--dry-run` by setting `ORBOROS_DRY_RUN=1` and
/// skipping the actual spawn.
///
/// # Errors
///
/// Returns an error if no hook with `name` is found, the orb id is
/// not in the store, or the hooks sink fails to load.
pub fn cmd_hooks_run(
    state_dir: &Path,
    name: &str,
    orb_id: &str,
    dry_run: bool,
) -> anyhow::Result<()> {
    let project_cwd = std::env::current_dir().unwrap_or_else(|_| state_dir.to_path_buf());
    let sink = HookSink::from_state_dir(state_dir, &project_cwd)?
        .ok_or_else(|| anyhow::anyhow!("no hooks.toml configured (global or project)"))?;
    let hook = sink
        .config
        .hooks
        .iter()
        .find(|h| h.name == name)
        .ok_or_else(|| anyhow::anyhow!("no hook named '{name}' (try `orboros hooks list`)"))?;

    let orb_store = OrbStore::new(state_dir.join("orbs.jsonl"));
    let orb = orb_store
        .load_by_id(&OrbId::from_raw(orb_id))
        .context("failed to load orb")?
        .ok_or_else(|| anyhow::anyhow!("orb {orb_id} not found"))?;

    let mut ctx = FireCtx::for_orb(&orb);
    ctx.dry_run = dry_run;

    let (outcome, invocations) = sink.fire_blocking(hook.on, ctx)?;
    print_outcome(name, &outcome, &invocations);
    Ok(())
}

fn print_hook(hook: &HookEntry) {
    let layer = match hook.source {
        ConfigLayer::Global => "global",
        ConfigLayer::Project => "project",
    };
    let mode = if hook.sync { "sync" } else { "async" };
    let enabled = if hook.enabled { " " } else { " (disabled) " };
    println!(
        "[{layer:>7}]{enabled}{name:<30} on={event} {mode} timeout={timeout_ms}ms",
        name = hook.name,
        event = hook.on,
        timeout_ms = hook.timeout_ms,
    );
    let summary = match_summary(&hook.match_rules);
    if !summary.is_empty() {
        println!("            match: {summary}");
    }
    println!("            run:   {}", hook.run);
}

fn match_summary(m: &crate::hooks::config::HookMatch) -> String {
    let mut parts = Vec::new();
    if let Some(types) = &m.orb_type {
        parts.push(format!(
            "orb_type={}",
            types
                .iter()
                .map(|t| format!("{t:?}").to_lowercase())
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    if let Some(labels) = &m.labels_any {
        parts.push(format!("labels={}", labels.join(",")));
    }
    if let Some(labels) = &m.labels_all {
        parts.push(format!("labels_all={}", labels.join(",")));
    }
    if let Some(s) = &m.status {
        parts.push(format!(
            "status={}",
            s.iter()
                .map(|x| format!("{x:?}").to_lowercase())
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    if let Some(p) = &m.phase {
        parts.push(format!(
            "phase={}",
            p.iter()
                .map(|x| format!("{x:?}").to_lowercase())
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    if let Some(v) = m.priority_max {
        parts.push(format!("priority_max={v}"));
    }
    if let Some(v) = m.priority_min {
        parts.push(format!("priority_min={v}"));
    }
    if let Some(wt) = &m.worker_type {
        parts.push(format!("worker_type={wt}"));
    }
    if m.title_regex.is_some() {
        parts.push("title_regex=<set>".into());
    }
    if m.description_regex.is_some() {
        parts.push("description_regex=<set>".into());
    }
    if let Some(scope) = &m.scope_includes {
        parts.push(format!("scope_includes={}", scope.join(",")));
    }
    parts.join(" ")
}

fn print_outcome(
    hook_name: &str,
    outcome: &crate::hooks::runner::FireOutcome,
    invocations: &[crate::hooks::runner::HookInvocation],
) {
    use crate::hooks::runner::FireOutcome;
    match outcome {
        FireOutcome::Ok => println!("[OK] hook '{hook_name}' ran successfully"),
        FireOutcome::SoftFail {
            hook_name,
            exit_code,
        } => {
            println!("[SOFT-FAIL] hook '{hook_name}' returned exit {exit_code}");
        }
        FireOutcome::Aborted {
            hook_name,
            exit_code,
        } => {
            println!(
                "[ABORTED] hook '{hook_name}' returned exit {exit_code} (would block gated action)"
            );
        }
    }
    for inv in invocations {
        if let Some(code) = inv.exit_code {
            println!(
                "  - {} (exit={}, {}ms)",
                inv.hook_name, code, inv.duration_ms
            );
        } else if inv.error.is_some() {
            println!(
                "  - {} (error: {})",
                inv.hook_name,
                inv.error.as_deref().unwrap_or("")
            );
        } else if !inv.sync {
            println!("  - {} (async dispatched)", inv.hook_name);
        }
        if !inv.stdout_truncated.trim().is_empty() {
            println!("    stdout: {}", inv.stdout_truncated.trim());
        }
        if !inv.stderr_truncated.trim().is_empty() {
            println!("    stderr: {}", inv.stderr_truncated.trim());
        }
    }
}

/// `orboros hooks log [--orb id] [--limit N]`. Prints the hook
/// invocation log, optionally filtered by orb id. Default limit is
/// 50; pass `--limit 0` to show everything.
///
/// # Errors
///
/// Returns an error if the log file cannot be read.
pub fn cmd_hooks_log(
    state_dir: &Path,
    orb_filter: Option<&str>,
    limit: usize,
) -> anyhow::Result<()> {
    let log = HookLog::new(state_dir);
    let entries = match orb_filter {
        Some(id) => log.read_for_orb(id)?,
        None => log.read_all()?,
    };
    if entries.is_empty() {
        println!("(no hook invocations recorded)");
        println!("  log path: {}", log.path().display());
        return Ok(());
    }
    let total = entries.len();
    let shown: Box<dyn Iterator<Item = &crate::hooks::log::HookLogEntry>> = if limit == 0 {
        Box::new(entries.iter())
    } else {
        // Show the most recent N.
        Box::new(entries.iter().skip(total.saturating_sub(limit)))
    };
    for entry in shown {
        let inv = &entry.invocation;
        let exit = inv.exit_code.map_or("-".to_string(), |c| c.to_string());
        let orb = inv.orb_id.as_deref().unwrap_or("-");
        println!(
            "{ts}  [{outcome:<9}] {hook:<30} event={event} orb={orb} exit={exit} ms={ms}",
            ts = inv.started_at.format("%Y-%m-%d %H:%M:%S"),
            outcome = entry.outcome_label,
            hook = inv.hook_name,
            event = inv.event,
            ms = inv.duration_ms,
        );
        if let Some(e) = &inv.error {
            println!("    error: {e}");
        }
    }
    if limit != 0 && total > limit {
        println!("\n(showing latest {limit} of {total}; --limit 0 for all)");
    }
    Ok(())
}

fn exist_label(p: &Path) -> &'static str {
    if p.exists() {
        "exists"
    } else {
        "not present"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orbs::orb::{Orb, OrbType};
    use std::fs;

    fn write_config(dir: &Path, body: &str) {
        fs::write(dir.join("hooks.toml"), body).unwrap();
    }

    #[test]
    fn cmd_hooks_list_handles_no_hooks() {
        let dir = tempfile::tempdir().unwrap();
        cmd_hooks_list(dir.path()).unwrap();
    }

    #[test]
    fn cmd_hooks_list_prints_loaded_hooks() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            r#"
            [[hook]]
            name = "fmt-after-edit"
            on = "post-worker-complete"
            run = "cargo fmt"
            match.orb_type = "task"
        "#,
        );
        cmd_hooks_list(dir.path()).unwrap();
    }

    #[test]
    fn cmd_hooks_check_handles_no_files() {
        let dir = tempfile::tempdir().unwrap();
        cmd_hooks_check(dir.path()).unwrap();
    }

    #[test]
    fn cmd_hooks_check_reports_malformed_file_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            r#"
            [[hook]]
            on = "not-real"
            run = "x"
        "#,
        );
        // check errors at the consolidated load step → propagates.
        let err = cmd_hooks_check(dir.path()).unwrap_err();
        assert!(format!("{err:?}").contains("not-real"));
    }

    #[test]
    fn cmd_hooks_run_fails_when_no_config() {
        let dir = tempfile::tempdir().unwrap();
        let orb_store = OrbStore::new(dir.path().join("orbs.jsonl"));
        let orb = Orb::new("test", "desc").with_type(OrbType::Task);
        orb_store.append(&orb).unwrap();
        let err = cmd_hooks_run(dir.path(), "any", orb.id.as_str(), false).unwrap_err();
        assert!(format!("{err}").contains("no hooks.toml"));
    }

    #[test]
    fn cmd_hooks_run_fails_on_unknown_hook_name() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            r#"
            [[hook]]
            name = "known"
            on = "on-orb-create"
            run = "echo hi"
        "#,
        );
        let orb_store = OrbStore::new(dir.path().join("orbs.jsonl"));
        let orb = Orb::new("test", "desc").with_type(OrbType::Task);
        orb_store.append(&orb).unwrap();
        let err = cmd_hooks_run(dir.path(), "unknown", orb.id.as_str(), false).unwrap_err();
        assert!(format!("{err}").contains("no hook named"));
    }

    #[test]
    fn cmd_hooks_run_fails_on_unknown_orb() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            r#"
            [[hook]]
            name = "known"
            on = "on-orb-create"
            run = "echo hi"
        "#,
        );
        // No orb store at all.
        let err = cmd_hooks_run(dir.path(), "known", "orb-ghost", true).unwrap_err();
        assert!(format!("{err}").contains("not found"));
    }
}

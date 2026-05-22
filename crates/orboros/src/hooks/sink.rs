//! Thin wrapper that bundles a loaded `HooksConfig` with the
//! filesystem context needed at firing time. Saves call sites from
//! threading 2-3 separate args.
//!
//! The sink is meant to be built once at startup (via
//! `HookSink::from_state_dir`) and passed by reference into command
//! handlers. Call sites that don't need hooks can simply ignore an
//! `Option<&HookSink>`.

use std::path::{Path, PathBuf};

use crate::hooks::config::HooksConfig;
use crate::hooks::event::HookEvent;
use crate::hooks::log::HookLog;
use crate::hooks::runner::{fire, FireCtx, FireOutcome, HookInvocation};

/// Bundles a `HooksConfig` with the state and working directories
/// hooks need at fire time, plus a `HookLog` for audit-trail
/// persistence.
#[derive(Debug, Clone)]
pub struct HookSink {
    pub config: HooksConfig,
    pub state_dir: PathBuf,
    pub project_cwd: PathBuf,
    pub log: HookLog,
}

impl HookSink {
    /// Loads hooks from `~/.orboros/hooks.toml` (global) and
    /// `<state_dir>/hooks.toml` (project). Returns `None` if both
    /// files are absent — the caller can opt out without an explicit
    /// `enabled` knob.
    ///
    /// # Errors
    ///
    /// Returns an error if either layer fails to parse, an unknown
    /// event name appears, or a regex fails to compile.
    pub fn from_state_dir(state_dir: &Path, project_cwd: &Path) -> anyhow::Result<Option<Self>> {
        let (global, project) = crate::hooks::config::default_paths(state_dir);
        let global_exists = global.as_ref().is_some_and(|p| p.exists());
        let project_exists = project.as_ref().is_some_and(|p| p.exists());
        if !global_exists && !project_exists {
            return Ok(None);
        }
        let config = HooksConfig::load(global.as_deref(), project.as_deref())?;
        Ok(Some(Self {
            config,
            state_dir: state_dir.to_path_buf(),
            project_cwd: project_cwd.to_path_buf(),
            log: HookLog::new(state_dir),
        }))
    }

    /// Asynchronous firing path. Call from async contexts. Records
    /// each invocation to the hook log; persistence failures are
    /// logged via tracing but never propagated — the hook itself
    /// already ran.
    pub async fn fire(
        &self,
        event: HookEvent,
        ctx: FireCtx<'_>,
    ) -> (FireOutcome, Vec<HookInvocation>) {
        let (outcome, invocations) = fire(&self.config, event, ctx, &self.project_cwd).await;
        if let Err(e) = self.log.append_batch(&invocations, &outcome) {
            tracing::warn!(error = %e, "failed to append hook invocations to log");
        }
        (outcome, invocations)
    }

    /// Synchronous firing path for CLI commands. Spins up a small
    /// tokio runtime, fires, returns. Cheap relative to the work the
    /// hooks themselves do.
    ///
    /// # Errors
    ///
    /// Returns an error only if the tokio runtime cannot be created;
    /// individual hook failures are captured in `HookInvocation` records.
    pub fn fire_blocking(
        &self,
        event: HookEvent,
        ctx: FireCtx<'_>,
    ) -> anyhow::Result<(FireOutcome, Vec<HookInvocation>)> {
        let rt = tokio::runtime::Runtime::new()?;
        Ok(rt.block_on(self.fire(event, ctx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn from_state_dir_no_files_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let sink = HookSink::from_state_dir(dir.path(), dir.path()).unwrap();
        assert!(sink.is_none());
    }

    #[test]
    fn from_state_dir_with_project_file_loads() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("hooks.toml"),
            r#"
            [[hook]]
            on = "on-orb-create"
            run = "echo hi"
        "#,
        )
        .unwrap();
        let sink = HookSink::from_state_dir(dir.path(), dir.path()).unwrap();
        assert!(sink.is_some());
        let sink = sink.unwrap();
        assert_eq!(sink.config.hooks.len(), 1);
    }
}

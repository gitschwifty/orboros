//! Benchmark corpus + harness (task 59).
//!
//! Three tiers of orboros benchmarks:
//! - **T1** — Single-shot tasks (~10–20 cases). Self-contained prompts
//!   with known-good answers. Tests pure worker behavior.
//! - **T2** — Targeted Orboros capabilities in seeded repos (~5–10).
//!   Seed repo + expected diff or test pass.
//! - **T3** — Full end-to-end Orboros runs (~2–5). Short-prompt
//!   greenfield cases and prewritten plan/spec cases both use normal
//!   Orboros execution under benchmark isolation, graded by artifact
//!   checks or a rubric grader.
//!
//! Result store: JSONL under `.orbs/bench/`. CLI: `orboros bench
//! list / run / show / compare / calibration`.

pub mod calibration;
pub mod case;
pub mod cmd;
pub mod runner;
pub mod runner_t2t3;
pub mod store;

use std::path::Path;
use std::process::Command;

#[must_use]
pub fn git_head_commit(path: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let commit = String::from_utf8(output.stdout).ok()?;
    let commit = commit.trim();
    (!commit.is_empty()).then(|| commit.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn git_head_commit_returns_none_for_non_git_dir() {
        let dir = tempdir().unwrap();
        assert_eq!(git_head_commit(dir.path()), None);
    }
}

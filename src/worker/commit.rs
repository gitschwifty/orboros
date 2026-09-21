//! Read-only verification of worker-authored completion commits.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

/// Verified completion commit stored in the execution ledger.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitEvidence {
    pub sha: String,
    pub subject: String,
}

pub(crate) struct Snapshot {
    root: PathBuf,
    head: String,
    residual: Vec<Vec<u8>>,
    dirty_paths: BTreeSet<Vec<u8>>,
}

async fn git(root: &Path, args: &[&str]) -> anyhow::Result<Vec<u8>> {
    let output = tokio::process::Command::new("git")
        .arg("--no-optional-locks")
        .args(args)
        .current_dir(root)
        .kill_on_drop(true)
        .output()
        .await
        .context("reading orb Git state")?;
    if !output.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output.stdout)
}

async fn head(root: &Path) -> anyhow::Result<String> {
    Ok(
        String::from_utf8(git(root, &["rev-parse", "--verify", "HEAD"]).await?)?
            .trim()
            .into(),
    )
}

fn paths(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
}

async fn residual(root: &Path) -> anyhow::Result<Vec<Vec<u8>>> {
    let untracked = git(root, &["ls-files", "--others", "--exclude-standard", "-z"]).await?;
    let mut state = vec![
        git(
            root,
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        )
        .await?,
        git(
            root,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--binary",
                "HEAD",
                "--",
            ],
        )
        .await?,
        git(
            root,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--binary",
                "--cached",
                "--",
            ],
        )
        .await?,
        untracked.clone(),
    ];
    for path in paths(&untracked) {
        // Reject unsupported names rather than lossily attributing ownership.
        let path = root.join(std::str::from_utf8(path)?);
        let metadata = tokio::fs::symlink_metadata(&path).await?;
        if !metadata.is_file() || metadata.is_symlink() {
            bail!(
                "unsupported pre-existing untracked path: {}",
                path.display()
            );
        }
        state.push(tokio::fs::read(path).await?);
    }
    Ok(state)
}

impl Snapshot {
    pub(crate) async fn capture(cwd: &Path) -> anyhow::Result<Self> {
        let root = git(cwd, &["rev-parse", "--show-toplevel"]).await?;
        let root = PathBuf::from(String::from_utf8(root)?.trim_end());
        // Inspect both layers: an unstaged reversal can hide an index change
        // from a combined HEAD-to-worktree diff.
        let staged = git(
            &root,
            &[
                "diff",
                "--no-renames",
                "--name-only",
                "-z",
                "--cached",
                "HEAD",
                "--",
            ],
        )
        .await?;
        let unstaged = git(&root, &["diff", "--no-renames", "--name-only", "-z", "--"]).await?;
        let untracked = git(&root, &["ls-files", "--others", "--exclude-standard", "-z"]).await?;
        Ok(Self {
            head: head(&root).await?,
            residual: residual(&root).await?,
            dirty_paths: paths(&staged)
                .chain(paths(&unstaged))
                .chain(paths(&untracked))
                .map(<[u8]>::to_vec)
                .collect(),
            root,
        })
    }

    pub(crate) fn guidance(&self, orb_id: &str) -> String {
        format!(
            "\n\nOrb completion contract for {orb_id}: one orb is one independently reviewable unit. \
             Inspect status before editing; do not touch or commit any pre-existing changed path. \
             Reject ambiguous overlap. Run relevant tests/checks before reporting success. \
             After validation passes, create exactly one focused non-empty commit (including \
             documentation-only changes), with the exact orb ID {orb_id} in its subject. \
             Stage only owned paths; preserve unrelated staged and unstaged changes. Never use \
             blanket staging or amend an earlier commit. Do not commit failed, cancelled, timed-out \
             or incomplete work. Hooks and signing must obey the selected Heddle policy; if blocked, \
             report failure without bypassing policy. Report VALIDATION: followed by checks and results, \
             and COMMIT: followed by the full SHA. Only if no change is needed, report NO_CHANGES: \
             followed by a concrete explanation and validation evidence instead of a commit. \
             Starting HEAD: {}. Pre-existing changed paths (off limits): {:?}. \
             Heddle discovers AGENTS.md from worker cwd and ancestors, not sibling launcher directories.",
            self.head,
            self.dirty_paths.iter().map(|path| String::from_utf8_lossy(path)).collect::<Vec<_>>()
        )
    }

    pub(crate) async fn verify(
        &self,
        orb_id: &str,
        successful: bool,
        response: &str,
    ) -> anyhow::Result<Option<CommitEvidence>> {
        let sha = head(&self.root).await?;
        if !successful {
            if sha != self.head {
                bail!(
                    "incomplete orb changed HEAD from {} to {sha}; manual review required",
                    self.head
                );
            }
            return Ok(None);
        }
        if !response.lines().any(|line| {
            line.strip_prefix("VALIDATION:")
                .is_some_and(|value| !value.trim().is_empty())
        }) {
            bail!("successful orb omitted validation evidence");
        }
        if residual(&self.root).await? != self.residual {
            bail!(
                "uncommitted orb changes or altered pre-existing changes; manual review required"
            );
        }
        if sha == self.head {
            if response.lines().any(|line| {
                line.strip_prefix("NO_CHANGES:")
                    .is_some_and(|value| !value.trim().is_empty())
            }) {
                return Ok(None);
            }
            bail!("successful orb has no completion commit or explicit no-op explanation");
        }
        let parents = git(&self.root, &["rev-list", "--parents", "-n", "1", &sha]).await?;
        let parents = String::from_utf8(parents)?;
        if parents.split_whitespace().collect::<Vec<_>>() != vec![sha.as_str(), self.head.as_str()]
        {
            bail!("expected exactly one non-merge completion commit on starting HEAD");
        }
        let changed = git(
            &self.root,
            &[
                "diff",
                "--no-renames",
                "--name-only",
                "-z",
                &self.head,
                &sha,
                "--",
            ],
        )
        .await?;
        if changed.is_empty() || paths(&changed).any(|path| self.dirty_paths.contains(path)) {
            bail!("empty completion commit or ambiguous overlap with pre-existing changes");
        }
        let subject =
            String::from_utf8(git(&self.root, &["show", "-s", "--format=%s", &sha]).await?)?
                .trim()
                .to_owned();
        if !subject
            .split(|c: char| !c.is_alphanumeric() && c != '-')
            .any(|word| word == orb_id)
        {
            bail!("completion commit subject must include orb ID {orb_id}");
        }
        if !response.lines().any(|line| {
            line.strip_prefix("COMMIT:")
                .is_some_and(|value| value.trim() == sha)
        }) {
            bail!("worker did not report the verified completion SHA {sha}");
        }
        Ok(Some(CommitEvidence { sha, subject }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn repository() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init"]).await.unwrap();
        git(dir.path(), &["config", "user.name", "Orb Test"])
            .await
            .unwrap();
        git(dir.path(), &["config", "user.email", "orb@example.invalid"])
            .await
            .unwrap();
        git(dir.path(), &["config", "commit.gpgsign", "false"])
            .await
            .unwrap();
        git(dir.path(), &["config", "core.hooksPath", "/dev/null"])
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("owned"), "before")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("unrelated"), "before")
            .await
            .unwrap();
        git(dir.path(), &["add", "."]).await.unwrap();
        git(dir.path(), &["commit", "-m", "initial"]).await.unwrap();
        dir
    }

    async fn commit_owned(path: &Path, subject: &str) -> String {
        tokio::fs::write(path.join("owned"), "after").await.unwrap();
        git(path, &["add", "owned"]).await.unwrap();
        git(path, &["commit", "-m", subject]).await.unwrap();
        head(path).await.unwrap()
    }

    #[tokio::test]
    async fn staged_change_hidden_by_unstaged_reversal_remains_off_limits() {
        let dir = repository().await;
        tokio::fs::write(dir.path().join("unrelated"), "staged user change")
            .await
            .unwrap();
        git(dir.path(), &["add", "unrelated"]).await.unwrap();
        tokio::fs::write(dir.path().join("unrelated"), "before")
            .await
            .unwrap();

        let before = Snapshot::capture(dir.path()).await.unwrap();
        assert!(before.dirty_paths.contains(b"unrelated".as_slice()));

        let sha = commit_owned(dir.path(), "orb-111: accidentally include staged change").await;
        let report = format!("VALIDATION: passed\nCOMMIT: {sha}");
        assert!(before.verify("orb-111", true, &report).await.is_err());
    }

    #[tokio::test]
    async fn successful_commit_preserves_preexisting_staged_and_untracked_changes() {
        let dir = repository().await;
        tokio::fs::write(dir.path().join("unrelated"), "user change")
            .await
            .unwrap();
        git(dir.path(), &["add", "unrelated"]).await.unwrap();
        tokio::fs::write(dir.path().join("notes"), "user notes")
            .await
            .unwrap();
        let before = Snapshot::capture(dir.path()).await.unwrap();
        tokio::fs::write(dir.path().join("owned"), "after")
            .await
            .unwrap();
        git(
            dir.path(),
            &[
                "commit",
                "--only",
                "-m",
                "orb-111: implement change",
                "--",
                "owned",
            ],
        )
        .await
        .unwrap();
        let sha = head(dir.path()).await.unwrap();
        let report = format!("VALIDATION: relevant checks passed\nCOMMIT: {sha}");
        let evidence = before
            .verify("orb-111", true, &report)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evidence.sha, sha);
        assert_eq!(evidence.subject, "orb-111: implement change");
    }

    #[tokio::test]
    async fn missing_commit_and_unexplained_clean_worktree_fail() {
        let dir = repository().await;
        let before = Snapshot::capture(dir.path()).await.unwrap();
        assert!(before.verify("orb-111", true, "done").await.is_err());
        tokio::fs::write(dir.path().join("owned"), "after")
            .await
            .unwrap();
        assert!(before
            .verify(
                "orb-111",
                true,
                "VALIDATION: passed\nNO_CHANGES: already done"
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn wrong_subject_and_failed_completion_are_rejected() {
        let dir = repository().await;
        let before = Snapshot::capture(dir.path()).await.unwrap();
        let sha = commit_owned(dir.path(), "unattributed change").await;
        assert!(before
            .verify(
                "orb-111",
                true,
                &format!("VALIDATION: passed\nCOMMIT: {sha}")
            )
            .await
            .is_err());
        assert!(before
            .verify("orb-111", false, "timed out or cancelled")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn failed_attempt_preserves_partial_work_without_creating_a_commit() {
        let dir = repository().await;
        let before = Snapshot::capture(dir.path()).await.unwrap();
        tokio::fs::write(dir.path().join("owned"), "partial")
            .await
            .unwrap();
        assert!(before
            .verify("orb-111", false, "failed")
            .await
            .unwrap()
            .is_none());
        assert_eq!(head(dir.path()).await.unwrap(), before.head);
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("owned"))
                .await
                .unwrap(),
            "partial"
        );
    }

    #[tokio::test]
    async fn sweeping_preexisting_changes_into_commit_is_rejected() {
        let dir = repository().await;
        tokio::fs::write(dir.path().join("unrelated"), "user change")
            .await
            .unwrap();
        let before = Snapshot::capture(dir.path()).await.unwrap();
        git(dir.path(), &["add", "."]).await.unwrap();
        let sha = commit_owned(dir.path(), "orb-111: change").await;
        assert!(before
            .verify(
                "orb-111",
                true,
                &format!("VALIDATION: passed\nCOMMIT: {sha}")
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn explicit_validated_noop_needs_no_commit() {
        let dir = repository().await;
        let before = Snapshot::capture(dir.path()).await.unwrap();
        assert!(before
            .verify(
                "orb-111",
                true,
                "VALIDATION: inspected existing behavior\nNO_CHANGES: already implemented"
            )
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn extra_commits_and_missing_reported_sha_are_rejected() {
        let dir = repository().await;
        let before = Snapshot::capture(dir.path()).await.unwrap();
        commit_owned(dir.path(), "orb-111: first change").await;
        assert!(before
            .verify("orb-111", true, "VALIDATION: passed")
            .await
            .is_err());
        tokio::fs::write(dir.path().join("owned"), "second change")
            .await
            .unwrap();
        git(dir.path(), &["commit", "-am", "orb-111: second change"])
            .await
            .unwrap();
        let sha = head(dir.path()).await.unwrap();
        assert!(before
            .verify(
                "orb-111",
                true,
                &format!("VALIDATION: passed\nCOMMIT: {sha}")
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn main_worktree_guidance_keeps_commit_and_context_policy_explicit() {
        let dir = repository().await;
        let before = Snapshot::capture(dir.path()).await.unwrap();
        let guidance = before.guidance("orb-111");
        assert!(guidance.contains("orb-111"));
        assert!(guidance.contains("documentation-only"));
        assert!(guidance.contains("Hooks and signing must obey"));
        assert!(guidance.contains("worker cwd and ancestors"));
        assert!(guidance.contains("not sibling launcher directories"));
    }
}

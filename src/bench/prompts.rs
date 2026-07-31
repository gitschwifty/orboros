//! Readable, benchmark-local prompt sets.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::{PromptConfig, PromptOverride};
use crate::prompt::prompt_hash;

const ROLES: &[&str] = &[
    "speccing",
    "decompose",
    "refining",
    "reevaluating",
    "execute",
];

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PromptFile {
    pub role: String,
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone)]
pub struct BenchPromptSet {
    pub name: String,
    pub root: PathBuf,
    files: BTreeMap<String, (PathBuf, String)>,
}

impl BenchPromptSet {
    /// Loads `bench_root/prompts/<name>`. Missing role files are intentional
    /// and fall back to built-in prompts.
    pub fn load(bench_root: &Path, name: &str) -> anyhow::Result<Self> {
        if name.is_empty()
            || name == "."
            || name == ".."
            || name.contains('/')
            || name.contains('\\')
        {
            anyhow::bail!("invalid prompt set name `{name}`");
        }
        let root = bench_root.join("prompts").join(name);
        if !root.is_dir() {
            anyhow::bail!("prompt set `{name}` not found at {}", root.display());
        }
        let mut files = BTreeMap::new();
        for role in ROLES {
            let path = root.join(format!("{role}.md"));
            if path.exists() {
                let content = fs::read_to_string(&path)?;
                if content.trim().is_empty() {
                    anyhow::bail!("prompt file {} is empty", path.display());
                }
                files.insert((*role).to_string(), (path, content));
            }
        }
        if files.is_empty() {
            anyhow::bail!("prompt set `{name}` contains no supported .md prompt files");
        }
        Ok(Self {
            name: name.to_string(),
            root,
            files,
        })
    }

    #[must_use]
    pub fn prompt_config(&self) -> PromptConfig {
        let mut config = PromptConfig::default();
        for (role, (_, content)) in &self.files {
            let prompt = PromptOverride {
                system: Some(content.clone()),
                system_file: None,
            };
            match role.as_str() {
                "execute" => {
                    config.workers.insert("execute".into(), prompt);
                }
                "decompose" => {
                    config.phases.insert("decomposing".into(), prompt);
                }
                "speccing" | "refining" | "reevaluating" => {
                    config.phases.insert(role.clone(), prompt);
                }
                _ => unreachable!("supported role list is exhaustive"),
            }
        }
        config
    }

    #[must_use]
    pub fn manifest(&self) -> Vec<PromptFile> {
        self.files
            .iter()
            .map(|(role, (path, content))| PromptFile {
                role: role.clone(),
                path: path
                    .strip_prefix(self.root.parent().unwrap_or(&self.root))
                    .unwrap_or(path)
                    .display()
                    .to_string(),
                sha256: prompt_hash(content),
            })
            .collect()
    }

    /// Copies selected prompt files and writes their manifest into a run.
    pub fn copy_to_run(&self, run_dir: &Path) -> anyhow::Result<()> {
        let destination = run_dir.join("prompts").join(&self.name);
        fs::create_dir_all(&destination)?;
        for (role, (_, content)) in &self.files {
            fs::write(destination.join(format!("{role}.md")), content)?;
        }
        fs::write(
            destination.join("manifest.json"),
            serde_json::to_vec_pretty(&self.manifest())?,
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_partial_set_and_builds_config() {
        let dir = tempfile::tempdir().unwrap();
        let set = dir.path().join("prompts/scoped-v1");
        fs::create_dir_all(&set).unwrap();
        fs::write(set.join("decompose.md"), "custom decompose").unwrap();
        let prompts = BenchPromptSet::load(dir.path(), "scoped-v1").unwrap();
        assert_eq!(prompts.manifest().len(), 1);
        assert!(prompts.prompt_config().phases.contains_key("decomposing"));
        assert!(prompts.prompt_config().workers.is_empty());
    }

    #[test]
    fn rejects_missing_set() {
        let dir = tempfile::tempdir().unwrap();
        assert!(BenchPromptSet::load(dir.path(), "missing").is_err());
    }

    #[test]
    fn copies_files_and_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let set = dir.path().join("prompts/x");
        fs::create_dir_all(&set).unwrap();
        fs::write(set.join("execute.md"), "custom execute").unwrap();
        let prompts = BenchPromptSet::load(dir.path(), "x").unwrap();
        let run = dir.path().join("run");
        prompts.copy_to_run(&run).unwrap();
        assert_eq!(
            fs::read_to_string(run.join("prompts/x/execute.md")).unwrap(),
            "custom execute"
        );
        assert!(run.join("prompts/x/manifest.json").exists());
    }
}

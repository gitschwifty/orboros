//! Readable, benchmark-local prompt sets.
//!
//! A set may either provide a complete top-level `<role>.md` prompt or define
//! ordered fragments in `composition.toml`. Complete files are intentionally
//! retained as an escape hatch for experiments that should not inherit a
//! composition layout.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{PromptConfig, PromptOverride};
use crate::prompt::prompt_hash;

const ROLES: &[&str] = &[
    "speccing",
    "decompose",
    "refining",
    "reevaluating",
    "execute",
];
const COMPOSITION_FILE: &str = "composition.toml";

#[derive(Debug, Clone, Deserialize)]
struct CompositionFile {
    roles: BTreeMap<String, RoleComposition>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RoleComposition {
    fragments: Vec<String>,
}

/// A source input used to construct an effective role prompt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptInputFile {
    pub path: String,
    pub sha256: String,
}

/// Provenance for one effective role prompt in a benchmark set.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptRoleManifest {
    pub role: String,
    /// `full_file` is the escape hatch; `composed` is assembled from ordered
    /// fragments named below.
    pub assembly: String,
    pub assembled_sha256: String,
    pub fragments: Vec<PromptInputFile>,
}

/// Complete provenance for the prompt inputs selected by a benchmark run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptManifest {
    pub prompt_set: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composition: Option<PromptInputFile>,
    pub roles: Vec<PromptRoleManifest>,
}

#[derive(Debug, Clone)]
struct ResolvedRole {
    content: String,
    manifest: PromptRoleManifest,
}

#[derive(Debug, Clone)]
pub struct BenchPromptSet {
    pub name: String,
    pub root: PathBuf,
    roles: BTreeMap<String, ResolvedRole>,
    /// Relative path -> original contents. This is copied verbatim into the
    /// run artifact so fragment provenance remains inspectable.
    source_files: BTreeMap<PathBuf, String>,
    composition: Option<PromptInputFile>,
}

impl BenchPromptSet {
    /// Loads `bench_root/prompts/<name>`. Missing roles intentionally fall
    /// back to built-in prompts. A root `<role>.md` takes precedence over a
    /// composition entry for the same role.
    pub fn load(bench_root: &Path, name: &str) -> anyhow::Result<Self> {
        validate_set_name(name)?;
        let root = bench_root.join("prompts").join(name);
        if !root.is_dir() {
            anyhow::bail!("prompt set `{name}` not found at {}", root.display());
        }

        let mut source_files = BTreeMap::new();
        let composition_path = root.join(COMPOSITION_FILE);
        let composition = if composition_path.exists() {
            let content = read_nonempty(&composition_path)?;
            let parsed: CompositionFile = toml::from_str(&content).map_err(|error| {
                anyhow::anyhow!(
                    "invalid prompt composition {}: {error}",
                    composition_path.display()
                )
            })?;
            for role in parsed.roles.keys() {
                validate_role(role)?;
            }
            source_files.insert(PathBuf::from(COMPOSITION_FILE), content.clone());
            Some((
                parsed,
                PromptInputFile {
                    path: COMPOSITION_FILE.into(),
                    sha256: prompt_hash(&content),
                },
            ))
        } else {
            None
        };

        let mut roles = BTreeMap::new();
        for role in ROLES {
            let full_path = root.join(format!("{role}.md"));
            if full_path.exists() {
                let content = read_nonempty(&full_path)?;
                let relative = PathBuf::from(format!("{role}.md"));
                source_files.insert(relative.clone(), content.clone());
                roles.insert(
                    (*role).to_string(),
                    ResolvedRole {
                        manifest: PromptRoleManifest {
                            role: (*role).to_string(),
                            assembly: "full_file".into(),
                            assembled_sha256: prompt_hash(&content),
                            fragments: vec![input_file(&relative, &content)],
                        },
                        content,
                    },
                );
                continue;
            }

            let Some((composition, _)) = composition.as_ref() else {
                continue;
            };
            let Some(definition) = composition.roles.get(*role) else {
                continue;
            };
            if definition.fragments.is_empty() {
                anyhow::bail!("composition for role `{role}` has no fragments");
            }

            let mut contents = Vec::with_capacity(definition.fragments.len());
            let mut fragments = Vec::with_capacity(definition.fragments.len());
            let mut seen = BTreeSet::new();
            for fragment in &definition.fragments {
                let relative = validated_relative_markdown_path(fragment)?;
                if !seen.insert(relative.clone()) {
                    anyhow::bail!(
                        "composition for role `{role}` repeats fragment {}",
                        relative.display()
                    );
                }
                let fragment_path = root.join(&relative);
                let content = read_nonempty(&fragment_path).map_err(|error| {
                    anyhow::anyhow!(
                        "failed to load fragment {} for role `{role}`: {error}",
                        fragment_path.display()
                    )
                })?;
                source_files.insert(relative.clone(), content.clone());
                fragments.push(input_file(&relative, &content));
                contents.push(content);
            }
            let content = contents.join("\n\n");
            roles.insert(
                (*role).to_string(),
                ResolvedRole {
                    manifest: PromptRoleManifest {
                        role: (*role).to_string(),
                        assembly: "composed".into(),
                        assembled_sha256: prompt_hash(&content),
                        fragments,
                    },
                    content,
                },
            );
        }
        if roles.is_empty() {
            anyhow::bail!("prompt set `{name}` contains no supported role files or compositions");
        }
        Ok(Self {
            name: name.to_string(),
            root,
            roles,
            source_files,
            composition: composition.map(|(_, manifest)| manifest),
        })
    }

    #[must_use]
    pub fn prompt_config(&self) -> PromptConfig {
        let mut config = PromptConfig::default();
        for (role, resolved) in &self.roles {
            let prompt = PromptOverride {
                system: Some(resolved.content.clone()),
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
    pub fn manifest(&self) -> PromptManifest {
        PromptManifest {
            prompt_set: self.name.clone(),
            composition: self.composition.clone(),
            roles: self
                .roles
                .values()
                .map(|resolved| resolved.manifest.clone())
                .collect(),
        }
    }

    /// Copies every selected input file and writes resolved provenance into a
    /// run artifact. The manifest names the fragment order and final hash.
    pub fn copy_to_run(&self, run_dir: &Path) -> anyhow::Result<()> {
        let destination = run_dir.join("prompts").join(&self.name);
        for (relative, content) in &self.source_files {
            let output = destination.join(relative);
            let parent = output.parent().expect("relative file has a parent");
            fs::create_dir_all(parent)?;
            fs::write(output, content)?;
        }
        fs::write(
            destination.join("manifest.json"),
            serde_json::to_vec_pretty(&self.manifest())?,
        )?;
        Ok(())
    }
}

fn validate_set_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        anyhow::bail!("invalid prompt set name `{name}`");
    }
    Ok(())
}

fn validate_role(role: &str) -> anyhow::Result<()> {
    if !ROLES.contains(&role) {
        anyhow::bail!("unsupported composed prompt role `{role}`");
    }
    Ok(())
}

fn validated_relative_markdown_path(value: &str) -> anyhow::Result<PathBuf> {
    let path = Path::new(value);
    if path.extension().and_then(|extension| extension.to_str()) != Some("md")
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        anyhow::bail!("invalid prompt fragment path `{value}`");
    }
    Ok(path.to_path_buf())
}

fn read_nonempty(path: &Path) -> anyhow::Result<String> {
    let content = fs::read_to_string(path)?;
    if content.trim().is_empty() {
        anyhow::bail!("prompt file {} is empty", path.display());
    }
    Ok(content)
}

fn input_file(path: &Path, content: &str) -> PromptInputFile {
    PromptInputFile {
        path: path.display().to_string(),
        sha256: prompt_hash(content),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_partial_full_file_set_and_builds_config() {
        let dir = tempfile::tempdir().unwrap();
        let set = dir.path().join("prompts/scoped-v1");
        fs::create_dir_all(&set).unwrap();
        fs::write(set.join("decompose.md"), "custom decompose").unwrap();
        let prompts = BenchPromptSet::load(dir.path(), "scoped-v1").unwrap();
        assert_eq!(prompts.manifest().roles.len(), 1);
        assert_eq!(prompts.manifest().roles[0].assembly, "full_file");
        assert!(prompts.prompt_config().phases.contains_key("decomposing"));
        assert!(prompts.prompt_config().workers.is_empty());
    }

    #[test]
    fn composes_ordered_fragments_and_records_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let set = dir.path().join("prompts/composable");
        fs::create_dir_all(set.join("base")).unwrap();
        fs::create_dir_all(set.join("roles")).unwrap();
        fs::write(
            set.join(COMPOSITION_FILE),
            "[roles.decompose]\nfragments = [\"base/worker.md\", \"roles/decompose.md\"]\n",
        )
        .unwrap();
        fs::write(set.join("base/worker.md"), "base").unwrap();
        fs::write(set.join("roles/decompose.md"), "role").unwrap();

        let prompts = BenchPromptSet::load(dir.path(), "composable").unwrap();
        assert_eq!(
            prompts.prompt_config().phases["decomposing"]
                .system
                .as_deref(),
            Some("base\n\nrole")
        );
        let manifest = prompts.manifest();
        assert_eq!(manifest.composition.unwrap().path, COMPOSITION_FILE);
        assert_eq!(manifest.roles[0].assembly, "composed");
        assert_eq!(manifest.roles[0].fragments.len(), 2);
        assert_eq!(
            manifest.roles[0].assembled_sha256,
            prompt_hash("base\n\nrole")
        );
    }

    #[test]
    fn full_file_overrides_composition_for_same_role() {
        let dir = tempfile::tempdir().unwrap();
        let set = dir.path().join("prompts/x");
        fs::create_dir_all(set.join("base")).unwrap();
        fs::write(
            set.join(COMPOSITION_FILE),
            "[roles.decompose]\nfragments = [\"base/worker.md\"]\n",
        )
        .unwrap();
        fs::write(set.join("base/worker.md"), "composed").unwrap();
        fs::write(set.join("decompose.md"), "escape hatch").unwrap();
        let prompts = BenchPromptSet::load(dir.path(), "x").unwrap();
        assert_eq!(
            prompts.prompt_config().phases["decomposing"]
                .system
                .as_deref(),
            Some("escape hatch")
        );
        assert_eq!(prompts.manifest().roles[0].assembly, "full_file");
    }

    #[test]
    fn rejects_missing_set_and_invalid_compositions() {
        let dir = tempfile::tempdir().unwrap();
        assert!(BenchPromptSet::load(dir.path(), "missing").is_err());

        let set = dir.path().join("prompts/x");
        fs::create_dir_all(&set).unwrap();
        fs::write(
            set.join(COMPOSITION_FILE),
            "[roles.decompose]\nfragments = [\"../outside.md\"]\n",
        )
        .unwrap();
        assert!(BenchPromptSet::load(dir.path(), "x").is_err());

        fs::write(
            set.join(COMPOSITION_FILE),
            "[roles.decompose]\nfragments = [\"base/worker.md\", \"base/worker.md\"]\n",
        )
        .unwrap();
        fs::create_dir_all(set.join("base")).unwrap();
        fs::write(set.join("base/worker.md"), "base").unwrap();
        let error = BenchPromptSet::load(dir.path(), "x").unwrap_err();
        assert!(error.to_string().contains("repeats fragment"));
    }

    #[test]
    fn copies_inputs_and_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let set = dir.path().join("prompts/x");
        fs::create_dir_all(set.join("roles")).unwrap();
        fs::write(
            set.join(COMPOSITION_FILE),
            "[roles.execute]\nfragments = [\"roles/execute.md\"]\n",
        )
        .unwrap();
        fs::write(set.join("roles/execute.md"), "custom execute").unwrap();
        let prompts = BenchPromptSet::load(dir.path(), "x").unwrap();
        let run = dir.path().join("run");
        prompts.copy_to_run(&run).unwrap();
        assert_eq!(
            fs::read_to_string(run.join("prompts/x/roles/execute.md")).unwrap(),
            "custom execute"
        );
        assert!(run.join("prompts/x/composition.toml").exists());
        assert!(run.join("prompts/x/manifest.json").exists());
    }

    #[test]
    fn manifest_round_trips_through_json() {
        let manifest = PromptManifest {
            prompt_set: "x".into(),
            composition: Some(PromptInputFile {
                path: COMPOSITION_FILE.into(),
                sha256: "definition".into(),
            }),
            roles: vec![PromptRoleManifest {
                role: "decompose".into(),
                assembly: "composed".into(),
                assembled_sha256: "assembled".into(),
                fragments: vec![PromptInputFile {
                    path: "base/worker.md".into(),
                    sha256: "fragment".into(),
                }],
            }],
        };
        let serialized = serde_json::to_string(&manifest).unwrap();
        assert_eq!(
            serde_json::from_str::<PromptManifest>(&serialized).unwrap(),
            manifest
        );
    }
}

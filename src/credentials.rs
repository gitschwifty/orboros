//! User-local provider credential resolution.
//!
//! Credential values never enter configuration, logs, telemetry, or durable
//! state. Resolved values are placed in the current process environment only
//! because Heddle's provider clients consume the standard provider variables.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const OPENROUTER_API_KEY: &str = "OPENROUTER_API_KEY";
const ANTHROPIC_API_KEY: &str = "ANTHROPIC_API_KEY";
const OPENAI_API_KEY: &str = "OPENAI_API_KEY";
const KNOWN_CREDENTIALS: [&str; 3] = [OPENROUTER_API_KEY, ANTHROPIC_API_KEY, OPENAI_API_KEY];
const MACOS_OPENROUTER_SERVICE: &str = "orboros.openrouter";

/// Non-secret source used for a credential loaded during this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    ProcessEnvironment,
    MacosKeychain,
    CredentialsFile,
}

impl CredentialSource {
    const fn label(self) -> &'static str {
        match self {
            Self::ProcessEnvironment => "process environment",
            Self::MacosKeychain => "macOS Keychain",
            Self::CredentialsFile => "user-local credentials file",
        }
    }
}

/// A safe record of which provider variables became available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialLoad {
    pub variable: &'static str,
    pub source: CredentialSource,
}

/// Resolves credentials for a worker-spawning command.
///
/// Explicit process environment variables always win, preserving CI and
/// launch-wrapper compatibility. On macOS, `OpenRouter` then falls back to the
/// `orboros.openrouter` Keychain service for the current account. Finally, an
/// explicitly user-local `~/.orboros/credentials.env` file may supply any
/// missing known provider variables when it has owner-only permissions.
///
/// # Errors
///
/// Returns an error only when the credentials file exists but is unsafe or
/// malformed. Missing sources are normal; provider-specific startup validation
/// produces the eventual actionable missing-credential error.
pub fn load_process_credentials() -> anyhow::Result<Vec<CredentialLoad>> {
    let mut loaded = present_process_credentials();

    if !loaded
        .iter()
        .any(|entry| entry.variable == OPENROUTER_API_KEY)
    {
        if let Some(value) = load_macos_openrouter_keychain()? {
            // Safe here: this binary is single-threaded before it starts its
            // runtime or worker threads, and Heddle expects this variable.
            std::env::set_var(OPENROUTER_API_KEY, value);
            loaded.push(CredentialLoad {
                variable: OPENROUTER_API_KEY,
                source: CredentialSource::MacosKeychain,
            });
        }
    }

    let missing: Vec<_> = KNOWN_CREDENTIALS
        .iter()
        .copied()
        .filter(|variable| !loaded.iter().any(|entry| entry.variable == *variable))
        .collect();
    if missing.is_empty() {
        return Ok(loaded);
    }

    let path = default_credentials_file()?;
    if !path.exists() {
        return Ok(loaded);
    }
    ensure_private_credentials_file(&path)?;
    let values = parse_credentials_file(&path)?;
    for variable in missing {
        if let Some(value) = values
            .get(variable)
            .filter(|value| !value.trim().is_empty())
        {
            std::env::set_var(variable, value);
            loaded.push(CredentialLoad {
                variable,
                source: CredentialSource::CredentialsFile,
            });
        }
    }
    Ok(loaded)
}

/// Logs only source labels and variable names, never credential values.
pub fn log_credential_sources(entries: &[CredentialLoad]) {
    for entry in entries {
        tracing::debug!(
            variable = entry.variable,
            source = entry.source.label(),
            "credential source resolved"
        );
    }
}

fn present_process_credentials() -> Vec<CredentialLoad> {
    KNOWN_CREDENTIALS
        .iter()
        .copied()
        .filter(|variable| std::env::var(variable).is_ok_and(|value| !value.trim().is_empty()))
        .map(|variable| CredentialLoad {
            variable,
            source: CredentialSource::ProcessEnvironment,
        })
        .collect()
}

fn default_credentials_file() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("could not determine home directory for credentials"))?;
    Ok(home.join(".orboros").join("credentials.env"))
}

fn parse_credentials_file(path: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    let content = std::fs::read_to_string(path).map_err(|error| {
        anyhow::anyhow!(
            "could not read user-local credentials file {}: {error}",
            path.display()
        )
    })?;
    let mut values = BTreeMap::new();
    for (line_number, raw_line) in content.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, value) = line.split_once('=').ok_or_else(|| {
            anyhow::anyhow!(
                "invalid credentials file entry on line {}: expected NAME=VALUE",
                line_number + 1
            )
        })?;
        let name = name.trim();
        if !KNOWN_CREDENTIALS.contains(&name) {
            continue;
        }
        values.insert(
            name.to_owned(),
            trim_optional_quotes(value.trim()).to_owned(),
        );
    }
    Ok(values)
}

fn trim_optional_quotes(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value)
}

#[cfg(unix)]
fn ensure_private_credentials_file(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        !metadata.file_type().is_symlink(),
        "refusing symlinked credentials file {}; use a regular owner-only file",
        path.display()
    );
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "credentials path {} is not a regular file",
        path.display()
    );
    anyhow::ensure!(
        metadata.mode().trailing_zeros() >= 6,
        "credentials file {} has insecure permissions; run chmod 600 {}",
        path.display(),
        path.display()
    );
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_credentials_file(path: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(
        path.is_file(),
        "credentials path {} is not a regular file",
        path.display()
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn load_macos_openrouter_keychain() -> anyhow::Result<Option<String>> {
    let account = std::env::var("USER").unwrap_or_else(|_| "openrouter".into());
    let output = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            MACOS_OPENROUTER_SERVICE,
            "-a",
            &account,
            "-w",
        ])
        .output()
        .map_err(|error| anyhow::anyhow!("could not query macOS Keychain: {error}"))?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8(output.stdout).map_err(|_| {
        anyhow::anyhow!("macOS Keychain returned a non-UTF-8 OpenRouter credential")
    })?;
    Ok((!value.trim().is_empty()).then(|| value.trim_end().to_owned()))
}

#[cfg(not(target_os = "macos"))]
fn load_macos_openrouter_keychain() -> anyhow::Result<Option<String>> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_accepts_known_credentials_and_ignores_other_values() {
        let path = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            path.path(),
            "# comment\nOPENROUTER_API_KEY=key\nOTHER=value\nOPENAI_API_KEY='other'\n",
        )
        .unwrap();

        let values = parse_credentials_file(path.path()).unwrap();

        assert_eq!(values.get(OPENROUTER_API_KEY), Some(&"key".to_string()));
        assert_eq!(values.get(OPENAI_API_KEY), Some(&"other".to_string()));
        assert_eq!(values.len(), 2);
    }

    #[test]
    fn parser_rejects_malformed_entries_without_echoing_values() {
        let path = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(path.path(), "OPENROUTER_API_KEY=key\nnot-an-entry\n").unwrap();

        let error = parse_credentials_file(path.path()).unwrap_err();

        assert!(format!("{error}").contains("line 2"));
        assert!(!format!("{error}").contains("key"));
    }

    #[cfg(unix)]
    #[test]
    fn private_file_check_rejects_group_readable_file() {
        use std::os::unix::fs::PermissionsExt;

        let path = tempfile::NamedTempFile::new().unwrap();
        std::fs::set_permissions(path.path(), std::fs::Permissions::from_mode(0o640)).unwrap();

        let error = ensure_private_credentials_file(path.path()).unwrap_err();

        assert!(format!("{error}").contains("chmod 600"));
    }
}

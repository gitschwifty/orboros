//! Fail-fast validation for commands that spawn workers.
//!
//! Called at the top of every CLI command that will eventually invoke a
//! heddle worker. Surfaces missing binaries, malformed model strings, and
//! absent provider credentials with clear errors before any subprocess is
//! spawned or any LLM call is attempted.

use std::path::{Path, PathBuf};

/// Inputs to a single prereq check.
#[derive(Debug, Clone)]
pub struct PrereqCheck<'a> {
    pub worker_binary: &'a str,
    pub model: &'a str,
    /// When false, skip credential checks. Useful for local models
    /// (ollama, llama.cpp) or in tests.
    pub require_credentials: bool,
}

/// Outcome of credential inspection for a known provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderCheck {
    Known { env_var: &'static str },
    NoCredentialsRequired,
    Unknown,
}

/// Runs all configured checks. Returns an `anyhow::Error` with a clear
/// message for the user on the first failure.
///
/// # Errors
///
/// Returns an error if the worker binary is missing or non-executable,
/// the model string is malformed, or the required provider credentials
/// are unset.
pub fn validate_worker_prereqs(check: &PrereqCheck<'_>) -> anyhow::Result<()> {
    check_binary(check.worker_binary)?;
    check_model_string(check.model)?;
    if check.require_credentials {
        check_credentials_for_model(check.model)?;
    }
    Ok(())
}

/// Confirms `path` exists and is an executable regular file. `~` is
/// expanded against the user's home directory.
///
/// # Errors
///
/// Returns an error if the binary cannot be located, is not a file, or
/// lacks execute permission.
pub fn check_binary(raw_path: &str) -> anyhow::Result<()> {
    let path = expand_tilde(raw_path);
    if !path.exists() {
        anyhow::bail!(
            "worker binary not found: {} (set --worker-binary or HEDDLE_BINARY)",
            path.display()
        );
    }
    let metadata = std::fs::metadata(&path)
        .map_err(|e| anyhow::anyhow!("could not stat worker binary {}: {e}", path.display()))?;
    if !metadata.is_file() {
        anyhow::bail!(
            "worker binary path is not a regular file: {}",
            path.display()
        );
    }
    if !is_executable(&path, &metadata) {
        anyhow::bail!(
            "worker binary is not executable: {} (chmod +x?)",
            path.display()
        );
    }
    Ok(())
}

/// Validates that `model` looks like `provider/model-id` — a single `/`
/// with non-empty halves. Emits a `tracing::warn!` when the provider
/// prefix is not in the known list; provider lists drift so we don't
/// hard-fail on unknown ones.
///
/// # Errors
///
/// Returns an error only on shape failures (empty, no slash, slash at
/// either end).
pub fn check_model_string(model: &str) -> anyhow::Result<()> {
    if model.is_empty() {
        anyhow::bail!("model string is empty");
    }
    let (provider, model_id) = model
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("model string must be 'provider/model' (got: {model})"))?;
    if provider.is_empty() || model_id.is_empty() {
        anyhow::bail!("model string must be 'provider/model' with non-empty halves (got: {model})");
    }
    if provider.contains('/') || model_id.contains(' ') {
        anyhow::bail!("model string has suspicious characters (got: {model})");
    }
    if matches!(classify_provider(provider), ProviderCheck::Unknown) {
        tracing::warn!(
            provider,
            "unknown model provider; credential check will be skipped"
        );
    }
    Ok(())
}

/// Checks that the env var expected by the model's provider is set
/// (either from the process env or via dotenvy which is loaded at
/// startup).
///
/// # Errors
///
/// Returns an error if the provider is known and the corresponding
/// credential env var is unset. Unknown providers are skipped with a
/// warning surfaced from `check_model_string`. Runtime auth failures
/// from unknown providers still propagate normally via the IPC error
/// path — this skip is not a license to swallow downstream failures.
pub fn check_credentials_for_model(model: &str) -> anyhow::Result<()> {
    let provider = model.split('/').next().unwrap_or_default();
    match classify_provider(provider) {
        ProviderCheck::Known { env_var } => {
            if std::env::var(env_var)
                .map(|s| s.trim().is_empty())
                .unwrap_or(true)
            {
                anyhow::bail!(
                    "missing credentials for {provider}: set {env_var} \
                     (looked at .env and process env)"
                );
            }
            Ok(())
        }
        ProviderCheck::NoCredentialsRequired | ProviderCheck::Unknown => Ok(()),
    }
}

fn classify_provider(provider: &str) -> ProviderCheck {
    match provider {
        "openrouter" => ProviderCheck::Known {
            env_var: "OPENROUTER_API_KEY",
        },
        "anthropic" => ProviderCheck::Known {
            env_var: "ANTHROPIC_API_KEY",
        },
        "openai" => ProviderCheck::Known {
            env_var: "OPENAI_API_KEY",
        },
        "ollama" | "local" | "llamacpp" => ProviderCheck::NoCredentialsRequired,
        _ => ProviderCheck::Unknown,
    }
}

fn expand_tilde(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    if raw == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(raw)
}

#[cfg(unix)]
fn is_executable(_path: &Path, metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(path: &Path, _metadata: &std::fs::Metadata) -> bool {
    // On non-Unix, we can't easily check the executable bit. Trust the
    // existence check above.
    path.exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    /// Global lock serializing tests that mutate environment variables.
    /// Rust runs tests in parallel by default; without this lock, two
    /// tests racing on `set_var`/`remove_var` produce flaky failures.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Wrapper that resets known env vars around a closure so tests don't
    /// interfere with each other or with the developer's shell.
    fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
        // Lock for the entire scope so a concurrent test doesn't observe
        // our half-applied state. Recover from poisoning since panics in
        // other tests can poison the mutex.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior: Vec<(String, Option<String>)> = vars
            .iter()
            .map(|(k, _)| ((*k).to_string(), std::env::var(*k).ok()))
            .collect();
        for (k, v) in vars {
            match v {
                Some(value) => std::env::set_var(k, value),
                None => std::env::remove_var(k),
            }
        }
        f();
        for (k, v) in prior {
            match v {
                Some(value) => std::env::set_var(&k, value),
                None => std::env::remove_var(&k),
            }
        }
    }

    // ── check_binary ──────────────────────────────────────────────

    #[test]
    fn check_binary_missing_path_errors() {
        let err = check_binary("/definitely/does/not/exist/heddle").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not found"), "got: {msg}");
    }

    #[test]
    fn check_binary_non_executable_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-exec");
        fs::write(&path, "stub").unwrap();
        #[cfg(unix)]
        {
            let mut perms = fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o644);
            fs::set_permissions(&path, perms).unwrap();
        }
        #[cfg(unix)]
        {
            let err = check_binary(path.to_str().unwrap()).unwrap_err();
            assert!(format!("{err}").contains("not executable"));
        }
        #[cfg(not(unix))]
        {
            // On non-Unix we just confirm it doesn't panic.
            let _ = check_binary(path.to_str().unwrap());
        }
    }

    #[test]
    fn check_binary_directory_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = check_binary(dir.path().to_str().unwrap()).unwrap_err();
        assert!(format!("{err}").contains("not a regular file"));
    }

    #[cfg(unix)]
    #[test]
    fn check_binary_executable_file_ok() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("exec-me");
        fs::write(&path, "#!/bin/sh\necho hi\n").unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        check_binary(path.to_str().unwrap()).unwrap();
    }

    // ── check_model_string ────────────────────────────────────────

    #[test]
    fn check_model_string_happy_paths() {
        for ok in [
            "openrouter/free",
            "anthropic/claude-sonnet-4-5",
            "ollama/llama3:8b",
        ] {
            check_model_string(ok).unwrap();
        }
    }

    #[test]
    fn check_model_string_rejects_empty() {
        assert!(check_model_string("").is_err());
    }

    #[test]
    fn check_model_string_rejects_no_slash() {
        let err = check_model_string("openrouter").unwrap_err();
        assert!(format!("{err}").contains("provider/model"));
    }

    #[test]
    fn check_model_string_rejects_empty_provider() {
        assert!(check_model_string("/model").is_err());
    }

    #[test]
    fn check_model_string_rejects_empty_model_id() {
        assert!(check_model_string("provider/").is_err());
    }

    #[test]
    fn check_model_string_rejects_double_slash_in_provider_half() {
        // The first '/' splits; the provider half must not contain another '/'.
        // We allow slashes in the model id (e.g. for openrouter routes), but
        // provider half must be a single token.
        // Construct a malformed case: empty provider, model containing slash.
        assert!(check_model_string("/a/b").is_err());
    }

    #[test]
    fn check_model_string_unknown_provider_is_ok_with_warning() {
        // Unknown provider is OK at this step (warn only).
        check_model_string("mystery/super-model-v3").unwrap();
    }

    // ── check_credentials_for_model ───────────────────────────────

    #[test]
    fn credentials_openrouter_missing_errors() {
        with_env(&[("OPENROUTER_API_KEY", None)], || {
            let err = check_credentials_for_model("openrouter/free").unwrap_err();
            let msg = format!("{err}");
            assert!(msg.contains("OPENROUTER_API_KEY"), "got: {msg}");
        });
    }

    #[test]
    fn credentials_openrouter_present_ok() {
        with_env(&[("OPENROUTER_API_KEY", Some("sk-test"))], || {
            check_credentials_for_model("openrouter/free").unwrap();
        });
    }

    #[test]
    fn credentials_openrouter_empty_string_treated_as_missing() {
        with_env(&[("OPENROUTER_API_KEY", Some("   "))], || {
            assert!(check_credentials_for_model("openrouter/free").is_err());
        });
    }

    #[test]
    fn credentials_anthropic_missing_errors() {
        with_env(&[("ANTHROPIC_API_KEY", None)], || {
            let err = check_credentials_for_model("anthropic/claude-sonnet-4-5").unwrap_err();
            assert!(format!("{err}").contains("ANTHROPIC_API_KEY"));
        });
    }

    #[test]
    fn credentials_openai_missing_errors() {
        with_env(&[("OPENAI_API_KEY", None)], || {
            let err = check_credentials_for_model("openai/gpt-5").unwrap_err();
            assert!(format!("{err}").contains("OPENAI_API_KEY"));
        });
    }

    #[test]
    fn credentials_ollama_skipped_no_env_required() {
        with_env(&[("OPENROUTER_API_KEY", None)], || {
            check_credentials_for_model("ollama/llama3:8b").unwrap();
        });
    }

    #[test]
    fn credentials_unknown_provider_skipped() {
        // Unknown providers are skipped here (paired with the model-string
        // warning earlier). Downstream auth failures from unknown providers
        // still propagate normally via the worker's error path — this is
        // not a license to silently succeed.
        check_credentials_for_model("mystery/model-x").unwrap();
    }

    // ── validate_worker_prereqs (orchestration) ───────────────────

    #[cfg(unix)]
    #[test]
    fn validate_worker_prereqs_all_ok() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("heddle-fake");
        fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&bin, perms).unwrap();

        with_env(&[("OPENROUTER_API_KEY", Some("sk-test"))], || {
            validate_worker_prereqs(&PrereqCheck {
                worker_binary: bin.to_str().unwrap(),
                model: "openrouter/free",
                require_credentials: true,
            })
            .unwrap();
        });
    }

    #[cfg(unix)]
    #[test]
    fn validate_worker_prereqs_skip_credentials_does_not_check_env() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("heddle-fake");
        fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&bin, perms).unwrap();

        with_env(&[("OPENROUTER_API_KEY", None)], || {
            // require_credentials=false — should pass even with no env var.
            validate_worker_prereqs(&PrereqCheck {
                worker_binary: bin.to_str().unwrap(),
                model: "openrouter/free",
                require_credentials: false,
            })
            .unwrap();
        });
    }

    #[test]
    fn validate_worker_prereqs_missing_binary_errors_before_model_check() {
        // Should fail on binary, not on model.
        let err = validate_worker_prereqs(&PrereqCheck {
            worker_binary: "/no/such/binary",
            model: "garbage",
            require_credentials: true,
        })
        .unwrap_err();
        assert!(format!("{err}").contains("not found"));
    }
}

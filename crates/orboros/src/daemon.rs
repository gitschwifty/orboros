use std::path::PathBuf;

use anyhow::{Context, Result};

// ---------------------------------------------------------------------------
// DaemonConfig
// ---------------------------------------------------------------------------

/// Configuration for daemon mode.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Path to the PID file.
    pub pid_file: PathBuf,
    /// Optional path to the log file.
    pub log_file: Option<PathBuf>,
    /// Maximum log file size in bytes before rotation (default: 10 MB).
    pub log_max_size: u64,
    /// Tick interval in milliseconds (default: 1000).
    pub tick_interval_ms: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        let pid_file = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".orboros")
            .join("orboros.pid");
        Self {
            pid_file,
            log_file: None,
            log_max_size: 10 * 1024 * 1024, // 10 MB
            tick_interval_ms: 1000,
        }
    }
}

// ---------------------------------------------------------------------------
// PID file helpers
// ---------------------------------------------------------------------------

/// Writes the current process PID to the configured pid file.
///
/// Creates parent directories if they don't exist.
///
/// # Errors
/// Returns an error if the file cannot be written.
pub fn write_pid_file(config: &DaemonConfig) -> Result<()> {
    if let Some(parent) = config.pid_file.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating pid file directory: {}", parent.display()))?;
    }
    let pid = std::process::id();
    std::fs::write(&config.pid_file, pid.to_string())
        .with_context(|| format!("writing pid file: {}", config.pid_file.display()))?;
    Ok(())
}

/// Reads the PID from the configured pid file.
///
/// Returns `None` if the file does not exist.
///
/// # Errors
/// Returns an error if the file exists but cannot be read or parsed.
pub fn read_pid_file(config: &DaemonConfig) -> Result<Option<u32>> {
    if !config.pid_file.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&config.pid_file)
        .with_context(|| format!("reading pid file: {}", config.pid_file.display()))?;
    let pid: u32 = content
        .trim()
        .parse()
        .with_context(|| format!("parsing pid from file: {:?}", content.trim()))?;
    Ok(Some(pid))
}

/// Removes the pid file if it exists.
///
/// # Errors
/// Returns an error if the file exists but cannot be removed.
pub fn remove_pid_file(config: &DaemonConfig) -> Result<()> {
    if config.pid_file.exists() {
        std::fs::remove_file(&config.pid_file)
            .with_context(|| format!("removing pid file: {}", config.pid_file.display()))?;
    }
    Ok(())
}

/// Checks if a daemon process is currently running.
///
/// Returns `true` if the pid file exists and the process with that PID is alive
/// (verified via `kill(pid, 0)`).
pub fn is_running(config: &DaemonConfig) -> bool {
    let Ok(Some(pid)) = read_pid_file(config) else {
        return false;
    };
    // kill(pid, 0) checks if the process exists without sending a signal.
    // Returns 0 on success (process exists), -1 on error.
    unsafe { libc::kill(pid.cast_signed(), 0) == 0 }
}

// ---------------------------------------------------------------------------
// Signal handling
// ---------------------------------------------------------------------------

/// Sets up SIGTERM and SIGINT signal handlers.
///
/// Returns a `watch::Receiver<bool>` that becomes `true` when a shutdown
/// signal is received.
///
/// # Panics
/// Panics if signal handlers cannot be registered (platform not supported).
pub fn setup_signal_handlers() -> tokio::sync::watch::Receiver<bool> {
    let (tx, rx) = tokio::sync::watch::channel(false);

    tokio::spawn(async move {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM handler");
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("failed to register SIGINT handler");

        tokio::select! {
            _ = sigterm.recv() => {
                tracing::info!("received SIGTERM, shutting down");
            }
            _ = sigint.recv() => {
                tracing::info!("received SIGINT, shutting down");
            }
        }

        let _ = tx.send(true);
    });

    rx
}

// ---------------------------------------------------------------------------
// Log rotation
// ---------------------------------------------------------------------------

/// Rotates the log file if it exceeds `config.log_max_size`.
///
/// Renames the current log file to `<name>.1` and creates a new empty file.
/// If no log file is configured, this is a no-op.
///
/// # Errors
/// Returns an error if the file cannot be renamed or created.
pub fn rotate_log(config: &DaemonConfig) -> Result<()> {
    let Some(ref log_file) = config.log_file else {
        return Ok(());
    };

    if !log_file.exists() {
        return Ok(());
    }

    let metadata = std::fs::metadata(log_file)
        .with_context(|| format!("reading log file metadata: {}", log_file.display()))?;

    if metadata.len() <= config.log_max_size {
        return Ok(());
    }

    // Rotate: rename current to .1
    let rotated = log_file.with_extension("log.1");
    std::fs::rename(log_file, &rotated)
        .with_context(|| format!("rotating log file to {}", rotated.display()))?;

    // Create new empty log file
    std::fs::write(log_file, "")
        .with_context(|| format!("creating new log file: {}", log_file.display()))?;

    tracing::info!(
        "rotated log file {} -> {}",
        log_file.display(),
        rotated.display()
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Daemon runner
// ---------------------------------------------------------------------------

/// Runs the daemon: writes PID file, sets up signal handlers, runs the queue
/// loop, and cleans up on shutdown.
///
/// # Errors
/// Returns an error if PID file operations or the queue loop fail.
pub async fn run_daemon(config: DaemonConfig, queue: crate::queue_loop::QueueLoop) -> Result<()> {
    // Write PID file
    write_pid_file(&config).context("failed to write PID file")?;
    tracing::info!(
        pid = std::process::id(),
        pid_file = %config.pid_file.display(),
        "daemon started"
    );

    // Set up signal handlers
    let mut shutdown_rx = setup_signal_handlers();

    // Run the queue loop with periodic log rotation
    let tick_interval = std::time::Duration::from_millis(config.tick_interval_ms);
    let running = queue.running_flag();

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    tracing::info!("shutdown signal received, stopping daemon");
                    queue.stop();
                    break;
                }
            }
            () = tokio::time::sleep(tick_interval) => {
                // Rotate logs if needed
                if let Err(e) = rotate_log(&config) {
                    tracing::warn!("log rotation failed: {e}");
                }

                // Run a tick
                match queue.tick() {
                    Ok(result) => {
                        if !result.is_idle() {
                            tracing::debug!(
                                pipelines = result.pipelines_started,
                                executed = result.orbs_executed,
                                completed = result.roots_completed,
                                reevaluated = result.orbs_reevaluated,
                                "tick completed"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::error!("tick failed: {e}");
                    }
                }
            }
        }

        if !running.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
    }

    // Cleanup
    if let Err(e) = remove_pid_file(&config) {
        tracing::warn!("failed to remove PID file: {e}");
    }
    tracing::info!("daemon stopped");

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn config_in(dir: &std::path::Path) -> DaemonConfig {
        DaemonConfig {
            pid_file: dir.join("test.pid"),
            log_file: None,
            log_max_size: 10 * 1024 * 1024,
            tick_interval_ms: 1000,
        }
    }

    // ── DaemonConfig defaults ───────────────────────────────────────

    #[test]
    fn default_config_has_expected_values() {
        let config = DaemonConfig::default();
        assert!(config.pid_file.ends_with(".orboros/orboros.pid"));
        assert!(config.log_file.is_none());
        assert_eq!(config.log_max_size, 10 * 1024 * 1024);
        assert_eq!(config.tick_interval_ms, 1000);
    }

    // ── PID file write/read/remove ──────────────────────────────────

    #[test]
    fn write_and_read_pid_file() {
        let tmp = tempdir().unwrap();
        let config = config_in(tmp.path());

        write_pid_file(&config).unwrap();

        let pid = read_pid_file(&config).unwrap();
        assert_eq!(pid, Some(std::process::id()));
    }

    #[test]
    fn read_pid_file_returns_none_when_missing() {
        let tmp = tempdir().unwrap();
        let config = config_in(tmp.path());

        let pid = read_pid_file(&config).unwrap();
        assert_eq!(pid, None);
    }

    #[test]
    fn remove_pid_file_deletes_file() {
        let tmp = tempdir().unwrap();
        let config = config_in(tmp.path());

        write_pid_file(&config).unwrap();
        assert!(config.pid_file.exists());

        remove_pid_file(&config).unwrap();
        assert!(!config.pid_file.exists());
    }

    #[test]
    fn remove_pid_file_ok_when_missing() {
        let tmp = tempdir().unwrap();
        let config = config_in(tmp.path());

        // Should not error when file doesn't exist
        remove_pid_file(&config).unwrap();
    }

    #[test]
    fn write_pid_creates_parent_directories() {
        let tmp = tempdir().unwrap();
        let config = DaemonConfig {
            pid_file: tmp.path().join("nested").join("deep").join("test.pid"),
            ..DaemonConfig::default()
        };

        write_pid_file(&config).unwrap();

        let pid = read_pid_file(&config).unwrap();
        assert_eq!(pid, Some(std::process::id()));
    }

    // ── is_running ──────────────────────────────────────────────────

    #[test]
    fn is_running_returns_false_with_no_pid_file() {
        let tmp = tempdir().unwrap();
        let config = config_in(tmp.path());

        assert!(!is_running(&config));
    }

    #[test]
    fn is_running_returns_true_for_current_process() {
        let tmp = tempdir().unwrap();
        let config = config_in(tmp.path());

        write_pid_file(&config).unwrap();
        assert!(is_running(&config));
    }

    #[test]
    fn is_running_returns_false_for_nonexistent_pid() {
        let tmp = tempdir().unwrap();
        let config = config_in(tmp.path());

        // Write a PID that (almost certainly) doesn't exist.
        // PID 99999 is far above typical daemon PIDs but below max; unlikely to be in use.
        // We also try a few to find one that truly isn't running.
        let fake_pid = (99_990..=99_999)
            .find(|&p| unsafe { libc::kill(p as libc::pid_t, 0) != 0 })
            .expect("could not find a non-existent PID in range 99990..99999");
        std::fs::write(&config.pid_file, fake_pid.to_string()).unwrap();
        assert!(!is_running(&config));
    }

    // ── Log rotation ────────────────────────────────────────────────

    #[test]
    fn rotate_log_noop_when_no_log_configured() {
        let tmp = tempdir().unwrap();
        let config = config_in(tmp.path());

        // log_file is None — should be a no-op
        rotate_log(&config).unwrap();
    }

    #[test]
    fn rotate_log_noop_when_file_missing() {
        let tmp = tempdir().unwrap();
        let config = DaemonConfig {
            log_file: Some(tmp.path().join("daemon.log")),
            ..config_in(tmp.path())
        };

        rotate_log(&config).unwrap();
    }

    #[test]
    fn rotate_log_noop_when_under_max_size() {
        let tmp = tempdir().unwrap();
        let log_path = tmp.path().join("daemon.log");
        std::fs::write(&log_path, "small content").unwrap();

        let config = DaemonConfig {
            log_file: Some(log_path.clone()),
            log_max_size: 1024, // 1 KB — file is well under this
            ..config_in(tmp.path())
        };

        rotate_log(&config).unwrap();

        // File should still exist and not be renamed
        assert!(log_path.exists());
        assert!(!tmp.path().join("daemon.log.1").exists());
    }

    #[test]
    fn rotate_log_renames_when_over_max_size() {
        let tmp = tempdir().unwrap();
        let log_path = tmp.path().join("daemon.log");

        // Write content exceeding max size
        let content = "x".repeat(2000);
        std::fs::write(&log_path, &content).unwrap();

        let config = DaemonConfig {
            log_file: Some(log_path.clone()),
            log_max_size: 1000, // 1 KB threshold
            ..config_in(tmp.path())
        };

        rotate_log(&config).unwrap();

        // Original file should be empty (new log)
        assert!(log_path.exists());
        let new_content = std::fs::read_to_string(&log_path).unwrap();
        assert!(new_content.is_empty());

        // Rotated file should have original content
        let rotated = tmp.path().join("daemon.log.1");
        assert!(rotated.exists());
        let rotated_content = std::fs::read_to_string(&rotated).unwrap();
        assert_eq!(rotated_content.len(), 2000);
    }
}

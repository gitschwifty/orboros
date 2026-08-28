use std::path::PathBuf;
use std::time::Duration;

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
    /// Optional aggregate cap shared by all supervisor-managed projects.
    pub global_max_concurrency: Option<usize>,
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
            global_max_concurrency: None,
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
    crate::bench::log::reopen_general(log_file)
        .with_context(|| format!("reopening rotated log file: {}", log_file.display()))?;

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

/// Per-tick dispatch settings. When present, the daemon calls
/// `queue.dispatch_ready_orbs` after each `tick()` to actually
/// run workers against orbs that landed in worker-eligible states.
/// When `None`, the daemon stays pure-state-machine (the prior
/// behavior).
#[derive(Debug, Clone)]
pub struct DispatchSettings {
    pub base_worker_config: crate::worker::process::WorkerConfig,
    pub max_concurrency: usize,
}

/// One independently-ticked project managed by the supervisor daemon.
#[derive(Clone)]
pub struct SupervisedProject {
    pub name: String,
    pub queue: crate::queue_loop::QueueLoop,
    pub dispatch: Option<DispatchSettings>,
}

fn log_run_summary(projects: &[SupervisedProject], started: chrono::DateTime<chrono::Utc>) {
    let mut done = 0_u64;
    let mut failed = 0_u64;
    let mut retries = 0_u64;
    let mut in_flight_dispatches = 0_u64;
    let mut input_tokens = 0_u64;
    let mut output_tokens = 0_u64;
    let mut cache_read_tokens = 0_u64;
    let mut cache_write_tokens = 0_u64;
    let mut cost_micros = 0_u64;
    for project in projects {
        match project.queue.execution_store().read_all() {
            Ok(records) => {
                for record in records.into_iter().filter(|r| r.dispatched_at >= started) {
                    input_tokens = input_tokens.saturating_add(record.prompt_tokens.unwrap_or(0));
                    output_tokens =
                        output_tokens.saturating_add(record.completion_tokens.unwrap_or(0));
                    cache_read_tokens =
                        cache_read_tokens.saturating_add(record.cache_read_tokens.unwrap_or(0));
                    cache_write_tokens =
                        cache_write_tokens.saturating_add(record.cache_write_tokens.unwrap_or(0));
                    cost_micros = cost_micros.saturating_add(record.cost_micros.unwrap_or(0));
                    retries = retries.saturating_add(u64::from(record.retries));
                    match record.status.as_str() {
                        "done" => done += 1,
                        "error" | "failed" => failed += 1,
                        _ => {}
                    }
                }
            }
            Err(error) => {
                tracing::warn!(project = %project.name, %error, "could not read execution summary");
            }
        }
        in_flight_dispatches = in_flight_dispatches.saturating_add(
            u64::try_from(project.queue.in_flight_dispatch_count()).unwrap_or(u64::MAX),
        );
    }
    tracing::info!(
        elapsed_secs = (chrono::Utc::now() - started).num_seconds(),
        completed = done,
        failed,
        in_flight_dispatches,
        retries,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        cost_usd = %crate::execution::format_cost_usd(Some(cost_micros)),
        "supervisor run summary"
    );
}

async fn tick_project(
    project: SupervisedProject,
    global_semaphore: Option<std::sync::Arc<tokio::sync::Semaphore>>,
) {
    let project_name = project.name.as_str();
    match project.queue.tick_async().await {
        Ok(result) if !result.is_idle() => tracing::debug!(
            project = project_name,
            pipelines = result.pipelines_started,
            executed = result.orbs_executed,
            completed = result.roots_completed,
            reevaluated = result.orbs_reevaluated,
            "tick completed"
        ),
        Ok(_) => {}
        Err(error) => {
            tracing::error!(project = project_name, %error, "project tick failed; continuing");
            return;
        }
    }

    if let Some(settings) = &project.dispatch {
        match project
            .queue
            .dispatch_ready_orbs_with_global(
                &settings.base_worker_config,
                settings.max_concurrency,
                global_semaphore,
            )
            .await
        {
            Ok(0) => {}
            Ok(dispatched) => tracing::debug!(
                project = project_name,
                dispatched,
                "workers completed this tick"
            ),
            Err(error) => {
                tracing::error!(project = project_name, %error, "project dispatch failed; continuing");
            }
        }
    }
    project.queue.fire_on_queue_tick().await;
}

/// Run exactly one isolated tick for every supplied project.  This is kept
/// public for operator tooling and lets tests exercise supervisor behavior
/// without waiting for signals or a timer.
pub async fn tick_supervised_projects(projects: &[SupervisedProject]) {
    tick_supervised_projects_with_global(projects, None).await;
}

async fn tick_supervised_projects_with_global(
    projects: &[SupervisedProject],
    global_semaphore: Option<std::sync::Arc<tokio::sync::Semaphore>>,
) {
    let mut tasks = tokio::task::JoinSet::new();
    for project in projects.iter().cloned() {
        let global_semaphore = global_semaphore.clone();
        tasks.spawn(async move { tick_project(project, global_semaphore).await });
    }
    while let Some(result) = tasks.join_next().await {
        if let Err(error) = result {
            tracing::error!(%error, "supervised project tick panicked");
        }
    }
}

/// Run a single PID-managed daemon which supervises multiple project queues.
/// A failed queue or worker setup in one project is logged and does not prevent
/// the remaining projects from receiving their tick.
pub async fn run_supervisor(config: DaemonConfig, projects: Vec<SupervisedProject>) -> Result<()> {
    run_supervisor_with_control_socket(config, projects, None).await
}

/// Runs the supervisor and, when supplied, serves its authoritative local
/// shared-state control socket for the lifetime of the daemon.
pub async fn run_supervisor_with_control_socket(
    config: DaemonConfig,
    projects: Vec<SupervisedProject>,
    control_socket: Option<(
        tokio::net::UnixListener,
        std::sync::Arc<tokio::sync::Mutex<crate::supervisor::LocalSupervisor>>,
    )>,
) -> Result<()> {
    write_pid_file(&config).context("failed to write PID file")?;
    tracing::info!(
        pid = std::process::id(),
        projects = projects.len(),
        "supervisor daemon started"
    );
    let mut shutdown_rx = setup_signal_handlers();
    let run_started_at = chrono::Utc::now();
    let summary_projects = projects.clone();
    let summary_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_mins(1));
        interval.tick().await;
        loop {
            interval.tick().await;
            log_run_summary(&summary_projects, run_started_at);
        }
    });
    let tick_interval = std::time::Duration::from_millis(config.tick_interval_ms);
    let global_semaphore = config
        .global_max_concurrency
        .map(|limit| std::sync::Arc::new(tokio::sync::Semaphore::new(limit.max(1))));
    let control_supervisor = control_socket
        .as_ref()
        .map(|(_, supervisor)| std::sync::Arc::clone(supervisor));
    let control_server = control_socket.map(|(listener, supervisor)| {
        tokio::spawn(crate::supervisor::serve_local_socket(listener, supervisor))
    });

    'supervisor: loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    tracing::info!("shutdown signal received, stopping supervisor");
                    for project in &projects { project.queue.stop(); }
                    break 'supervisor;
                }
            }
            () = tokio::time::sleep(tick_interval) => {
                if let Err(error) = rotate_log(&config) { tracing::warn!(%error, "log rotation failed"); }
                let tick = tick_supervised_projects_with_global(&projects, global_semaphore.clone());
                tokio::pin!(tick);
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            tracing::info!("shutdown signal received, draining supervisor dispatches");
                            for project in &projects { project.queue.stop(); }
                            // Do not drop the tick future here: dropping it
                            // aborts its JoinSet and turns an operator-requested
                            // shutdown into worker transport errors. `stop()`
                            // prevents still-queued work from starting; work
                            // that already holds a permit finishes and writes
                            // its normal outcome before we exit.
                            (&mut tick).await;
                            break 'supervisor;
                        }
                    }
                    () = &mut tick => {}
                }
                if let Some(supervisor) = &control_supervisor {
                    let (queues, dispatch) = supervisor.lock().await.take_attached_queues();
                    let attached: Vec<_> = queues.iter().filter_map(|(name, queue)| {
                        Some(SupervisedProject {
                            name: name.clone(),
                            queue: queue.clone(),
                            dispatch: dispatch.get(name)?.clone(),
                        })
                    }).collect();
                    tick_supervised_projects_with_global(&attached, global_semaphore.clone()).await;
                    supervisor.lock().await.restore_attached_queues(queues, dispatch);
                }
            }
        }
    }
    if let Err(error) = remove_pid_file(&config) {
        tracing::warn!(%error, "failed to remove PID file");
    }
    if let Some(server) = control_server {
        server.abort();
    }
    summary_task.abort();
    tracing::info!("supervisor daemon stopped");
    Ok(())
}

/// Runs the daemon: writes PID file, sets up signal handlers, runs the queue
/// loop, and cleans up on shutdown.
///
/// When `dispatch` is `Some`, the daemon also calls
/// `queue.dispatch_ready_orbs` after each tick to spawn workers
/// for any orbs that became worker-eligible. The `on-queue-tick`
/// hook fires after each non-paused tick regardless.
///
/// # Errors
/// Returns an error if PID file operations or the queue loop fail.
pub async fn run_daemon(
    config: DaemonConfig,
    queue: crate::queue_loop::QueueLoop,
    dispatch: Option<DispatchSettings>,
) -> Result<()> {
    // Write PID file
    write_pid_file(&config).context("failed to write PID file")?;
    tracing::info!(
        pid = std::process::id(),
        pid_file = %config.pid_file.display(),
        dispatch_enabled = dispatch.is_some(),
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

                // Run a tick (state transitions only). Uses the
                // async path so `pre-/post-phase-transition` hooks
                // fire around each phase change.
                match queue.tick_async().await {
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

                // Dispatch workers for any newly-eligible orbs.
                if let Some(ref settings) = dispatch {
                    match queue
                        .dispatch_ready_orbs(&settings.base_worker_config, settings.max_concurrency)
                        .await
                    {
                        Ok(0) => {} // idle
                        Ok(n) => {
                            tracing::debug!(dispatched = n, "workers completed this tick");
                        }
                        Err(e) => {
                            tracing::error!("dispatch_ready_orbs failed: {e}");
                        }
                    }
                }

                // Fire on-queue-tick hook (closes one of the task 56
                // follow-ups: the daemon now matches QueueLoop::run's
                // hook firing behavior).
                queue.fire_on_queue_tick().await;
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
    use orbs::dep_store::DepStore;
    use orbs::orb_store::OrbStore;
    use tempfile::tempdir;

    fn config_in(dir: &std::path::Path) -> DaemonConfig {
        DaemonConfig {
            pid_file: dir.join("test.pid"),
            log_file: None,
            log_max_size: 10 * 1024 * 1024,
            tick_interval_ms: 1000,
            global_max_concurrency: None,
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

    #[tokio::test]
    async fn supervisor_ticks_each_registered_project() {
        let tmp = tempdir().unwrap();
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        let project = |name: &str, dir: &std::path::Path| SupervisedProject {
            name: name.into(),
            queue: crate::queue_loop::QueueLoop::new(
                OrbStore::new(dir.join("orbs.jsonl")),
                DepStore::new(dir.join("deps.jsonl")),
                dir.to_path_buf(),
            ),
            dispatch: None,
        };
        let projects = vec![project("first", &first), project("second", &second)];

        tick_supervised_projects(&projects).await;

        // Both queues are still live: a clean/idle first project did not
        // prevent the second one from receiving its independent tick.
        assert!(projects.iter().all(|project| project
            .queue
            .running_flag()
            .load(std::sync::atomic::Ordering::SeqCst)));
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

#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]

use std::path::{Path, PathBuf};

use clap::{ArgAction, Parser, Subcommand};

use orbs::dep_store::DepStore;
use orbs::id::OrbId;
use orbs::orb::{Orb, OrbType};
use orbs::orb_store::OrbStore;

use orboros::config;
use orboros::coordinator::decompose::decompose_with_prompt_resolver;
use orboros::daemon::DaemonConfig;
use orboros::orb_cmd;
use orboros::orchestrator::{orchestrate, OrchestrateConfig, CONTEXT_RESULT_MAX_CHARS};
use orboros::plan::{self, PlanConfig};
use orboros::queue_loop::{DrainResult, QueueLoop};
use orboros::routing::profile::builtin_tools;
use orboros::runner::execute_task;
use orboros::state::store::TaskStore;
use orboros::state::task::{Task, TaskStatus};
use orboros::worker::process::WorkerConfig;

const DEFAULT_STATE_DIR: &str = "~/.orboros/default";

#[derive(Debug, Clone)]
struct EffectiveStateDir {
    state_dir: PathBuf,
    project_dir: Option<PathBuf>,
}

/// Orboros — multi-agent orchestrator.
#[derive(Parser)]
#[command(name = "orboros", version, about)]
struct Cli {
    /// Path to the project state directory. Defaults to nearest ancestor .orbs,
    /// then ~/.orboros/default.
    #[arg(long, default_value = "~/.orboros/default")]
    state_dir: String,

    /// Path to the heddle-headless binary.
    #[arg(long, env = "HEDDLE_BINARY")]
    worker_binary: Option<String>,

    /// Model catalog key or raw provider/model override for workers.
    #[arg(long)]
    model: Option<String>,

    /// Skip startup validation of worker binary, model string, and
    /// provider credentials. Use when running against a local proxy or
    /// when the validator is being overly strict.
    #[arg(long, global = true)]
    skip_prereq_check: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a task orb and run the normal queue/dispatch path in the foreground.
    Run {
        /// The task description.
        task: String,
        /// Priority (1=highest, 5=lowest).
        #[arg(short, long, default_value = "3")]
        priority: u8,
        /// Queue only, don't execute immediately.
        #[arg(long)]
        queue: bool,
        /// Maximum foreground queue cycles before giving up.
        #[arg(long, default_value_t = 20)]
        max_ticks: u32,
        /// Delay between foreground queue cycles.
        #[arg(long, default_value_t = 100)]
        interval_ms: u64,
    },
    /// Legacy `TaskStore`: decompose a task into subtasks without executing.
    Decompose {
        /// The high-level task to decompose.
        task: String,
        /// Override the decomposition system prompt.
        #[arg(long)]
        system_prompt: Option<String>,
        /// Read the system prompt override from a file.
        #[arg(long)]
        system_prompt_file: Option<PathBuf>,
    },
    /// Legacy `TaskStore`: decompose a task and execute all subtasks.
    Orchestrate {
        /// The high-level task to orchestrate.
        task: String,
        /// Priority for subtasks (1=highest, 5=lowest).
        #[arg(short, long, default_value = "3")]
        priority: u8,
        /// Override all system prompts used by this orchestration.
        #[arg(long)]
        system_prompt: Option<String>,
        /// Read the system prompt override from a file.
        #[arg(long)]
        system_prompt_file: Option<PathBuf>,
    },
    /// Legacy `TaskStore`: list tasks, optionally filtered by status.
    Tasks {
        /// Filter by status (pending, active, review, done, failed).
        #[arg(short, long)]
        status: Option<String>,
    },
    /// Legacy `TaskStore`: show status of a specific task by ID.
    Status {
        /// Task ID (UUID).
        id: String,
    },
    /// Legacy `TaskStore`: list tasks awaiting review.
    Review,
    /// Access legacy `TaskStore` commands backed by tasks.jsonl.
    Legacy {
        #[command(subcommand)]
        action: LegacyAction,
    },
    /// Drive the normal orb queue/dispatch path for an existing orb.
    Execute {
        /// Orb ID (e.g. orb-k4f).
        id: String,
        /// Wait until the target orb reaches a terminal state.
        #[arg(long)]
        wait: bool,
        /// Maximum foreground queue cycles before giving up.
        #[arg(long, default_value_t = 20)]
        max_ticks: u32,
        /// Delay between foreground queue cycles.
        #[arg(long, default_value_t = 100)]
        interval_ms: u64,
    },
    /// Create a plan by decomposing a description into an epic with subtasks.
    Plan {
        /// The task description (or use --file to read from a markdown file).
        description: Option<String>,
        /// Read the plan description from a markdown file.
        #[arg(long)]
        file: Option<PathBuf>,
        /// Only run shallow decomposition (no refinement).
        #[arg(long)]
        shallow: bool,
    },
    /// Initialize a new project in the current directory.
    Init,
    /// Create, upgrade, or inspect the layered Orboros configuration.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Run or manage the daemon process.
    Daemon {
        /// Stop a running daemon.
        #[arg(long)]
        stop: bool,
        /// Show daemon status.
        #[arg(long)]
        status: bool,
        /// PID file path (default: ~/.orboros/orboros.pid).
        #[arg(long)]
        pid_file: Option<String>,
        /// Log file path.
        #[arg(long)]
        log_file: Option<String>,
        /// Tick interval in milliseconds (default: 1000).
        #[arg(long)]
        tick_interval: Option<u64>,
        /// Supervise only this registered project name.
        #[arg(long)]
        project: Option<String>,
    },
    /// Manage orbs (create, show, list, update, delete, deps, review).
    Orb {
        #[command(subcommand)]
        action: OrbAction,
    },
    /// Start an interactive conversation with an agent.
    Chat {
        /// Override the model for this session (defaults to top-level --model).
        #[arg(long)]
        chat_model: Option<String>,
        /// System prompt for the session.
        #[arg(long, default_value = "You are a helpful conversational agent.")]
        system_prompt: String,
        /// Tie this session to an existing orb id (recorded in transcript).
        #[arg(long)]
        link_orb: Option<String>,
    },
    /// List or inspect past chat sessions.
    Sessions {
        #[command(subcommand)]
        action: Option<SessionsAction>,
    },
    /// Inspect and manually fire lifecycle hooks.
    Hooks {
        #[command(subcommand)]
        action: HooksAction,
    },
    /// List orbs whose second-opinion reviewer verdict is `Revise`,
    /// pending operator action.
    ReviewQueue,
    /// Benchmark corpus + harness (task 59).
    Bench {
        /// Root containing t1/, t2/, t3/, prompts/, and results/.
        #[arg(long, env = "ORBOROS_BENCH_ROOT", default_value = "bench")]
        bench_root: PathBuf,
        /// Benchmark config file. Defaults to `<bench-root>/config.toml` when present.
        #[arg(long, env = "ORBOROS_BENCH_CONFIG")]
        bench_config: Option<PathBuf>,
        /// Directory for benchmark run/result JSONL. Defaults to `<bench-root>/results`.
        #[arg(long, env = "ORBOROS_BENCH_RESULTS_DIR")]
        bench_results_dir: Option<PathBuf>,
        #[command(subcommand)]
        action: BenchAction,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Write an annotated starter config (project by default).
    Init {
        /// Target ~/.orboros/config.toml instead of .orbs/config.toml.
        #[arg(long)]
        global: bool,
        /// Replace an existing config file.
        #[arg(long)]
        force: bool,
        /// Write only the version marker so settings inherit from lower layers.
        #[arg(long)]
        minimal: bool,
    },
    /// Preview or apply a schema/default upgrade (project by default).
    Upgrade {
        #[arg(long)]
        global: bool,
        /// Persist the previewed upgrade. Without this flag no files change.
        #[arg(long)]
        apply: bool,
    },
    /// Print effective configuration values and their layered sources.
    Show,
}

#[derive(Subcommand)]
enum BenchAction {
    /// List every case in the corpus, grouped by tier.
    List,
    /// Run benchmark cases.
    Run {
        /// Tier to run. Omit to run every tier.
        #[arg(long)]
        tier: Option<String>,
        /// Single case id to run (overrides --tier filtering).
        #[arg(long)]
        case: Option<String>,
        /// Model catalog key or raw provider/model string for benchmark workers.
        #[arg(long)]
        model: Option<String>,
        /// Human-readable variant label stored with the run.
        #[arg(long)]
        variant: Option<String>,
        /// Named Markdown prompt set under <bench-root>/prompts/.
        #[arg(long)]
        prompt_set: Option<String>,
        /// Skip the per-case cost ceiling (`max_cost_cents`).
        #[arg(long)]
        no_budget: bool,
        /// Maximum benchmark cases to run concurrently. Defaults to serial execution.
        #[arg(long)]
        jobs: Option<usize>,
    },
    /// Print every result row in a saved run.
    Show {
        /// Run id, as printed by `bench run` or `bench list-runs`.
        run_id: String,
    },
    /// Print detailed saved output for failed/error cases in a run.
    Details {
        /// Run id, as printed by `bench run` or `bench list-runs`.
        run_id: String,
        /// Limit details to one case id.
        #[arg(long)]
        case: Option<String>,
        /// Include passing cases too. Defaults to non-pass only.
        #[arg(long)]
        all: bool,
    },
    /// Summarize persisted per-dispatch telemetry for one benchmark run.
    Report {
        /// Run id, as printed by `bench run` or `bench list-runs`.
        run_id: String,
        /// Limit the report to one canonical case id.
        #[arg(long)]
        case: Option<String>,
    },
    /// Aggregate persisted dispatch telemetry across compatible historical runs.
    ReportHistory,
    /// Inspect durable worker prompt snapshots from one benchmark run.
    Prompts {
        /// Run id, as printed by `bench run` or `bench list-runs`.
        run_id: String,
        /// Canonical case id to inspect. Required to avoid dumping a whole run.
        #[arg(long)]
        case: String,
        /// Limit output to one orb id.
        #[arg(long)]
        orb: Option<String>,
    },
    /// Diff two saved runs by case outcome.
    Compare { run_a: String, run_b: String },
    /// List recorded runs, with optional comparability filters.
    ListRuns {
        /// Match the human-readable experiment variant label.
        #[arg(long)]
        variant: Option<String>,
        /// Match configured or resolved worker model text.
        #[arg(long)]
        model: Option<String>,
        /// Match a tier (`t1`, `t2`, or `t3`).
        #[arg(long)]
        tier: Option<String>,
        /// Match a suite fingerprint or its prefix.
        #[arg(long)]
        suite: Option<String>,
        /// Match a benchmark prompt-set name.
        #[arg(long)]
        prompt_set: Option<String>,
        /// Include runs started on or after this UTC date (`YYYY-MM-DD`).
        #[arg(long)]
        since: Option<String>,
        /// Show at most this many newest matching runs.
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Calibration report: bucket confidence vs pass rate + correlation.
    Calibration {
        /// Run id to analyze.
        run_id: String,
        /// Number of histogram buckets across [0.0, 1.0].
        #[arg(long, default_value_t = 10)]
        buckets: usize,
    },
}

#[derive(Subcommand)]
enum LegacyAction {
    /// Submit a new legacy task for execution.
    Run {
        /// The task description.
        task: String,
        /// Priority (1=highest, 5=lowest).
        #[arg(short, long, default_value = "3")]
        priority: u8,
        /// Queue only, don't execute immediately.
        #[arg(long)]
        queue: bool,
        /// Override the system prompt for this worker invocation.
        #[arg(long)]
        system_prompt: Option<String>,
        /// Read the system prompt override from a file.
        #[arg(long)]
        system_prompt_file: Option<PathBuf>,
    },
    /// Decompose a legacy task into subtasks without executing.
    Decompose {
        /// The high-level task to decompose.
        task: String,
        /// Override the decomposition system prompt.
        #[arg(long)]
        system_prompt: Option<String>,
        /// Read the system prompt override from a file.
        #[arg(long)]
        system_prompt_file: Option<PathBuf>,
    },
    /// Decompose a legacy task and execute all subtasks.
    Orchestrate {
        /// The high-level task to orchestrate.
        task: String,
        /// Priority for subtasks (1=highest, 5=lowest).
        #[arg(short, long, default_value = "3")]
        priority: u8,
        /// Override all system prompts used by this orchestration.
        #[arg(long)]
        system_prompt: Option<String>,
        /// Read the system prompt override from a file.
        #[arg(long)]
        system_prompt_file: Option<PathBuf>,
    },
    /// List legacy tasks, optionally filtered by status.
    Tasks {
        /// Filter by status (pending, active, review, done, failed).
        #[arg(short, long)]
        status: Option<String>,
    },
    /// Show status of a specific legacy task by ID.
    Status {
        /// Task ID (UUID).
        id: String,
    },
    /// List legacy tasks awaiting review.
    Review,
}

#[derive(Subcommand)]
enum HooksAction {
    /// Print every loaded hook with its event, source layer, and match summary.
    List,
    /// Validate global + project hooks.toml without firing anything.
    Check,
    /// Manually fire a named hook against an existing orb id.
    Run {
        /// Hook name as listed by `orboros hooks list`.
        name: String,
        /// Orb id to fire against (e.g. orb-abc1234).
        #[arg(long)]
        orb: String,
        /// Don't actually spawn the hook command; just record what would
        /// happen and pass `ORBOROS_DRY_RUN=1` in the env.
        #[arg(long)]
        dry_run: bool,
    },
    /// Print recorded hook invocations from the log.
    Log {
        /// Filter to invocations targeting this orb id.
        #[arg(long)]
        orb: Option<String>,
        /// Maximum entries to print (newest first). 0 means all.
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
}

#[derive(Subcommand)]
enum SessionsAction {
    /// List sessions, optionally filtered by status.
    List {
        /// Filter by status (active, idle, closed).
        #[arg(short, long)]
        status: Option<String>,
    },
    /// Replay a session's transcript.
    Show {
        /// Session id (e.g. session-abc12345).
        id: String,
    },
}

#[derive(Subcommand)]
enum OrbAction {
    /// Create a new orb.
    Create {
        /// Title for the orb.
        title: String,
        /// Description (defaults to title if not provided).
        #[arg(short, long)]
        description: Option<String>,
        /// Orb type: task, epic, feature, bug, chore, docs.
        #[arg(short = 't', long = "type", default_value = "task")]
        orb_type: String,
        /// Priority (1=critical, 5=backlog).
        #[arg(short, long, default_value = "3")]
        priority: u8,
        /// Attach a label to the orb. Repeatable: `--label db --label external`.
        #[arg(long = "label", value_name = "LABEL")]
        labels: Vec<String>,
    },
    /// Show details of an orb.
    Show {
        /// Orb ID (e.g. orb-k4f).
        id: String,
    },
    /// List orbs with optional filters.
    List {
        /// Filter by type (task, epic, feature, bug, chore, docs).
        #[arg(short = 't', long = "type")]
        orb_type: Option<String>,
        /// Filter by status (draft, pending, active, review, done, failed, cancelled, deferred).
        #[arg(short, long)]
        status: Option<String>,
        /// Only show orbs whose confidence is at least this value (0.0–1.0).
        #[arg(long)]
        min_confidence: Option<f32>,
        /// Only show orbs whose confidence is at most this value (0.0–1.0).
        #[arg(long)]
        max_confidence: Option<f32>,
        /// Filter by second-opinion reviewer verdict (accept, reject, revise, any, missing).
        #[arg(long)]
        review_status: Option<String>,
        /// Show only orbs with at least one of these labels (any-of).
        /// Repeatable: `--label db --label external`.
        #[arg(long = "label", value_name = "LABEL")]
        label: Vec<String>,
    },
    /// Update fields on an existing orb.
    Update {
        /// Orb ID.
        id: String,
        /// New title.
        #[arg(long)]
        title: Option<String>,
        /// New description.
        #[arg(long)]
        description: Option<String>,
        /// New priority (1-5).
        #[arg(short, long)]
        priority: Option<u8>,
        /// New status.
        #[arg(short, long)]
        status: Option<String>,
        /// Set the orb's confidence score (0.0–1.0). Used by the benchmark
        /// harness and manual reviewer scoring.
        #[arg(long)]
        confidence: Option<f32>,
        /// Set whether a parent has its own final synthesis/verification work.
        /// Pass `true` or `false`; relevant to phase-type parent orbs.
        #[arg(long, action = ArgAction::Set)]
        parent_final_work: Option<bool>,
        /// Add a label to the orb. Repeatable: `--add-label db --add-label external`.
        #[arg(long = "add-label", value_name = "LABEL")]
        add_label: Vec<String>,
        /// Remove a label from the orb. Repeatable.
        #[arg(long = "remove-label", value_name = "LABEL")]
        remove_label: Vec<String>,
        /// Replace the orb's labels entirely. Comma-separated:
        /// `--set-labels db,external,wip`. Wins over --add-label / --remove-label.
        #[arg(long = "set-labels", value_name = "CSV")]
        set_labels: Option<String>,
    },
    /// Soft-delete (tombstone) an orb.
    Delete {
        /// Orb ID.
        id: String,
        /// Reason for deletion.
        #[arg(short, long)]
        reason: Option<String>,
    },
    /// Manage dependencies between orbs.
    Dep {
        #[command(subcommand)]
        dep_action: DepAction,
    },
    /// List dependencies for an orb.
    Deps {
        /// Orb ID.
        id: String,
    },
    /// Apply a review decision (approve, reject, revise).
    Review {
        /// Orb ID.
        id: String,
        /// Decision: approve, reject, or revise.
        decision: String,
    },
}

#[derive(Subcommand)]
enum DepAction {
    /// Add a dependency edge.
    Add {
        /// Source orb ID.
        from: String,
        /// Target orb ID.
        to: String,
        /// Edge type: blocks, `depends_on`, parent, child, related, duplicates, follows.
        #[arg(short = 't', long = "type", default_value = "blocks")]
        edge_type: String,
    },
    /// Remove a dependency edge.
    Rm {
        /// Source orb ID.
        from: String,
        /// Target orb ID.
        to: String,
        /// Edge type.
        #[arg(short = 't', long = "type", default_value = "blocks")]
        edge_type: String,
    },
}

fn resolve_state_dir(raw: &str) -> PathBuf {
    if raw.starts_with('~') {
        if let Some(home) = dirs::home_dir() {
            return home.join(&raw[2..]);
        }
    }
    PathBuf::from(raw)
}

fn project_dir_for_state_dir(state_dir: &Path) -> Option<PathBuf> {
    (state_dir.file_name().and_then(|name| name.to_str()) == Some(".orbs"))
        .then(|| state_dir.parent().map(Path::to_path_buf))
        .flatten()
}

fn find_orbs_dir_upwards(start: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        if home.is_some_and(|home| dir == home) {
            break;
        }
        let candidate = dir.join(".orbs");
        if candidate.is_dir() {
            return Some(candidate);
        }
        current = dir.parent();
    }
    None
}

fn resolve_effective_state_dir(raw: &str) -> EffectiveStateDir {
    let fallback = resolve_state_dir(raw);
    if raw != DEFAULT_STATE_DIR {
        let project_dir = project_dir_for_state_dir(&fallback);
        return EffectiveStateDir {
            state_dir: fallback,
            project_dir,
        };
    }

    if let Ok(cwd) = std::env::current_dir() {
        if let Some(orbs_dir) = find_orbs_dir_upwards(&cwd, dirs::home_dir().as_deref()) {
            let project_dir = project_dir_for_state_dir(&orbs_dir);
            return EffectiveStateDir {
                state_dir: orbs_dir,
                project_dir,
            };
        }
    }

    EffectiveStateDir {
        state_dir: fallback,
        project_dir: None,
    }
}

fn require_binary(worker_binary: Option<&str>) -> anyhow::Result<&str> {
    worker_binary.ok_or_else(|| {
        anyhow::anyhow!("No worker binary configured. Set --worker-binary or HEDDLE_BINARY.")
    })
}

/// Combined entry point used by every worker-spawning command. Resolves
/// the worker binary (errors if unset), then runs `validate_worker_prereqs`
/// unless `skip_prereq_check` is true.
///
/// Returns the resolved binary path for the caller to keep using.
fn prereq_check(
    worker_binary: Option<&str>,
    model: &str,
    router: Option<&str>,
    skip: bool,
) -> anyhow::Result<String> {
    let binary = require_binary(worker_binary)?;
    if skip {
        tracing::warn!("--skip-prereq-check set; trusting caller for binary/model/credentials");
        return Ok(binary.to_string());
    }
    orboros::startup_check::validate_worker_prereqs(&orboros::startup_check::PrereqCheck {
        worker_binary: binary,
        model,
        router,
        require_credentials: true,
    })?;
    Ok(orboros::startup_check::resolve_binary(binary)?
        .display()
        .to_string())
}

fn resolved_worker_config(
    project_dir: Option<&Path>,
    worker_binary: Option<&str>,
    model: Option<&str>,
    role: config::ModelRole<'_>,
    skip_prereq_check: bool,
) -> anyhow::Result<(WorkerConfig, config::OrbConfig)> {
    let resolver = config::RuntimeConfigResolver::load(project_dir, model, worker_binary)?;
    let resolved_model = resolver.model_for(role)?;
    let binary = prereq_check(
        Some(resolver.worker_binary()?),
        &resolved_model.model,
        resolved_model.router.as_deref(),
        skip_prereq_check,
    )?;
    Ok((
        make_worker_config(&binary, &resolved_model.model, ""),
        resolver.config,
    ))
}

fn make_worker_config(binary: &str, model: &str, system_prompt: &str) -> WorkerConfig {
    WorkerConfig {
        command: binary.into(),
        args: vec![],
        cwd: None,
        env: vec![],
        model: model.into(),
        system_prompt: system_prompt.into(),
        tools: builtin_tools("execute")
            .iter()
            .map(ToString::to_string)
            .collect(),
        max_iterations: None,
        init_timeout: None,
        send_timeout: None,
        shutdown_timeout: None,
        task_id: None,
        worker_id: None,
        runtime: None,
        routing: None,
    }
}

#[allow(
    clippy::items_after_statements,
    clippy::too_many_arguments,
    clippy::too_many_lines
)]
fn main() -> anyhow::Result<()> {
    // Load .env from current dir or ancestors (silently ignore if missing)
    let _ = dotenvy::dotenv();

    use tracing_subscriber::prelude::*;

    let terminal_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("orboros=info,heddle=warn"));
    let bench_filter = tracing_subscriber::EnvFilter::new("orboros=info,heddle=warn");
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_filter(terminal_filter),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(orboros::bench::log::BenchLogWriter)
                .with_ansi(false)
                .with_target(false)
                .with_filter(bench_filter),
        )
        .init();

    let cli = Cli::parse();
    let effective_state = resolve_effective_state_dir(&cli.state_dir);
    let state_dir = effective_state.state_dir;
    std::fs::create_dir_all(&state_dir)?;
    let store = TaskStore::new(state_dir.join("tasks.jsonl"));

    match cli.command {
        Commands::Run {
            task,
            priority,
            queue,
            max_ticks,
            interval_ms,
        } => cmd_run_orb(
            &state_dir,
            effective_state.project_dir.as_deref(),
            &task,
            priority,
            queue,
            cli.worker_binary.as_deref(),
            cli.model.as_deref(),
            max_ticks,
            interval_ms,
            cli.skip_prereq_check,
        ),
        Commands::Execute {
            id,
            wait,
            max_ticks,
            interval_ms,
        } => cmd_execute_orb(
            &state_dir,
            effective_state.project_dir.as_deref(),
            &id,
            wait,
            cli.worker_binary.as_deref(),
            cli.model.as_deref(),
            max_ticks,
            interval_ms,
            cli.skip_prereq_check,
        ),
        Commands::Decompose {
            task,
            system_prompt,
            system_prompt_file,
        } => cmd_decompose(
            cli.worker_binary.as_deref(),
            cli.model.as_deref(),
            &task,
            system_prompt.as_deref(),
            system_prompt_file.as_deref(),
            cli.skip_prereq_check,
        ),
        Commands::Orchestrate {
            task,
            priority,
            system_prompt,
            system_prompt_file,
        } => cmd_orchestrate(
            &store,
            cli.worker_binary.as_deref(),
            cli.model.as_deref(),
            &task,
            priority,
            system_prompt.as_deref(),
            system_prompt_file.as_deref(),
            cli.skip_prereq_check,
        ),
        Commands::Tasks { status } => cmd_tasks(&store, status.as_deref()),
        Commands::Status { id } => cmd_status(&store, &id),
        Commands::Review => cmd_review(&store),
        Commands::Legacy { action } => cmd_legacy(
            &store,
            cli.worker_binary.as_deref(),
            cli.model.as_deref(),
            action,
            cli.skip_prereq_check,
        ),
        Commands::Plan {
            description,
            file,
            shallow,
        } => cmd_plan(&state_dir, description.as_deref(), file.as_deref(), shallow),
        Commands::Init => cmd_init(),
        Commands::Config { action } => cmd_config(action, effective_state.project_dir.as_deref()),
        Commands::Daemon {
            stop,
            status,
            pid_file,
            log_file,
            tick_interval,
            project,
        } => {
            let mut daemon_config = DaemonConfig::default();
            let orb_config = config::load_config(effective_state.project_dir.as_deref())?;
            apply_daemon_settings(&mut daemon_config, &orb_config.daemon);
            if let Some(pf) = pid_file {
                daemon_config.pid_file = resolve_state_dir(&pf);
            }
            if let Some(lf) = log_file {
                daemon_config.log_file = Some(resolve_state_dir(&lf));
            }
            if let Some(ti) = tick_interval {
                daemon_config.tick_interval_ms = ti;
            }

            if stop {
                cmd_daemon_stop(&daemon_config)
            } else if status {
                cmd_daemon_status(&daemon_config, dirs::home_dir().as_deref())
            } else {
                cmd_daemon_start(
                    &store,
                    &state_dir,
                    daemon_config,
                    project.as_deref(),
                    &cli.state_dir,
                )
            }
        }
        Commands::Orb { action } => {
            let orb_store = OrbStore::new(state_dir.join("orbs.jsonl"));
            let dep_store = DepStore::new(state_dir.join("deps.jsonl"));
            let project_cwd = std::env::current_dir().unwrap_or_else(|_| state_dir.clone());
            let hooks = orboros::hooks::HookSink::from_state_dir(&state_dir, &project_cwd)
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "failed to load hooks; continuing without them");
                    None
                });
            let hooks_ref = hooks.as_ref();
            match action {
                OrbAction::Create {
                    title,
                    description,
                    orb_type,
                    priority,
                    labels,
                } => {
                    let parsed_type = orb_cmd::parse_orb_type(&orb_type)?;
                    let desc = description.as_deref().unwrap_or(&title);
                    orb_cmd::cmd_orb_create(
                        &orb_store,
                        &title,
                        desc,
                        parsed_type,
                        priority,
                        labels,
                        hooks_ref,
                    )?;
                    Ok(())
                }
                OrbAction::Show { id } => orb_cmd::cmd_orb_show(&orb_store, &id),
                OrbAction::List {
                    orb_type,
                    status,
                    min_confidence,
                    max_confidence,
                    review_status,
                    label,
                } => orb_cmd::cmd_orb_list(
                    &orb_store,
                    orb_type.as_deref(),
                    status.as_deref(),
                    min_confidence,
                    max_confidence,
                    review_status.as_deref(),
                    &label,
                ),
                OrbAction::Update {
                    id,
                    title,
                    description,
                    priority,
                    status,
                    confidence,
                    parent_final_work,
                    add_label,
                    remove_label,
                    set_labels,
                } => {
                    let label_edits = orb_cmd::LabelEdits {
                        add: add_label,
                        remove: remove_label,
                        set: set_labels
                            .map(|csv| csv.split(',').map(|s| s.trim().to_string()).collect()),
                    };
                    let result = orb_cmd::cmd_orb_update(
                        &orb_store,
                        &id,
                        title.as_deref(),
                        description.as_deref(),
                        priority,
                        status.as_deref(),
                        confidence,
                        label_edits,
                        hooks_ref,
                    );
                    result?;
                    if let Some(value) = parent_final_work {
                        orb_cmd::cmd_orb_set_parent_final_work(&orb_store, &id, value)?;
                    }
                    Ok(())
                }
                OrbAction::Delete { id, reason } => {
                    orb_cmd::cmd_orb_delete(&orb_store, &id, reason.as_deref(), hooks_ref)
                }
                OrbAction::Dep { dep_action } => match dep_action {
                    DepAction::Add {
                        from,
                        to,
                        edge_type,
                    } => {
                        let et = orb_cmd::parse_edge_type(&edge_type)?;
                        orb_cmd::cmd_orb_dep_add(&dep_store, &from, &to, et)
                    }
                    DepAction::Rm {
                        from,
                        to,
                        edge_type,
                    } => {
                        let et = orb_cmd::parse_edge_type(&edge_type)?;
                        orb_cmd::cmd_orb_dep_remove(&dep_store, &from, &to, et)
                    }
                },
                OrbAction::Deps { id } => orb_cmd::cmd_orb_deps(&dep_store, &id),
                OrbAction::Review { id, decision } => {
                    orb_cmd::cmd_orb_review(&orb_store, &id, &decision, hooks_ref)
                }
            }
        }
        Commands::Chat {
            chat_model,
            system_prompt,
            link_orb,
        } => cmd_chat(
            &state_dir,
            effective_state.project_dir.as_deref(),
            cli.worker_binary.as_deref(),
            chat_model.as_deref().or(cli.model.as_deref()),
            &system_prompt,
            link_orb.as_deref(),
            cli.skip_prereq_check,
        ),
        Commands::Sessions { action } => cmd_sessions(&state_dir, action),
        Commands::Hooks { action } => match action {
            HooksAction::List => orboros::hooks::cmd::cmd_hooks_list(&state_dir),
            HooksAction::Check => orboros::hooks::cmd::cmd_hooks_check(&state_dir),
            HooksAction::Run { name, orb, dry_run } => {
                orboros::hooks::cmd::cmd_hooks_run(&state_dir, &name, &orb, dry_run)
            }
            HooksAction::Log { orb, limit } => {
                orboros::hooks::cmd::cmd_hooks_log(&state_dir, orb.as_deref(), limit)
            }
        },
        Commands::ReviewQueue => {
            let orb_store = OrbStore::new(state_dir.join("orbs.jsonl"));
            orb_cmd::cmd_review_queue(&orb_store)
        }
        Commands::Bench {
            bench_root,
            bench_config,
            bench_results_dir,
            action,
        } => cmd_bench(
            &bench_root,
            bench_config.as_deref(),
            bench_results_dir.as_deref(),
            action,
            cli.worker_binary.as_deref(),
            cli.skip_prereq_check,
        ),
    }
}

fn apply_daemon_settings(
    daemon_config: &mut DaemonConfig,
    settings: &config::DaemonSettingsConfig,
) {
    if let Some(pid_file) = &settings.pid_file {
        daemon_config.pid_file = PathBuf::from(pid_file);
    }
    if let Some(log_file) = &settings.log_file {
        daemon_config.log_file = Some(PathBuf::from(log_file));
    }
    if let Some(log_max_size) = settings.log_max_size {
        daemon_config.log_max_size = log_max_size;
    }
    if let Some(tick_interval_ms) = settings.tick_interval_ms {
        daemon_config.tick_interval_ms = tick_interval_ms;
    }
}

fn cmd_legacy(
    store: &TaskStore,
    worker_binary: Option<&str>,
    model: Option<&str>,
    action: LegacyAction,
    skip_prereq_check: bool,
) -> anyhow::Result<()> {
    match action {
        LegacyAction::Run {
            task,
            priority,
            queue,
            system_prompt,
            system_prompt_file,
        } => cmd_legacy_run(
            store,
            worker_binary,
            model,
            &task,
            priority,
            queue,
            system_prompt.as_deref(),
            system_prompt_file.as_deref(),
            skip_prereq_check,
        ),
        LegacyAction::Decompose {
            task,
            system_prompt,
            system_prompt_file,
        } => cmd_decompose(
            worker_binary,
            model,
            &task,
            system_prompt.as_deref(),
            system_prompt_file.as_deref(),
            skip_prereq_check,
        ),
        LegacyAction::Orchestrate {
            task,
            priority,
            system_prompt,
            system_prompt_file,
        } => cmd_orchestrate(
            store,
            worker_binary,
            model,
            &task,
            priority,
            system_prompt.as_deref(),
            system_prompt_file.as_deref(),
            skip_prereq_check,
        ),
        LegacyAction::Tasks { status } => cmd_tasks(store, status.as_deref()),
        LegacyAction::Status { id } => cmd_status(store, &id),
        LegacyAction::Review => cmd_review(store),
    }
}

#[allow(clippy::too_many_lines)]
fn cmd_bench(
    bench_root: &std::path::Path,
    bench_config_path: Option<&std::path::Path>,
    bench_results_dir: Option<&std::path::Path>,
    action: BenchAction,
    worker_binary: Option<&str>,
    skip_prereq_check: bool,
) -> anyhow::Result<()> {
    use orboros::bench::cmd as bench_cmd;
    use orboros::bench::runner::BenchRunConfig;
    use orboros::bench::store::BenchStore;
    use orboros::config::ModelRole;
    let bench_dir =
        bench_results_dir.map_or_else(|| bench_root.join("results"), std::path::Path::to_path_buf);
    let store = BenchStore::new(&bench_dir);

    match action {
        BenchAction::List => bench_cmd::cmd_bench_list(bench_root),
        BenchAction::Run {
            tier,
            case,
            model,
            variant,
            prompt_set,
            no_budget,
            jobs,
        } => {
            let tier = match tier.as_deref() {
                None => None,
                Some(s) => Some(
                    s.parse::<orboros::bench::case::BenchTier>()
                        .map_err(anyhow::Error::msg)?,
                ),
            };
            let project_dir = std::env::current_dir().ok();
            let (cfg, resolved_bench_config) = config::load_config_with_bench(
                project_dir.as_deref(),
                bench_root,
                bench_config_path,
            )?;
            let orboros_commit = project_dir
                .as_deref()
                .and_then(orboros::bench::git_head_commit);
            let orboros_dirty = project_dir
                .as_deref()
                .and_then(orboros::bench::git_is_dirty);
            let bench_commit = orboros::bench::git_head_commit(bench_root);
            let bench_dirty = orboros::bench::git_is_dirty(bench_root);
            let resolver = cfg.model_resolver();
            let resolved_model = if let Some(selector) = model.as_deref() {
                resolver.resolve_selector(selector, "bench --model".to_string())?
            } else {
                resolver.resolve(ModelRole::BenchDefault)?
            };
            let resolved_grader = if model.is_some() {
                resolved_model.model.clone()
            } else {
                resolver
                    .resolve(ModelRole::BenchGrader)
                    .map_or_else(|_| resolved_model.model.clone(), |m| m.model)
            };
            let binary_owned;
            let binary = if let Some(binary) = worker_binary {
                binary
            } else {
                binary_owned = cfg
                    .worker_binary
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("worker_binary is unset in OrbConfig"))?;
                &binary_owned
            };
            let binary = bench_prereq_check(
                Some(binary),
                &resolved_model.model,
                resolved_model.router.as_deref(),
                skip_prereq_check,
            )?;
            let worker_config = make_worker_config(&binary, &resolved_model.model, "");
            let prompt_set = prompt_set
                .as_deref()
                .map(|name| orboros::bench::prompts::BenchPromptSet::load(bench_root, name))
                .transpose()?;
            let run_config = BenchRunConfig {
                variant,
                model_selector: model
                    .clone()
                    .or_else(|| resolved_model.key.clone())
                    .or_else(|| Some(resolved_model.model.clone())),
                model_key: resolved_model.key.clone(),
                worker_model: Some(resolved_model.model.clone()),
                grader_model: Some(resolved_grader),
                prompt_variant: prompt_set.as_ref().map(|set| set.name.clone()),
                prompt_manifest: prompt_set
                    .as_ref()
                    .map(orboros::bench::prompts::BenchPromptSet::manifest),
                suite_manifest: None,
                cases_root: Some(bench_root.display().to_string()),
                bench_config_path: resolved_bench_config
                    .as_ref()
                    .map(|path| path.display().to_string()),
                orboros_commit,
                bench_commit,
                orboros_dirty,
                bench_dirty,
                timeout_s: cfg.bench.timeout_s,
                max_iterations: cfg.bench.max_iterations,
            };
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(bench_cmd::cmd_bench_run(bench_cmd::BenchRunRequest {
                bench_root,
                store: &store,
                tier,
                case_id: case.as_deref(),
                worker_config: &worker_config,
                no_budget,
                jobs: jobs.or(cfg.bench.jobs).unwrap_or(1),
                timeout_s: cfg.bench.timeout_s,
                max_iterations: cfg.bench.max_iterations,
                run_config: &run_config,
                prompt_set: prompt_set.as_ref(),
            }))
        }
        BenchAction::Show { run_id } => bench_cmd::cmd_bench_show(&store, &run_id),
        BenchAction::Details { run_id, case, all } => {
            bench_cmd::cmd_bench_details(&store, &run_id, case.as_deref(), all)
        }
        BenchAction::Report { run_id, case } => {
            bench_cmd::cmd_bench_report(&store, &run_id, case.as_deref())
        }
        BenchAction::ReportHistory => bench_cmd::cmd_bench_report_history(&store),
        BenchAction::Prompts { run_id, case, orb } => {
            bench_cmd::cmd_bench_prompts(&store, &run_id, &case, orb.as_deref())
        }
        BenchAction::Compare { run_a, run_b } => {
            bench_cmd::cmd_bench_compare(&store, &run_a, &run_b)
        }
        BenchAction::ListRuns {
            variant,
            model,
            tier,
            suite,
            prompt_set,
            since,
            limit,
        } => {
            let tier = tier
                .as_deref()
                .map(str::parse::<orboros::bench::case::BenchTier>)
                .transpose()
                .map_err(anyhow::Error::msg)?;
            let since = since
                .as_deref()
                .map(|date| chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d"))
                .transpose()?;
            bench_cmd::cmd_bench_list_runs_filtered(
                &store,
                &bench_cmd::BenchRunFilter {
                    variant: variant.as_deref(),
                    model: model.as_deref(),
                    tier,
                    suite: suite.as_deref(),
                    prompt_set: prompt_set.as_deref(),
                    since,
                    limit,
                },
            )
        }
        BenchAction::Calibration { run_id, buckets } => {
            orboros::bench::calibration::cmd_bench_calibration(&store, &run_id, buckets)
        }
    }
}

fn bench_prereq_check(
    worker_binary: Option<&str>,
    model: &str,
    router: Option<&str>,
    skip: bool,
) -> anyhow::Result<String> {
    prereq_check(worker_binary, model, router, skip)
}

fn cmd_sessions(state_dir: &std::path::Path, action: Option<SessionsAction>) -> anyhow::Result<()> {
    let session_store = orbs::session_store::SessionStore::new(state_dir.join("sessions"));
    match action.unwrap_or(SessionsAction::List { status: None }) {
        SessionsAction::List { status } => {
            let status_filter = match status.as_deref() {
                None => None,
                Some(s) => Some(parse_session_status(s)?),
            };
            orboros::convo::sessions_cmd::cmd_sessions_list(
                &session_store,
                orboros::convo::sessions_cmd::SessionListFilter {
                    status: status_filter,
                },
                std::io::stdout().lock(),
            )?;
            Ok(())
        }
        SessionsAction::Show { id } => {
            orboros::convo::sessions_cmd::cmd_sessions_show_stdout(
                &session_store,
                &orbs::session::SessionId::from_raw(id),
            )?;
            Ok(())
        }
    }
}

fn parse_session_status(s: &str) -> anyhow::Result<orbs::session::SessionStatus> {
    match s.to_ascii_lowercase().as_str() {
        "active" => Ok(orbs::session::SessionStatus::Active),
        "idle" => Ok(orbs::session::SessionStatus::Idle),
        "closed" => Ok(orbs::session::SessionStatus::Closed),
        other => Err(anyhow::anyhow!(
            "unknown session status: {other} (expected active, idle, or closed)"
        )),
    }
}

fn cmd_chat(
    state_dir: &std::path::Path,
    project_dir: Option<&Path>,
    worker_binary: Option<&str>,
    model: Option<&str>,
    system_prompt: &str,
    link_orb: Option<&str>,
    skip_prereq_check: bool,
) -> anyhow::Result<()> {
    let (mut worker_config, _) = resolved_worker_config(
        project_dir,
        worker_binary,
        model,
        config::ModelRole::Chat,
        skip_prereq_check,
    )?;
    worker_config.system_prompt = system_prompt.into();
    let sessions_dir = state_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir)?;
    let session_store = orbs::session_store::SessionStore::new(sessions_dir);

    let init = orbs::session::SessionInit {
        id: orbs::session::SessionId::new(),
        created_at: chrono::Utc::now(),
        model: worker_config.model.clone(),
        system_prompt: Some(system_prompt.into()),
        cwd: std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().into_owned()),
        linked_orb: link_orb.map(orbs::id::OrbId::from_raw),
    };
    let runtime = orboros::convo::ConvoRuntime::new(session_store);
    let orb_store = OrbStore::new(state_dir.join("orbs.jsonl"));

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(orboros::convo::cli::run_chat(
        runtime,
        init,
        worker_config,
        Some(orb_store),
    ))
}

fn foreground_worker_config(
    project_dir: Option<&std::path::Path>,
    worker_binary: Option<&str>,
    model: Option<&str>,
    skip_prereq_check: bool,
) -> anyhow::Result<WorkerConfig> {
    resolved_worker_config(
        project_dir,
        worker_binary,
        model,
        config::ModelRole::Worker("execute"),
        skip_prereq_check,
    )
    .map(|(worker_config, _)| worker_config)
}

fn foreground_queue_with_project(
    state_dir: &std::path::Path,
    project_dir: Option<&std::path::Path>,
) -> QueueLoop {
    let orb_store = OrbStore::new(state_dir.join("orbs.jsonl"));
    let dep_store = DepStore::new(state_dir.join("deps.jsonl"));
    let project_cwd = project_dir
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| state_dir.to_path_buf());
    let queue = QueueLoop::new(orb_store, dep_store, state_dir.to_path_buf());
    if let Some(sink) = orboros::hooks::HookSink::from_state_dir(state_dir, &project_cwd)
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "failed to load hooks; continuing without them");
            None
        })
    {
        queue.with_hooks(sink)
    } else {
        queue
    }
}

fn print_drain_result(result: &DrainResult) {
    if let Some(orb) = result.target.as_ref() {
        println!("Orb:      {}", orb.id);
        println!("Title:    {}", orb.title);
        println!("Status:   {:?}", orb.effective_status());
        println!("Cycles:   {}", result.cycles);
        println!("Workers:  {}", result.workers_completed);
        println!("Reason:   {:?}", result.reason);
        if let Some(response) = orb.result.as_deref() {
            println!();
            println!("{response}");
        }
    } else {
        println!("Orb {} not found.", result.target_id);
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_run_orb(
    state_dir: &std::path::Path,
    project_dir: Option<&std::path::Path>,
    description: &str,
    priority: u8,
    queue_only: bool,
    worker_binary: Option<&str>,
    model: Option<&str>,
    max_ticks: u32,
    interval_ms: u64,
    skip_prereq_check: bool,
) -> anyhow::Result<()> {
    let orb_store = OrbStore::new(state_dir.join("orbs.jsonl"));
    let mut orb = Orb::new(description, description).with_type(OrbType::Task);
    orb.priority = priority;
    orb_store
        .append(&orb)
        .map_err(|e| anyhow::anyhow!("failed to append orb: {e}"))?;
    println!("Created orb {}", orb.id);
    println!("  priority: {}", orb.priority);

    if queue_only {
        println!("  status:   pending (queued)");
        return Ok(());
    }

    let worker_config =
        foreground_worker_config(project_dir, worker_binary, model, skip_prereq_check)?;
    let queue = foreground_queue_with_project(state_dir, project_dir);
    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(queue.drain_target(
        &orb.id,
        &worker_config,
        1,
        true,
        max_ticks,
        std::time::Duration::from_millis(interval_ms),
    ))?;
    print_drain_result(&result);
    if !result.target_terminal() {
        anyhow::bail!("orb did not reach a terminal state: {:?}", result.reason);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_execute_orb(
    state_dir: &std::path::Path,
    project_dir: Option<&std::path::Path>,
    id: &str,
    wait: bool,
    worker_binary: Option<&str>,
    model: Option<&str>,
    max_ticks: u32,
    interval_ms: u64,
    skip_prereq_check: bool,
) -> anyhow::Result<()> {
    let target_id = OrbId::from_raw(id);
    let worker_config =
        foreground_worker_config(project_dir, worker_binary, model, skip_prereq_check)?;
    let queue = foreground_queue_with_project(state_dir, project_dir);
    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(queue.drain_target(
        &target_id,
        &worker_config,
        1,
        wait,
        max_ticks,
        std::time::Duration::from_millis(interval_ms),
    ))?;
    print_drain_result(&result);
    if wait && !result.target_terminal() {
        anyhow::bail!("orb did not reach a terminal state: {:?}", result.reason);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_legacy_run(
    store: &TaskStore,
    worker_binary: Option<&str>,
    model: Option<&str>,
    description: &str,
    priority: u8,
    queue: bool,
    system_prompt: Option<&str>,
    system_prompt_file: Option<&std::path::Path>,
    skip_prereq_check: bool,
) -> anyhow::Result<()> {
    let mut task = Task::new(description, description).with_priority(priority);
    store.append(&task)?;
    println!("Created task {}", task.id);
    println!("  priority: {}", task.priority);

    if queue {
        println!("  status:   pending (queued)");
        return Ok(());
    }

    let project_dir = std::env::current_dir().ok();
    let (mut config, _) = resolved_worker_config(
        project_dir.as_deref(),
        worker_binary,
        model,
        config::ModelRole::Worker("execute"),
        skip_prereq_check,
    )?;
    let default_system_prompt =
        "You are a helpful assistant. Complete the task described in the user message.";
    let resolved_override =
        orboros::prompt::resolve_cli_system_prompt(system_prompt, system_prompt_file)?;
    let resolved_system_prompt = resolved_override
        .as_ref()
        .map_or(default_system_prompt, |resolved| {
            resolved.system_prompt.as_str()
        });
    config.system_prompt = resolved_system_prompt.into();

    println!("  status:   executing...");
    println!();

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        match execute_task(store, &mut task, &config).await {
            Ok(()) => {
                println!("Task completed: {:?}", task.status);
                if let Some(ref result) = task.result {
                    println!();
                    println!("{result}");
                }
            }
            Err(e) => {
                eprintln!("Task failed: {e}");
                if let Some(ref result) = task.result {
                    eprintln!("  detail: {result}");
                }
            }
        }
    });
    Ok(())
}

fn cmd_decompose(
    worker_binary: Option<&str>,
    model: Option<&str>,
    description: &str,
    system_prompt: Option<&str>,
    system_prompt_file: Option<&std::path::Path>,
    skip_prereq_check: bool,
) -> anyhow::Result<()> {
    let project_dir = std::env::current_dir().ok();
    let (config, orb_config) = resolved_worker_config(
        project_dir.as_deref(),
        worker_binary,
        model,
        config::ModelRole::Coordinator("decompose"),
        skip_prereq_check,
    )?;
    let prompt_config = orb_config.prompts;
    let cli_override =
        orboros::prompt::resolve_cli_system_prompt(system_prompt, system_prompt_file)?;
    let prompt_resolver =
        orboros::prompt::PromptResolver::from_config(prompt_config, project_dir.as_deref())
            .with_cli_override(cli_override);

    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(decompose_with_prompt_resolver(
        description,
        &config,
        &prompt_resolver,
    ))?;

    println!("Decomposed into {} subtask(s):\n", result.subtasks.len());
    for (i, sub) in result.subtasks.iter().enumerate() {
        println!(
            "  {}. [{}] {} (order: {})",
            i + 1,
            sub.worker_type,
            sub.title,
            sub.order
        );
        println!("     {}", sub.description);
        if !sub.tools_needed.is_empty() {
            println!("     tools: {}", sub.tools_needed.join(", "));
        }
        println!();
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_orchestrate(
    store: &TaskStore,
    worker_binary: Option<&str>,
    model: Option<&str>,
    description: &str,
    priority: u8,
    system_prompt: Option<&str>,
    system_prompt_file: Option<&std::path::Path>,
    skip_prereq_check: bool,
) -> anyhow::Result<()> {
    let state_dir = store.path().parent();
    let project_dir = state_dir.and_then(project_dir_for_state_dir);
    let (config, orb_config) = resolved_worker_config(
        project_dir.as_deref(),
        worker_binary,
        model,
        config::ModelRole::Coordinator("decompose"),
        skip_prereq_check,
    )?;
    let prompt_config = orb_config.prompts.clone();
    let cli_override =
        orboros::prompt::resolve_cli_system_prompt(system_prompt, system_prompt_file)?;
    let prompt_resolver =
        orboros::prompt::PromptResolver::from_config(prompt_config.clone(), project_dir.as_deref())
            .with_cli_override(cli_override);

    // Create parent task
    let mut parent = Task::new(description, description).with_priority(priority);
    store.append(&parent)?;
    println!("Created parent task {}", parent.id);
    println!();

    let rt = tokio::runtime::Runtime::new()?;

    // Decompose
    println!("Decomposing task...");
    let decomposition = rt.block_on(decompose_with_prompt_resolver(
        description,
        &config,
        &prompt_resolver,
    ))?;
    println!("  → {} subtask(s)\n", decomposition.subtasks.len());

    // Print subtask plan
    for (i, sub) in decomposition.subtasks.iter().enumerate() {
        println!(
            "  {}. [{}] {} (order: {})",
            i + 1,
            sub.worker_type,
            sub.title,
            sub.order
        );
    }
    println!();

    let max_concurrency = orb_config.max_concurrency;
    let orch_config = OrchestrateConfig {
        worker_binary: config.command.clone(),
        worker_args: vec![],
        worker_cwd: None,
        worker_env: vec![],
        tool_profiles: orb_config.tool_profiles.clone(),
        model_config: Some(orb_config),
        worker_default_model: config.model.clone(),
        prompt_resolver,
        max_concurrency,
        context_result_max_chars: CONTEXT_RESULT_MAX_CHARS,
        task_timeout: None,
        budget_limit: None,
    };

    // Run orchestration
    println!("Executing subtasks...");
    let outcome = rt.block_on(orchestrate(
        store,
        &mut parent,
        &decomposition.subtasks,
        &orch_config,
    ))?;

    // Print results
    println!();
    for result in &outcome.subtask_results {
        let status_icon = if result.status == TaskStatus::Done {
            "✓"
        } else {
            "✗"
        };
        println!("  {status_icon} {} — {:?}", result.title, result.status);
        if let Some(ref response) = result.response {
            let preview = if response.len() > 200 {
                format!("{}...", &response[..200])
            } else {
                response.clone()
            };
            println!("    {preview}");
        }
    }
    println!();

    println!("Orchestration complete: {:?}", outcome.parent_status);
    if let Some(ref result) = parent.result {
        let preview = if result.len() > 500 {
            format!("{}...", &result[..500])
        } else {
            result.clone()
        };
        println!();
        println!("{preview}");
    }

    Ok(())
}

fn cmd_tasks(store: &TaskStore, status_filter: Option<&str>) -> anyhow::Result<()> {
    let tasks = if let Some(status_str) = status_filter {
        let status = parse_status(status_str)?;
        store.load_by_status(status)?
    } else {
        store.load_all()?
    };

    if tasks.is_empty() {
        println!("No tasks found.");
    } else {
        for task in &tasks {
            println!(
                "[{:?}] {} — {} (p{})",
                task.status, task.id, task.title, task.priority
            );
        }
        println!("\n{} task(s)", tasks.len());
    }
    Ok(())
}

fn cmd_status(store: &TaskStore, id: &str) -> anyhow::Result<()> {
    let uuid = id.parse::<uuid::Uuid>()?;
    match store.load_by_id(uuid)? {
        Some(task) => {
            println!("Task:     {}", task.id);
            println!("Title:    {}", task.title);
            println!("Status:   {:?}", task.status);
            println!("Priority: {}", task.priority);
            println!("Created:  {}", task.created_at);
            println!("Updated:  {}", task.updated_at);
            if let Some(ref result) = task.result {
                println!("Result:   {result}");
            }
            if let Some(ref model) = task.worker_model {
                println!("Model:    {model}");
            }
            if let Some(parent) = task.parent_id {
                println!("Parent:   {parent}");
            }
        }
        None => {
            println!("Task {id} not found.");
        }
    }
    Ok(())
}

fn cmd_review(store: &TaskStore) -> anyhow::Result<()> {
    let tasks = store.load_by_status(TaskStatus::Review)?;
    if tasks.is_empty() {
        println!("No tasks awaiting review.");
    } else {
        for task in &tasks {
            println!("[Review] {} — {}", task.id, task.title);
            if let Some(ref result) = task.result {
                println!("  Result: {result}");
            }
        }
        println!("\n{} task(s) awaiting review", tasks.len());
    }
    Ok(())
}

fn cmd_plan(
    state_dir: &std::path::Path,
    description: Option<&str>,
    file: Option<&std::path::Path>,
    shallow: bool,
) -> anyhow::Result<()> {
    let config = PlanConfig {
        shallow,
        file: file.map(PathBuf::from),
    };

    let (epic, pipeline) = if let Some(file_path) = file {
        plan::create_plan_from_file(file_path, state_dir, &config)?
    } else if let Some(desc) = description {
        // Use first line as title if multi-line, otherwise use full text as both
        let (title, body) = if let Some((first, rest)) = desc.split_once('\n') {
            (first.trim().to_string(), rest.trim().to_string())
        } else {
            (desc.to_string(), desc.to_string())
        };
        plan::create_plan(&title, &body, state_dir, &config)?
    } else {
        anyhow::bail!("Provide a description or use --file <path>");
    };

    let store = pipeline.orb_store();
    let dep_store = orbs::dep_store::DepStore::new(pipeline.deps_path());

    plan::print_plan_tree(&store, &dep_store, &epic);

    Ok(())
}

fn cmd_daemon_start(
    _store: &TaskStore,
    state_dir: &std::path::Path,
    daemon_config: DaemonConfig,
    project: Option<&str>,
    requested_state_dir: &str,
) -> anyhow::Result<()> {
    if orboros::daemon::is_running(&daemon_config) {
        let pid = orboros::daemon::read_pid_file(&daemon_config)?;
        anyhow::bail!(
            "Daemon is already running (PID {}). Use --stop first.",
            pid.unwrap_or(0)
        );
    }

    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    let supervisor_mode = project.is_some() || requested_state_dir == DEFAULT_STATE_DIR;
    if supervisor_mode {
        return cmd_supervisor_start(&home, daemon_config, project);
    }

    println!(
        "Starting single-project daemon for {}...",
        state_dir.display()
    );

    let orb_store = orbs::orb_store::OrbStore::new(state_dir.join("orbs.jsonl"));
    let dep_store = orbs::dep_store::DepStore::new(state_dir.join("deps.jsonl"));
    let mut queue =
        orboros::queue_loop::QueueLoop::new(orb_store, dep_store, state_dir.to_path_buf());

    // Attach HookSink so the daemon fires lifecycle hooks (closes
    // task 56 follow-up: daemon-side QueueLoop::with_hooks plumbing).
    if let Some(sink) = orboros::hooks::HookSink::from_state_dir(state_dir, state_dir)? {
        queue = queue.with_hooks(sink);
    }

    // Build a base WorkerConfig if the project config has a
    // worker_binary. When absent, the daemon stays pure
    // state-machine — workers never spawn. Lets users opt in
    // to autonomous dispatch without making it mandatory.
    let project_max_concurrency = config::load_config(Some(state_dir))?.max_concurrency;
    let dispatch = match orboros::worker::dispatcher::default_worker_config(
        dirs::home_dir().as_deref(),
        Some(state_dir),
    ) {
        Ok(base_worker_config) => Some(orboros::daemon::DispatchSettings {
            base_worker_config,
            max_concurrency: project_max_concurrency,
        }),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "daemon starting without dispatch — worker_binary unconfigured"
            );
            println!("  note: dispatch disabled (worker_binary unconfigured: {e})");
            None
        }
    };

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(orboros::daemon::run_daemon(daemon_config, queue, dispatch))?;

    Ok(())
}

fn cmd_supervisor_start(
    home: &Path,
    daemon_config: DaemonConfig,
    selected_project: Option<&str>,
) -> anyhow::Result<()> {
    use orboros::daemon::SupervisedProject;
    let (registered, skipped) = config::registered_project_state_dirs(home)?;
    for project in skipped {
        tracing::warn!(project = %project.name, path = %project.path.display(), "skipping missing or uninitialized registered project");
        println!(
            "  warning: skipping {} (missing {})",
            project.name,
            project.path.join(".orbs").display()
        );
    }
    let registered: Vec<_> = registered
        .into_iter()
        .filter(|project| selected_project.is_none_or(|name| project.entry.name == name))
        .collect();
    if registered.is_empty() {
        anyhow::bail!("No initialized registered projects to supervise");
    }
    if let Some(name) = selected_project {
        if registered.len() != 1 {
            anyhow::bail!("Registered project {name:?} was not initialized");
        }
    }

    let mut projects = Vec::with_capacity(registered.len());
    for project in registered {
        let entry = project.entry;
        let project_state_dir = project.state_dir;
        let orb_store = OrbStore::new(project_state_dir.join("orbs.jsonl"));
        let dep_store = DepStore::new(project_state_dir.join("deps.jsonl"));
        let mut queue = QueueLoop::new(orb_store, dep_store, project_state_dir.clone());
        if let Some(sink) =
            orboros::hooks::HookSink::from_state_dir(&project_state_dir, &entry.path)?
        {
            queue = queue.with_hooks(sink);
        }
        let project_max_concurrency =
            config::load_config(Some(&project_state_dir))?.max_concurrency;
        let dispatch = match orboros::worker::dispatcher::default_worker_config(
            Some(home),
            Some(&project_state_dir),
        ) {
            Ok(base_worker_config) => Some(orboros::daemon::DispatchSettings {
                base_worker_config,
                max_concurrency: project_max_concurrency,
            }),
            Err(error) => {
                tracing::warn!(project = %entry.name, %error, "project dispatch disabled — worker_binary unconfigured");
                None
            }
        };
        projects.push(SupervisedProject {
            name: entry.name,
            queue,
            dispatch,
        });
    }
    println!(
        "Starting supervisor for {} registered project(s)...",
        projects.len()
    );
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(orboros::daemon::run_supervisor(daemon_config, projects))
}

fn cmd_daemon_stop(daemon_config: &DaemonConfig) -> anyhow::Result<()> {
    match orboros::daemon::read_pid_file(daemon_config)? {
        Some(pid) => {
            if orboros::daemon::is_running(daemon_config) {
                println!("Sending SIGTERM to daemon (PID {pid})...");
                // Safety: sending SIGTERM to a known PID
                unsafe {
                    libc::kill(pid.cast_signed(), libc::SIGTERM);
                }
                println!("Stop signal sent.");
            } else {
                println!("Daemon is not running (stale PID file). Cleaning up.");
                orboros::daemon::remove_pid_file(daemon_config)?;
            }
        }
        None => {
            println!("No daemon is running (no PID file found).");
        }
    }
    Ok(())
}

fn cmd_daemon_status(daemon_config: &DaemonConfig, home: Option<&Path>) -> anyhow::Result<()> {
    let project_count = home
        .and_then(|home| config::registered_project_state_dirs(home).ok())
        .map_or(0, |(projects, _)| projects.len());
    if orboros::daemon::is_running(daemon_config) {
        let pid = orboros::daemon::read_pid_file(daemon_config)?;
        println!(
            "Daemon is running (PID {}). Supervising {project_count} registered project(s).",
            pid.unwrap_or(0)
        );
    } else if daemon_config.pid_file.exists() {
        println!(
            "Daemon is not running (stale PID file at {}).",
            daemon_config.pid_file.display()
        );
    } else {
        println!("Daemon is not running. {project_count} registered project(s) available.");
    }
    Ok(())
}
fn cmd_init() -> anyhow::Result<()> {
    let project_dir = std::env::current_dir()?;
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;

    config::init_project(&home, &project_dir)?;

    println!("Initialized orboros project in {}", project_dir.display());
    println!("  Created .orbs/config.toml");
    println!("  Created .orbs/orbs.jsonl");
    println!(
        "  Registered project \"{}\" in ~/.orboros/projects.toml",
        project_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unnamed")
    );
    Ok(())
}

fn config_target(global: bool, project_dir: Option<&Path>) -> anyhow::Result<PathBuf> {
    if global {
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?;
        return Ok(home.join(".orboros/config.toml"));
    }
    let project = project_dir
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| anyhow::anyhow!("could not determine project directory"))?;
    Ok(project.join(".orbs/config.toml"))
}

fn cmd_config(action: ConfigAction, project_dir: Option<&Path>) -> anyhow::Result<()> {
    match action {
        ConfigAction::Init {
            global,
            force,
            minimal,
        } => {
            let target = config_target(global, project_dir)?;
            if target.exists() && !force {
                anyhow::bail!(
                    "config already exists: {} (use --force to replace it)",
                    target.display()
                );
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&target, config::starter_config_template(minimal))?;
            println!(
                "Wrote {} config: {}",
                if minimal { "minimal" } else { "starter" },
                target.display()
            );
            Ok(())
        }
        ConfigAction::Upgrade { global, apply } => {
            let target = config_target(global, project_dir)?;
            let content = std::fs::read_to_string(&target)
                .map_err(|e| anyhow::anyhow!("reading {}: {e}", target.display()))?;
            let (mut upgraded, mut added, examples) = config::upgrade_config_toml(&content)?;

            if !global {
                let routing = target.with_file_name("routing.toml");
                if routing.exists() {
                    let legacy = orboros::routing::rules::parse_routing_config(
                        &std::fs::read_to_string(&routing)?,
                    )?;
                    if !legacy.profiles.is_empty() {
                        let mut table: toml::value::Table = toml::from_str(&upgraded)?;
                        let profiles = table
                            .entry("tool_profiles")
                            .or_insert_with(|| toml::Value::Table(Default::default()));
                        let profiles = profiles
                            .as_table_mut()
                            .ok_or_else(|| anyhow::anyhow!("tool_profiles must be a TOML table"))?;
                        for (name, profile) in legacy.profiles {
                            if !profiles.contains_key(&name) {
                                profiles.insert(name.clone(), toml::Value::try_from(profile)?);
                                added.push(format!(
                                    "tool_profiles.{name} (imported from routing.toml)"
                                ));
                            }
                        }
                        upgraded = toml::to_string_pretty(&table)?;
                    }
                    println!("note: legacy routing model rules are not migrated; configure [models] instead");
                }
            }
            if added.is_empty() {
                println!("{} is already current", target.display());
            } else {
                println!("{} would add:\n  {}", target.display(), added.join("\n  "));
            }
            if !examples.is_empty() {
                println!("New optional config fields (not written automatically):");
                for example in examples {
                    println!("\n{}\n# {}", example.toml, example.description);
                }
            }
            if apply && !added.is_empty() {
                std::fs::write(&target, upgraded)?;
                println!("Applied config upgrade.");
            } else if !apply && !added.is_empty() {
                println!("Dry run only; re-run with --apply to write changes.");
            }
            Ok(())
        }
        ConfigAction::Show => {
            let cfg = config::load_config(project_dir)?;
            let resolver = cfg.model_resolver();
            let worker = resolver.resolve(config::ModelRole::Worker("execute"))?;
            let chat = resolver.resolve(config::ModelRole::Chat)?;
            println!("config_version: {}", cfg.config_version);
            println!(
                "worker_binary: {}",
                cfg.worker_binary.as_deref().unwrap_or("<unset>")
            );
            println!("max_concurrency: {}", cfg.max_concurrency);
            println!("worker model: {} ({})", worker.model, worker.source);
            println!("chat model: {} ({})", chat.model, chat.source);
            Ok(())
        }
    }
}

fn parse_status(s: &str) -> anyhow::Result<TaskStatus> {
    match s.to_lowercase().as_str() {
        "pending" => Ok(TaskStatus::Pending),
        "active" => Ok(TaskStatus::Active),
        "review" => Ok(TaskStatus::Review),
        "done" => Ok(TaskStatus::Done),
        "failed" => Ok(TaskStatus::Failed),
        other => {
            anyhow::bail!("unknown status: {other}. Use: pending, active, review, done, failed")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_legacy_tasks_namespace() {
        let cli = Cli::parse_from(["orboros", "legacy", "tasks", "--status", "done"]);

        match cli.command {
            Commands::Legacy {
                action: LegacyAction::Tasks { status },
            } => assert_eq!(status.as_deref(), Some("done")),
            _ => panic!("expected legacy tasks command"),
        }
    }

    #[test]
    fn parses_legacy_run_namespace() {
        let cli = Cli::parse_from([
            "orboros",
            "--model",
            "openrouter/free",
            "legacy",
            "run",
            "do the thing",
            "--priority",
            "2",
            "--queue",
        ]);

        match cli.command {
            Commands::Legacy {
                action:
                    LegacyAction::Run {
                        task,
                        priority,
                        queue,
                        ..
                    },
            } => {
                assert_eq!(task, "do the thing");
                assert_eq!(priority, 2);
                assert!(queue);
            }
            _ => panic!("expected legacy run command"),
        }
    }

    #[test]
    fn top_level_run_parses_as_orb_backed_command() {
        let cli = Cli::parse_from([
            "orboros",
            "run",
            "do the thing",
            "--priority",
            "2",
            "--max-ticks",
            "3",
        ]);

        match cli.command {
            Commands::Run {
                task,
                priority,
                queue,
                max_ticks,
                ..
            } => {
                assert_eq!(task, "do the thing");
                assert_eq!(priority, 2);
                assert!(!queue);
                assert_eq!(max_ticks, 3);
            }
            _ => panic!("expected top-level orb-backed run command"),
        }
    }

    #[test]
    fn parses_execute_wait_command() {
        let cli = Cli::parse_from(["orboros", "execute", "orb-k4f", "--wait"]);

        match cli.command {
            Commands::Execute { id, wait, .. } => {
                assert_eq!(id, "orb-k4f");
                assert!(wait);
            }
            _ => panic!("expected execute command"),
        }
    }

    #[test]
    fn finds_orbs_dir_in_ancestor_before_home() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let nested = project.join("a/b/c");
        std::fs::create_dir_all(project.join(".orbs")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();

        let found = find_orbs_dir_upwards(&nested, Some(tmp.path()));

        assert_eq!(found.as_deref(), Some(project.join(".orbs").as_path()));
    }

    #[test]
    fn ignores_orbs_dir_at_home_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("work/project");
        std::fs::create_dir_all(tmp.path().join(".orbs")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();

        let found = find_orbs_dir_upwards(&nested, Some(tmp.path()));

        assert!(found.is_none());
    }

    #[test]
    fn explicit_orbs_state_dir_sets_project_root() {
        let state = PathBuf::from("/tmp/project/.orbs");

        assert_eq!(
            project_dir_for_state_dir(&state).as_deref(),
            Some(Path::new("/tmp/project"))
        );
    }

    #[test]
    fn top_level_tasks_remains_compat_alias() {
        let cli = Cli::parse_from(["orboros", "tasks"]);

        match cli.command {
            Commands::Tasks { status } => assert!(status.is_none()),
            _ => panic!("expected top-level tasks compatibility alias"),
        }
    }
}

#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use orbs::dep_store::DepStore;
use orbs::orb_store::OrbStore;

use orboros::config;
use orboros::coordinator::decompose::decompose_with_prompt_resolver;
use orboros::daemon::DaemonConfig;
use orboros::orb_cmd;
use orboros::orchestrator::{orchestrate, OrchestrateConfig, CONTEXT_RESULT_MAX_CHARS};
use orboros::plan::{self, PlanConfig};
use orboros::routing::rules::RoutingConfig;
use orboros::runner::execute_task;
use orboros::state::store::TaskStore;
use orboros::state::task::{Task, TaskStatus};
use orboros::worker::process::WorkerConfig;

/// Orboros — multi-agent orchestrator.
#[derive(Parser)]
#[command(name = "orboros", version, about)]
struct Cli {
    /// Path to the project state directory.
    #[arg(long, default_value = "~/.orboros/default")]
    state_dir: String,

    /// Path to the heddle-headless binary.
    #[arg(long, env = "HEDDLE_BINARY")]
    worker_binary: Option<String>,

    /// Model to use for workers.
    #[arg(long, default_value = "openrouter/free")]
    model: String,

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
    /// Submit a new task for execution.
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
    /// Decompose a task into subtasks without executing.
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
    /// Decompose a task and execute all subtasks.
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
    /// List tasks, optionally filtered by status.
    Tasks {
        /// Filter by status (pending, active, review, done, failed).
        #[arg(short, long)]
        status: Option<String>,
    },
    /// Show status of a specific task by ID.
    Status {
        /// Task ID (UUID).
        id: String,
    },
    /// List tasks awaiting review.
    Review,
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
        /// Root containing cases/, fixtures/, prompts/, and results/.
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
        /// Skip the per-case cost ceiling (`max_cost_cents`).
        #[arg(long)]
        no_budget: bool,
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
    /// Diff two saved runs by case outcome.
    Compare { run_a: String, run_b: String },
    /// List every recorded run.
    ListRuns,
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

fn require_binary(worker_binary: Option<&str>) -> anyhow::Result<&str> {
    worker_binary.ok_or_else(|| {
        anyhow::anyhow!("No worker binary configured. Set --worker-binary or HEDDLE_BINARY.")
    })
}

/// Combined entry point used by every worker-spawning command. Resolves
/// the worker binary (errors if unset), then runs `validate_worker_prereqs`
/// unless `skip_prereq_check` is true.
///
/// Returns the borrowed binary path for the caller to keep using.
fn prereq_check<'a>(
    worker_binary: Option<&'a str>,
    model: &str,
    skip: bool,
) -> anyhow::Result<&'a str> {
    let binary = require_binary(worker_binary)?;
    if skip {
        tracing::warn!("--skip-prereq-check set; trusting caller for binary/model/credentials");
        return Ok(binary);
    }
    orboros::startup_check::validate_worker_prereqs(&orboros::startup_check::PrereqCheck {
        worker_binary: binary,
        model,
        require_credentials: true,
    })?;
    Ok(binary)
}

fn load_routing_config(state_dir: Option<&std::path::Path>) -> RoutingConfig {
    if let Some(dir) = state_dir {
        let config_path = dir.join("routing.toml");
        if let Ok(content) = std::fs::read_to_string(&config_path) {
            match orboros::routing::rules::parse_routing_config(&content) {
                Ok(config) => {
                    tracing::info!("Loaded routing config from {}", config_path.display());
                    return config;
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to parse routing config at {}: {e}",
                        config_path.display()
                    );
                }
            }
        }
    }
    RoutingConfig::default()
}

fn make_worker_config(binary: &str, model: &str, system_prompt: &str) -> WorkerConfig {
    WorkerConfig {
        command: binary.into(),
        args: vec![],
        cwd: None,
        env: vec![],
        model: model.into(),
        system_prompt: system_prompt.into(),
        tools: vec![],
        max_iterations: None,
        init_timeout: None,
        send_timeout: None,
        shutdown_timeout: None,
        task_id: None,
        worker_id: None,
    }
}

#[allow(clippy::too_many_lines)]
fn main() -> anyhow::Result<()> {
    // Load .env from current dir or ancestors (silently ignore if missing)
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("orboros=info,heddle=warn")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let state_dir = resolve_state_dir(&cli.state_dir);
    std::fs::create_dir_all(&state_dir)?;
    let store = TaskStore::new(state_dir.join("tasks.jsonl"));

    match cli.command {
        Commands::Run {
            task,
            priority,
            queue,
            system_prompt,
            system_prompt_file,
        } => cmd_run(
            &store,
            cli.worker_binary.as_deref(),
            &cli.model,
            &task,
            priority,
            queue,
            system_prompt.as_deref(),
            system_prompt_file.as_deref(),
            cli.skip_prereq_check,
        ),
        Commands::Decompose {
            task,
            system_prompt,
            system_prompt_file,
        } => cmd_decompose(
            cli.worker_binary.as_deref(),
            &cli.model,
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
            &cli.model,
            &task,
            priority,
            system_prompt.as_deref(),
            system_prompt_file.as_deref(),
            cli.skip_prereq_check,
        ),
        Commands::Tasks { status } => cmd_tasks(&store, status.as_deref()),
        Commands::Status { id } => cmd_status(&store, &id),
        Commands::Review => cmd_review(&store),
        Commands::Plan {
            description,
            file,
            shallow,
        } => cmd_plan(&state_dir, description.as_deref(), file.as_deref(), shallow),
        Commands::Init => cmd_init(),
        Commands::Daemon {
            stop,
            status,
            pid_file,
            log_file,
            tick_interval,
        } => {
            let mut daemon_config = DaemonConfig::default();
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
                cmd_daemon_status(&daemon_config)
            } else {
                cmd_daemon_start(&store, &state_dir, daemon_config)
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
                    orb_cmd::cmd_orb_update(
                        &orb_store,
                        &id,
                        title.as_deref(),
                        description.as_deref(),
                        priority,
                        status.as_deref(),
                        confidence,
                        label_edits,
                        hooks_ref,
                    )
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
            cli.worker_binary.as_deref(),
            chat_model.as_deref().unwrap_or(&cli.model),
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
    let cases_root = bench_root.join("cases");
    let fixtures_root = bench_root.join("fixtures");
    let bench_dir =
        bench_results_dir.map_or_else(|| bench_root.join("results"), std::path::Path::to_path_buf);
    let store = BenchStore::new(&bench_dir);

    match action {
        BenchAction::List => bench_cmd::cmd_bench_list(&cases_root),
        BenchAction::Run {
            tier,
            case,
            model,
            variant,
            no_budget,
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
            let bench_commit = orboros::bench::git_head_commit(bench_root);
            let resolver = cfg.model_resolver();
            let resolved_model = if let Some(selector) = model.as_deref() {
                resolver.resolve_selector(selector, "bench --model".to_string())?
            } else {
                resolver.resolve(ModelRole::BenchDefault)?
            };
            let resolved_model = normalize_bench_model_for_heddle(resolved_model);
            let resolved_grader = if model.is_some() {
                resolved_model.model.clone()
            } else {
                resolver
                    .resolve(ModelRole::BenchGrader)
                    .map(normalize_bench_model_for_heddle)
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
            bench_prereq_check(Some(binary), &resolved_model.model, skip_prereq_check)?;
            let worker_config = make_worker_config(binary, &resolved_model.model, "");
            let run_config = BenchRunConfig {
                variant,
                model_selector: model
                    .clone()
                    .or_else(|| resolved_model.key.clone())
                    .or_else(|| Some(resolved_model.model.clone())),
                model_key: resolved_model.key.clone(),
                worker_model: Some(resolved_model.model.clone()),
                grader_model: Some(resolved_grader),
                prompt_variant: None,
                cases_root: Some(cases_root.display().to_string()),
                bench_config_path: resolved_bench_config
                    .as_ref()
                    .map(|path| path.display().to_string()),
                orboros_commit,
                bench_commit,
                timeout_s: cfg.bench.timeout_s,
                max_iterations: cfg.bench.max_iterations,
            };
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(bench_cmd::cmd_bench_run(bench_cmd::BenchRunRequest {
                cases_root: &cases_root,
                store: &store,
                tier,
                case_id: case.as_deref(),
                worker_config: &worker_config,
                no_budget,
                timeout_s: cfg.bench.timeout_s,
                max_iterations: cfg.bench.max_iterations,
                run_config: &run_config,
                fixtures_root: &fixtures_root,
            }))
        }
        BenchAction::Show { run_id } => bench_cmd::cmd_bench_show(&store, &run_id),
        BenchAction::Details { run_id, case, all } => {
            bench_cmd::cmd_bench_details(&store, &run_id, case.as_deref(), all)
        }
        BenchAction::Compare { run_a, run_b } => {
            bench_cmd::cmd_bench_compare(&store, &run_a, &run_b)
        }
        BenchAction::ListRuns => bench_cmd::cmd_bench_list_runs(&store),
        BenchAction::Calibration { run_id, buckets } => {
            orboros::bench::calibration::cmd_bench_calibration(&store, &run_id, buckets)
        }
    }
}

fn normalize_bench_model_for_heddle(
    mut resolved: orboros::config::ResolvedModel,
) -> orboros::config::ResolvedModel {
    if let Some(model) = resolved.model.strip_prefix("openrouter/") {
        resolved.model = model.to_string();
        resolved.router = Some("openrouter".into());
    }
    resolved
}

fn bench_prereq_check<'a>(
    worker_binary: Option<&'a str>,
    model: &str,
    skip: bool,
) -> anyhow::Result<&'a str> {
    let binary = require_binary(worker_binary)?;
    if skip {
        tracing::warn!("--skip-prereq-check set; trusting caller for binary/model/credentials");
        return Ok(binary);
    }
    orboros::startup_check::check_binary(binary)?;
    orboros::startup_check::check_model_string(model)?;
    if std::env::var("OPENROUTER_API_KEY").map_or(true, |s| s.trim().is_empty()) {
        anyhow::bail!(
            "missing credentials for bench OpenRouter route: set OPENROUTER_API_KEY \
             (looked at .env and process env)"
        );
    }
    Ok(binary)
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
    worker_binary: Option<&str>,
    model: &str,
    system_prompt: &str,
    link_orb: Option<&str>,
    skip_prereq_check: bool,
) -> anyhow::Result<()> {
    let binary = prereq_check(worker_binary, model, skip_prereq_check)?;
    let sessions_dir = state_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir)?;
    let session_store = orbs::session_store::SessionStore::new(sessions_dir);

    let init = orbs::session::SessionInit {
        id: orbs::session::SessionId::new(),
        created_at: chrono::Utc::now(),
        model: model.into(),
        system_prompt: Some(system_prompt.into()),
        cwd: std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().into_owned()),
        linked_orb: link_orb.map(orbs::id::OrbId::from_raw),
    };
    let worker_config = make_worker_config(binary, model, system_prompt);
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

#[allow(clippy::too_many_arguments)]
fn cmd_run(
    store: &TaskStore,
    worker_binary: Option<&str>,
    model: &str,
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

    let binary = prereq_check(worker_binary, model, skip_prereq_check)?;
    let default_system_prompt =
        "You are a helpful assistant. Complete the task described in the user message.";
    let resolved_override =
        orboros::prompt::resolve_cli_system_prompt(system_prompt, system_prompt_file)?;
    let resolved_system_prompt = resolved_override
        .as_ref()
        .map_or(default_system_prompt, |resolved| {
            resolved.system_prompt.as_str()
        });
    let config = make_worker_config(binary, model, resolved_system_prompt);

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
    model: &str,
    description: &str,
    system_prompt: Option<&str>,
    system_prompt_file: Option<&std::path::Path>,
    skip_prereq_check: bool,
) -> anyhow::Result<()> {
    let binary = prereq_check(worker_binary, model, skip_prereq_check)?;
    let config = make_worker_config(binary, model, ""); // system prompt set by decompose()
    let prompt_config = config::load_config(None)?.prompts;
    let cli_override =
        orboros::prompt::resolve_cli_system_prompt(system_prompt, system_prompt_file)?;
    let prompt_resolver = orboros::prompt::PromptResolver::from_config(prompt_config, None)
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
    model: &str,
    description: &str,
    priority: u8,
    system_prompt: Option<&str>,
    system_prompt_file: Option<&std::path::Path>,
    skip_prereq_check: bool,
) -> anyhow::Result<()> {
    let binary = prereq_check(worker_binary, model, skip_prereq_check)?;
    let config = make_worker_config(binary, model, ""); // system prompt set per step
    let project_dir = store.path().parent();
    let orb_config = config::load_config(project_dir)?;
    let prompt_config = orb_config.prompts.clone();
    let cli_override =
        orboros::prompt::resolve_cli_system_prompt(system_prompt, system_prompt_file)?;
    let prompt_resolver =
        orboros::prompt::PromptResolver::from_config(prompt_config.clone(), project_dir)
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

    // Load routing config and build orchestrate config
    let routing = load_routing_config(project_dir);
    let orch_config = OrchestrateConfig {
        worker_binary: binary.to_string(),
        worker_args: vec![],
        worker_cwd: None,
        worker_env: vec![],
        routing,
        model_config: Some(orb_config),
        worker_default_model: model.to_string(),
        prompt_resolver,
        max_concurrency: 4,
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
) -> anyhow::Result<()> {
    if orboros::daemon::is_running(&daemon_config) {
        let pid = orboros::daemon::read_pid_file(&daemon_config)?;
        anyhow::bail!(
            "Daemon is already running (PID {}). Use --stop first.",
            pid.unwrap_or(0)
        );
    }

    println!("Starting daemon...");

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
    let dispatch = match orboros::worker::dispatcher::default_worker_config(
        dirs::home_dir().as_deref(),
        Some(state_dir),
    ) {
        Ok(base_worker_config) => Some(orboros::daemon::DispatchSettings {
            base_worker_config,
            max_concurrency: daemon_config.max_concurrency,
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

fn cmd_daemon_status(daemon_config: &DaemonConfig) -> anyhow::Result<()> {
    if orboros::daemon::is_running(daemon_config) {
        let pid = orboros::daemon::read_pid_file(daemon_config)?;
        println!("Daemon is running (PID {}).", pid.unwrap_or(0));
    } else if daemon_config.pid_file.exists() {
        println!(
            "Daemon is not running (stale PID file at {}).",
            daemon_config.pid_file.display()
        );
    } else {
        println!("Daemon is not running.");
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

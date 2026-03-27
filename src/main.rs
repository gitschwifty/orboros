#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use orboros::coordinator::decompose::decompose;
use orboros::orchestrator::{orchestrate, OrchestrateConfig, CONTEXT_RESULT_MAX_CHARS};
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
    },
    /// Decompose a task into subtasks without executing.
    Decompose {
        /// The high-level task to decompose.
        task: String,
    },
    /// Decompose a task and execute all subtasks.
    Orchestrate {
        /// The high-level task to orchestrate.
        task: String,
        /// Priority for subtasks (1=highest, 5=lowest).
        #[arg(short, long, default_value = "3")]
        priority: u8,
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
    }
}

fn main() -> anyhow::Result<()> {
    // Load .env from current dir or ancestors (silently ignore if missing)
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("orboros=info")),
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
        } => cmd_run(
            &store,
            cli.worker_binary.as_deref(),
            &cli.model,
            &task,
            priority,
            queue,
        ),
        Commands::Decompose { task } => {
            cmd_decompose(cli.worker_binary.as_deref(), &cli.model, &task)
        }
        Commands::Orchestrate { task, priority } => cmd_orchestrate(
            &store,
            cli.worker_binary.as_deref(),
            &cli.model,
            &task,
            priority,
        ),
        Commands::Tasks { status } => cmd_tasks(&store, status.as_deref()),
        Commands::Status { id } => cmd_status(&store, &id),
        Commands::Review => cmd_review(&store),
    }
}

fn cmd_run(
    store: &TaskStore,
    worker_binary: Option<&str>,
    model: &str,
    description: &str,
    priority: u8,
    queue: bool,
) -> anyhow::Result<()> {
    let mut task = Task::new(description, description).with_priority(priority);
    store.append(&task)?;
    println!("Created task {}", task.id);
    println!("  priority: {}", task.priority);

    if queue {
        println!("  status:   pending (queued)");
        return Ok(());
    }

    let binary = require_binary(worker_binary)?;
    let config = make_worker_config(
        binary,
        model,
        "You are a helpful assistant. Complete the task described in the user message.",
    );

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
) -> anyhow::Result<()> {
    let binary = require_binary(worker_binary)?;
    let config = make_worker_config(binary, model, ""); // system prompt set by decompose()

    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(decompose(description, &config))?;

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

fn cmd_orchestrate(
    store: &TaskStore,
    worker_binary: Option<&str>,
    model: &str,
    description: &str,
    priority: u8,
) -> anyhow::Result<()> {
    let binary = require_binary(worker_binary)?;
    let config = make_worker_config(binary, model, ""); // system prompt set per step

    // Create parent task
    let mut parent = Task::new(description, description).with_priority(priority);
    store.append(&parent)?;
    println!("Created parent task {}", parent.id);
    println!();

    let rt = tokio::runtime::Runtime::new()?;

    // Decompose
    println!("Decomposing task...");
    let decomposition = rt.block_on(decompose(description, &config))?;
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
    let routing = load_routing_config(store.path().parent());
    let orch_config = OrchestrateConfig {
        worker_binary: binary.to_string(),
        worker_args: vec![],
        worker_cwd: None,
        worker_env: vec![],
        routing,
        max_concurrency: 4,
        context_result_max_chars: CONTEXT_RESULT_MAX_CHARS,
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

#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use orboros::state::store::TaskStore;
use orboros::state::task::{Task, TaskStatus};

/// Orboros — multi-agent orchestrator.
#[derive(Parser)]
#[command(name = "orboros", version, about)]
struct Cli {
    /// Path to the project state directory.
    #[arg(long, default_value = "~/.orboros/default")]
    state_dir: String,

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

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let state_dir = resolve_state_dir(&cli.state_dir);
    std::fs::create_dir_all(&state_dir)?;
    let store = TaskStore::new(state_dir.join("tasks.jsonl"));

    match cli.command {
        Commands::Run { task, priority } => {
            let task = Task::new(&task, &task).with_priority(priority);
            store.append(&task)?;
            println!("Created task {}", task.id);
            println!("  title:    {}", task.title);
            println!("  priority: {}", task.priority);
            println!("  status:   pending");
            println!();
            println!("Task queued. Worker execution not yet implemented.");
        }
        Commands::Tasks { status } => {
            let tasks = if let Some(status_str) = &status {
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
        }
        Commands::Status { id } => {
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
        }
        Commands::Review => {
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
        }
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

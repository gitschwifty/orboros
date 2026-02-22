use std::path::{Path, PathBuf};

use crate::state::task::{Task, TaskStatus};

/// Append-only JSONL task store.
///
/// Each task mutation is appended as a full JSON line. Reading replays
/// the log and keeps the latest version of each task (by ID).
pub struct TaskStore {
    path: PathBuf,
}

impl TaskStore {
    /// Opens or creates a JSONL store at the given path.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Returns the path to the store file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends a task to the store.
    ///
    /// # Errors
    ///
    /// Returns an IO error if writing fails.
    pub fn append(&self, task: &Task) -> std::io::Result<()> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let mut line = serde_json::to_string(task)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        file.write_all(line.as_bytes())?;
        Ok(())
    }

    /// Reads all tasks from the store, deduplicating by ID (latest wins).
    ///
    /// # Errors
    ///
    /// Returns an IO error if reading fails, or if any line is invalid JSON.
    pub fn load_all(&self) -> std::io::Result<Vec<Task>> {
        use std::collections::HashMap;
        use std::io::BufRead;

        if !self.path.exists() {
            return Ok(vec![]);
        }

        let file = std::fs::File::open(&self.path)?;
        let reader = std::io::BufReader::new(file);
        let mut tasks: HashMap<uuid::Uuid, Task> = HashMap::new();

        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let task: Task = serde_json::from_str(&line)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            tasks.insert(task.id, task);
        }

        let mut result: Vec<Task> = tasks.into_values().collect();
        result.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(result)
    }

    /// Loads tasks filtered by status.
    ///
    /// # Errors
    ///
    /// Returns an IO error if reading fails.
    pub fn load_by_status(&self, status: TaskStatus) -> std::io::Result<Vec<Task>> {
        Ok(self
            .load_all()?
            .into_iter()
            .filter(|t| t.status == status)
            .collect())
    }

    /// Loads a single task by ID, or None if not found.
    ///
    /// # Errors
    ///
    /// Returns an IO error if reading fails.
    pub fn load_by_id(&self, id: uuid::Uuid) -> std::io::Result<Option<Task>> {
        Ok(self.load_all()?.into_iter().find(|t| t.id == id))
    }

    /// Updates a task by appending its new state. The caller is responsible
    /// for mutating the task before calling this.
    ///
    /// # Errors
    ///
    /// Returns an IO error if writing fails.
    pub fn update(&self, task: &Task) -> std::io::Result<()> {
        self.append(task)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_load_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));

        let task1 = Task::new("Task one", "First task");
        let task2 = Task::new("Task two", "Second task");
        store.append(&task1).unwrap();
        store.append(&task2).unwrap();

        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].title, "Task one");
        assert_eq!(loaded[1].title, "Task two");
    }

    #[test]
    fn update_deduplicates_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));

        let mut task = Task::new("Test task", "description");
        store.append(&task).unwrap();

        task.transition(TaskStatus::Active);
        store.update(&task).unwrap();

        task.transition(TaskStatus::Done);
        task.result = Some("completed successfully".into());
        store.update(&task).unwrap();

        // File has 3 lines, but load_all deduplicates to 1 task
        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].status, TaskStatus::Done);
        assert_eq!(loaded[0].result.as_deref(), Some("completed successfully"));
    }

    #[test]
    fn filter_by_status() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));

        let mut task1 = Task::new("Pending task", "waiting");
        store.append(&task1).unwrap();

        let mut task2 = Task::new("Active task", "working");
        task2.transition(TaskStatus::Active);
        store.append(&task2).unwrap();

        let mut task3 = Task::new("Done task", "finished");
        task3.transition(TaskStatus::Done);
        store.append(&task3).unwrap();

        let pending = store.load_by_status(TaskStatus::Pending).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].title, "Pending task");

        let active = store.load_by_status(TaskStatus::Active).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].title, "Active task");

        // Transition task1 to active, update store
        task1.transition(TaskStatus::Active);
        store.update(&task1).unwrap();

        let active = store.load_by_status(TaskStatus::Active).unwrap();
        assert_eq!(active.len(), 2);
    }

    #[test]
    fn load_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));

        let task = Task::new("Find me", "test");
        let target_id = task.id;
        store.append(&task).unwrap();
        store.append(&Task::new("Other", "task")).unwrap();

        let found = store.load_by_id(target_id).unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().title, "Find me");

        let missing = store.load_by_id(uuid::Uuid::new_v4()).unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn empty_store_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("nonexistent.jsonl"));
        let loaded = store.load_all().unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn priority_ordering_in_subtasks() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskStore::new(dir.path().join("tasks.jsonl"));

        let parent = Task::new("Parent task", "orchestrate");
        let parent_id = parent.id;
        store.append(&parent).unwrap();

        let child1 = Task::new("Child 1", "research")
            .with_parent(parent_id)
            .with_priority(1);
        let child2 = Task::new("Child 2", "implement")
            .with_parent(parent_id)
            .with_priority(2);
        store.append(&child1).unwrap();
        store.append(&child2).unwrap();

        let all = store.load_all().unwrap();
        assert_eq!(all.len(), 3);

        let children: Vec<_> = all
            .iter()
            .filter(|t| t.parent_id == Some(parent_id))
            .collect();
        assert_eq!(children.len(), 2);
    }
}

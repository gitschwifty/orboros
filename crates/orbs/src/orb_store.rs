use std::path::{Path, PathBuf};

use crate::id::OrbId;
use crate::orb::{Orb, OrbType};
use crate::task::TaskStatus;

/// Append-only JSONL store for Orbs.
///
/// Each mutation is appended as a full JSON line. Reading replays the log
/// and keeps the latest version of each orb (by ID). Tombstoned orbs are
/// excluded from queries unless explicitly requested.
#[derive(Clone)]
pub struct OrbStore {
    path: PathBuf,
}

impl OrbStore {
    /// Opens or creates a JSONL store at the given path.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Returns the path to the store file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends an orb to the store.
    ///
    /// # Errors
    ///
    /// Returns an IO error if writing fails.
    pub fn append(&self, orb: &Orb) -> std::io::Result<()> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let mut line = serde_json::to_string(orb)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        file.write_all(line.as_bytes())?;
        Ok(())
    }

    /// Reads all non-tombstoned orbs, deduplicating by ID (latest wins).
    ///
    /// # Errors
    ///
    /// Returns an IO error if reading fails.
    pub fn load_all(&self) -> std::io::Result<Vec<Orb>> {
        Ok(self
            .load_all_including_tombstoned()?
            .into_iter()
            .filter(|o| !o.is_tombstoned())
            .collect())
    }

    /// Reads all orbs including tombstoned ones, deduplicating by ID (latest wins).
    ///
    /// # Errors
    ///
    /// Returns an IO error if reading fails.
    pub fn load_all_including_tombstoned(&self) -> std::io::Result<Vec<Orb>> {
        use std::collections::HashMap;
        use std::io::BufRead;

        if !self.path.exists() {
            return Ok(vec![]);
        }

        let file = std::fs::File::open(&self.path)?;
        let reader = std::io::BufReader::new(file);
        let mut orbs: HashMap<String, Orb> = HashMap::new();

        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let orb: Orb = serde_json::from_str(&line)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            orbs.insert(orb.id.as_str().to_string(), orb);
        }

        let mut result: Vec<Orb> = orbs.into_values().collect();
        result.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(result)
    }

    /// Loads orbs filtered by effective `TaskStatus`.
    ///
    /// # Errors
    ///
    /// Returns an IO error if reading fails.
    pub fn load_by_status(&self, status: TaskStatus) -> std::io::Result<Vec<Orb>> {
        Ok(self
            .load_all()?
            .into_iter()
            .filter(|o| o.effective_status() == status)
            .collect())
    }

    /// Loads orbs filtered by `OrbType`.
    ///
    /// # Errors
    ///
    /// Returns an IO error if reading fails.
    pub fn load_by_type(&self, orb_type: &OrbType) -> std::io::Result<Vec<Orb>> {
        Ok(self
            .load_all()?
            .into_iter()
            .filter(|o| &o.orb_type == orb_type)
            .collect())
    }

    /// Loads a single orb by ID, or None if not found (excludes tombstoned).
    ///
    /// # Errors
    ///
    /// Returns an IO error if reading fails.
    pub fn load_by_id(&self, id: &OrbId) -> std::io::Result<Option<Orb>> {
        let id_str = id.as_str();
        Ok(self
            .load_all()?
            .into_iter()
            .find(|o| o.id.as_str() == id_str))
    }

    /// Loads children of a given parent ID.
    ///
    /// # Errors
    ///
    /// Returns an IO error if reading fails.
    pub fn load_children(&self, parent_id: &OrbId) -> std::io::Result<Vec<Orb>> {
        Ok(self
            .load_all()?
            .into_iter()
            .filter(|o| o.parent_id.as_ref() == Some(parent_id))
            .collect())
    }

    /// Updates an orb by appending its new state.
    ///
    /// # Errors
    ///
    /// Returns an IO error if writing fails.
    pub fn update(&self, orb: &Orb) -> std::io::Result<()> {
        self.append(orb)
    }

    /// Returns the set of all existing IDs (for collision checking during ID generation).
    ///
    /// # Errors
    ///
    /// Returns an IO error if reading fails.
    pub fn existing_ids(&self) -> std::io::Result<std::collections::HashSet<String>> {
        Ok(self
            .load_all_including_tombstoned()?
            .into_iter()
            .map(|o| o.id.as_str().to_string())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orb::OrbStatus;

    #[test]
    fn create_and_load_orbs() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrbStore::new(dir.path().join("orbs.jsonl"));

        let orb1 = Orb::new("Orb one", "First orb");
        let orb2 = Orb::new("Orb two", "Second orb");
        store.append(&orb1).unwrap();
        store.append(&orb2).unwrap();

        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 2);
    }

    #[test]
    fn update_deduplicates_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrbStore::new(dir.path().join("orbs.jsonl"));

        let mut orb = Orb::new("Test orb", "description");
        store.append(&orb).unwrap();

        orb.set_status(OrbStatus::Active);
        store.update(&orb).unwrap();

        orb.set_status(OrbStatus::Done);
        orb.result = Some("completed".into());
        store.update(&orb).unwrap();

        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].result.as_deref(), Some("completed"));
    }

    #[test]
    fn tombstoned_excluded_from_queries() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrbStore::new(dir.path().join("orbs.jsonl"));

        let mut orb = Orb::new("Will be deleted", "test");
        store.append(&orb).unwrap();

        let normal = Orb::new("Normal", "stays");
        store.append(&normal).unwrap();

        orb.tombstone(Some("duplicate".into()));
        store.update(&orb).unwrap();

        let loaded = store.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].title, "Normal");

        // But load_all_including_tombstoned returns both
        let all = store.load_all_including_tombstoned().unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn load_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrbStore::new(dir.path().join("orbs.jsonl"));

        let orb = Orb::new("Find me", "test");
        let target_id = orb.id.clone();
        store.append(&orb).unwrap();
        store.append(&Orb::new("Other", "orb")).unwrap();

        let found = store.load_by_id(&target_id).unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().title, "Find me");
    }

    #[test]
    fn load_children() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrbStore::new(dir.path().join("orbs.jsonl"));

        let parent = Orb::new("Parent", "parent orb");
        let parent_id = parent.id.clone();
        store.append(&parent).unwrap();

        let child1 = Orb::new("Child 1", "first child").with_parent(parent_id.clone(), None);
        let child2 = Orb::new("Child 2", "second child").with_parent(parent_id.clone(), None);
        store.append(&child1).unwrap();
        store.append(&child2).unwrap();

        let children = store.load_children(&parent_id).unwrap();
        assert_eq!(children.len(), 2);
    }

    #[test]
    fn load_by_type() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrbStore::new(dir.path().join("orbs.jsonl"));

        let task = Orb::new("Task", "a task");
        let epic = Orb::new("Epic", "an epic").with_type(OrbType::Epic);
        store.append(&task).unwrap();
        store.append(&epic).unwrap();

        let tasks = store.load_by_type(&OrbType::Task).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "Task");

        let epics = store.load_by_type(&OrbType::Epic).unwrap();
        assert_eq!(epics.len(), 1);
        assert_eq!(epics[0].title, "Epic");
    }

    #[test]
    fn empty_store_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrbStore::new(dir.path().join("nonexistent.jsonl"));
        let loaded = store.load_all().unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn existing_ids_for_collision_check() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrbStore::new(dir.path().join("orbs.jsonl"));

        let orb = Orb::new("Test", "test");
        let id = orb.id.as_str().to_string();
        store.append(&orb).unwrap();

        let existing = store.existing_ids().unwrap();
        assert!(existing.contains(&id));
    }
}

use std::path::{Path, PathBuf};

use crate::audit::{AuditEvent, Comment, EventType};
use crate::id::OrbId;

/// Append-only JSONL store for audit events and comments.
///
/// Events are stored in `events.jsonl` and comments in `comments.jsonl`,
/// both in the same directory.
#[derive(Clone)]
pub struct AuditStore {
    events_path: PathBuf,
    comments_path: PathBuf,
}

impl AuditStore {
    /// Creates an `AuditStore` rooted at the given events file path.
    ///
    /// Comments are stored alongside in a sibling `comments.jsonl` file
    /// in the same directory.
    pub fn new(events_path: impl Into<PathBuf>) -> Self {
        let events_path = events_path.into();
        let comments_path = events_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("comments.jsonl");
        Self {
            events_path,
            comments_path,
        }
    }

    /// Returns the path to the events file.
    pub fn events_path(&self) -> &Path {
        &self.events_path
    }

    /// Returns the path to the comments file.
    pub fn comments_path(&self) -> &Path {
        &self.comments_path
    }

    /// Appends an audit event to the events log.
    ///
    /// # Errors
    ///
    /// Returns an IO error if writing fails.
    pub fn log_event(&self, event: &AuditEvent) -> std::io::Result<()> {
        append_jsonl(&self.events_path, event)
    }

    /// Returns all audit events for a given orb, in chronological order.
    ///
    /// # Errors
    ///
    /// Returns an IO error if reading fails.
    pub fn events_for_orb(&self, orb_id: &OrbId) -> std::io::Result<Vec<AuditEvent>> {
        let all = self.all_events()?;
        Ok(all.into_iter().filter(|e| e.orb_id == *orb_id).collect())
    }

    /// Returns all audit events in chronological order.
    ///
    /// # Errors
    ///
    /// Returns an IO error if reading fails.
    pub fn all_events(&self) -> std::io::Result<Vec<AuditEvent>> {
        read_jsonl(&self.events_path)
    }

    /// Adds a comment and logs a corresponding `Commented` audit event.
    ///
    /// # Errors
    ///
    /// Returns an IO error if writing fails.
    pub fn add_comment(&self, comment: &Comment) -> std::io::Result<()> {
        append_jsonl(&self.comments_path, comment)?;
        let event = AuditEvent::new(
            comment.orb_id.clone(),
            EventType::Commented,
            &comment.author,
            Some(comment.body.clone()),
        );
        self.log_event(&event)
    }

    /// Returns all comments for a given orb, in chronological order.
    ///
    /// # Errors
    ///
    /// Returns an IO error if reading fails.
    pub fn comments_for_orb(&self, orb_id: &OrbId) -> std::io::Result<Vec<Comment>> {
        let all = self.all_comments()?;
        Ok(all.into_iter().filter(|c| c.orb_id == *orb_id).collect())
    }

    /// Returns all comments in chronological order.
    ///
    /// # Errors
    ///
    /// Returns an IO error if reading fails.
    fn all_comments(&self) -> std::io::Result<Vec<Comment>> {
        read_jsonl(&self.comments_path)
    }
}

/// Appends a serializable value as a JSON line to the given file.
fn append_jsonl<T: serde::Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let mut line = serde_json::to_string(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push('\n');
    file.write_all(line.as_bytes())
}

/// Reads all JSON lines from a file, returning them in order.
fn read_jsonl<T: serde::de::DeserializeOwned>(path: &Path) -> std::io::Result<Vec<T>> {
    use std::io::BufRead;

    if !path.exists() {
        return Ok(vec![]);
    }

    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut items = Vec::new();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let item: T = serde_json::from_str(&line)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        items.push(item);
    }

    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{AuditEvent, Comment, EventType};

    #[test]
    fn log_and_retrieve_events() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuditStore::new(dir.path().join("events.jsonl"));

        let orb_id = OrbId::from_raw("orb-abc");
        let event = AuditEvent::new(orb_id.clone(), EventType::Created, "alice", None);
        store.log_event(&event).unwrap();

        let events = store.all_events().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].orb_id, orb_id);
        assert_eq!(events[0].event_type, EventType::Created);
    }

    #[test]
    fn events_for_orb_filters_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuditStore::new(dir.path().join("events.jsonl"));

        let orb_a = OrbId::from_raw("orb-a");
        let orb_b = OrbId::from_raw("orb-b");

        store
            .log_event(&AuditEvent::new(
                orb_a.clone(),
                EventType::Created,
                "alice",
                None,
            ))
            .unwrap();
        store
            .log_event(&AuditEvent::new(
                orb_b.clone(),
                EventType::Created,
                "bob",
                None,
            ))
            .unwrap();
        store
            .log_event(&AuditEvent::new(
                orb_a.clone(),
                EventType::StatusChanged,
                "alice",
                Some("pending -> active".into()),
            ))
            .unwrap();

        let events_a = store.events_for_orb(&orb_a).unwrap();
        assert_eq!(events_a.len(), 2);
        assert_eq!(events_a[0].event_type, EventType::Created);
        assert_eq!(events_a[1].event_type, EventType::StatusChanged);

        let events_b = store.events_for_orb(&orb_b).unwrap();
        assert_eq!(events_b.len(), 1);
    }

    #[test]
    fn empty_store_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuditStore::new(dir.path().join("events.jsonl"));

        let events = store.all_events().unwrap();
        assert!(events.is_empty());

        let orb_id = OrbId::from_raw("orb-xyz");
        let events = store.events_for_orb(&orb_id).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn add_comment_stores_and_logs_event() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuditStore::new(dir.path().join("events.jsonl"));

        let orb_id = OrbId::from_raw("orb-abc");
        let comment = Comment::new(orb_id.clone(), "alice", "This looks good");
        store.add_comment(&comment).unwrap();

        // Comment is stored
        let comments = store.comments_for_orb(&orb_id).unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].author, "alice");
        assert_eq!(comments[0].body, "This looks good");

        // Audit event is also logged
        let events = store.events_for_orb(&orb_id).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::Commented);
        assert_eq!(events[0].details.as_deref(), Some("This looks good"));
    }

    #[test]
    fn comments_for_orb_filters_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuditStore::new(dir.path().join("events.jsonl"));

        let orb_a = OrbId::from_raw("orb-a");
        let orb_b = OrbId::from_raw("orb-b");

        store
            .add_comment(&Comment::new(orb_a.clone(), "alice", "Comment on A"))
            .unwrap();
        store
            .add_comment(&Comment::new(orb_b.clone(), "bob", "Comment on B"))
            .unwrap();
        store
            .add_comment(&Comment::new(orb_a.clone(), "carol", "Another on A"))
            .unwrap();

        let comments_a = store.comments_for_orb(&orb_a).unwrap();
        assert_eq!(comments_a.len(), 2);
        assert_eq!(comments_a[0].body, "Comment on A");
        assert_eq!(comments_a[1].body, "Another on A");

        let comments_b = store.comments_for_orb(&orb_b).unwrap();
        assert_eq!(comments_b.len(), 1);
    }

    #[test]
    fn empty_comments_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuditStore::new(dir.path().join("events.jsonl"));

        let orb_id = OrbId::from_raw("orb-xyz");
        let comments = store.comments_for_orb(&orb_id).unwrap();
        assert!(comments.is_empty());
    }

    #[test]
    fn multiple_event_types_for_same_orb() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuditStore::new(dir.path().join("events.jsonl"));
        let orb_id = OrbId::from_raw("orb-lifecycle");

        store
            .log_event(&AuditEvent::new(
                orb_id.clone(),
                EventType::Created,
                "system",
                None,
            ))
            .unwrap();
        store
            .log_event(&AuditEvent::new(
                orb_id.clone(),
                EventType::StatusChanged,
                "alice",
                Some("pending -> active".into()),
            ))
            .unwrap();
        store
            .log_event(&AuditEvent::new(
                orb_id.clone(),
                EventType::PriorityChanged,
                "bob",
                Some("3 -> 1".into()),
            ))
            .unwrap();
        store
            .log_event(&AuditEvent::new(
                orb_id.clone(),
                EventType::Deferred,
                "carol",
                None,
            ))
            .unwrap();

        let events = store.events_for_orb(&orb_id).unwrap();
        assert_eq!(events.len(), 4);

        let all = store.all_events().unwrap();
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn event_serde_round_trip_through_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuditStore::new(dir.path().join("events.jsonl"));

        let orb_id = OrbId::from_raw("orb-serde");
        let event = AuditEvent::new(
            orb_id.clone(),
            EventType::Tombstoned,
            "admin",
            Some("no longer needed".into()),
        );
        let original_id = event.id;
        store.log_event(&event).unwrap();

        let loaded = store.all_events().unwrap();
        assert_eq!(loaded[0].id, original_id);
        assert_eq!(loaded[0].event_type, EventType::Tombstoned);
        assert_eq!(loaded[0].details.as_deref(), Some("no longer needed"));
    }

    #[test]
    fn comment_serde_round_trip_through_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuditStore::new(dir.path().join("events.jsonl"));

        let orb_id = OrbId::from_raw("orb-serde");
        let comment = Comment::new(orb_id.clone(), "tester", "Round trip test body");
        let original_id = comment.id;
        store.add_comment(&comment).unwrap();

        let loaded = store.comments_for_orb(&orb_id).unwrap();
        assert_eq!(loaded[0].id, original_id);
        assert_eq!(loaded[0].body, "Round trip test body");
    }
}

use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::session::{CloseReason, Session, SessionEvent, SessionId, SessionInit, SessionStatus};

/// Default per-session file size ceiling: 512 MiB. Sized to comfortably hold
/// any practical conversation; the cap exists to catch runaway loops, not
/// to paginate normal use.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;

/// Default age (days) after which a closed session is archived.
pub const DEFAULT_ROTATE_AFTER_DAYS: u32 = 30;

#[derive(Debug, thiserror::Error)]
pub enum SessionStoreError {
    #[error("session {id} already exists at {path}")]
    AlreadyExists { id: String, path: PathBuf },

    #[error("session {id} not found")]
    NotFound { id: String },

    #[error("session {id} is closed; no further events accepted")]
    Closed { id: String },

    #[error(
        "session {id} would exceed configured size cap ({cap} bytes) after append (current {current} bytes)"
    )]
    SizeCapExceeded { id: String, current: u64, cap: u64 },

    #[error("transcript for session {id} is empty")]
    EmptyTranscript { id: String },

    #[error("transcript for session {id} has no init header")]
    MissingHeader { id: String },

    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("serde error in session {id}: {source}")]
    Serde {
        id: String,
        #[source]
        source: serde_json::Error,
    },
}

/// Retention/size policy for sessions in a store.
#[derive(Debug, Clone, Copy)]
pub struct SessionRetentionConfig {
    pub rotate_after_days: u32,
    pub max_file_bytes: u64,
}

impl Default for SessionRetentionConfig {
    fn default() -> Self {
        Self {
            rotate_after_days: DEFAULT_ROTATE_AFTER_DAYS,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
        }
    }
}

/// Lightweight metadata returned by `SessionStore::list`. Reads just the init
/// header and the last event to avoid scanning whole transcripts.
#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub init: SessionInit,
    pub status: SessionStatus,
    pub last_event_at: Option<DateTime<Utc>>,
    pub byte_size: u64,
}

/// Append-only JSONL store for conversation sessions.
///
/// Layout: `<root>/<session-id>.jsonl`. First line is the `SessionInit` header;
/// subsequent lines are `SessionEvent`. Archived sessions move under
/// `<root>/archive/<yyyy-mm>/<session-id>.jsonl`.
#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
    retention: SessionRetentionConfig,
}

impl SessionStore {
    /// Opens or creates a session store rooted at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            root: path.into(),
            retention: SessionRetentionConfig::default(),
        }
    }

    /// Overrides retention/size policy.
    #[must_use]
    pub fn with_retention(mut self, retention: SessionRetentionConfig) -> Self {
        self.retention = retention;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn retention(&self) -> SessionRetentionConfig {
        self.retention
    }

    /// Creates a new session file by writing its init header.
    ///
    /// # Errors
    ///
    /// Returns `AlreadyExists` if the session id already has a transcript on
    /// disk; `Io` if directory creation or write fails; `Serde` if the header
    /// cannot be serialized.
    pub fn create(&self, init: &SessionInit) -> Result<(), SessionStoreError> {
        self.ensure_root()?;
        let path = self.session_path(&init.id);
        if path.exists() {
            return Err(SessionStoreError::AlreadyExists {
                id: init.id.to_string(),
                path,
            });
        }
        let mut line = serde_json::to_string(init).map_err(|e| SessionStoreError::Serde {
            id: init.id.to_string(),
            source: e,
        })?;
        line.push('\n');
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(|e| SessionStoreError::Io {
                path: path.clone(),
                source: e,
            })?;
        file.write_all(line.as_bytes())
            .map_err(|e| SessionStoreError::Io {
                path: path.clone(),
                source: e,
            })?;
        Ok(())
    }

    /// Appends an event. Refuses if the session is closed, or if the file
    /// would exceed the configured size cap after the write.
    ///
    /// # Errors
    ///
    /// `NotFound` if the session id has no transcript; `Closed` if the
    /// session is in `Closed` status; `SizeCapExceeded` if the projected
    /// file size would exceed `max_file_bytes`; `Io`/`Serde` on transport
    /// or serialization failures.
    pub fn append_event(
        &self,
        id: &SessionId,
        event: &SessionEvent,
    ) -> Result<(), SessionStoreError> {
        let path = self.session_path(id);
        if !path.exists() {
            return Err(SessionStoreError::NotFound { id: id.to_string() });
        }
        // Refuse appends to closed sessions. Cheap check via load.
        let snapshot = self.load(id)?.0;
        if matches!(snapshot.status, SessionStatus::Closed) {
            return Err(SessionStoreError::Closed { id: id.to_string() });
        }

        let mut line = serde_json::to_string(event).map_err(|e| SessionStoreError::Serde {
            id: id.to_string(),
            source: e,
        })?;
        line.push('\n');

        let current = fs::metadata(&path)
            .map_err(|e| SessionStoreError::Io {
                path: path.clone(),
                source: e,
            })?
            .len();
        let projected = current.saturating_add(line.len() as u64);
        if projected > self.retention.max_file_bytes {
            return Err(SessionStoreError::SizeCapExceeded {
                id: id.to_string(),
                current,
                cap: self.retention.max_file_bytes,
            });
        }

        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|e| SessionStoreError::Io {
                path: path.clone(),
                source: e,
            })?;
        file.write_all(line.as_bytes())
            .map_err(|e| SessionStoreError::Io {
                path: path.clone(),
                source: e,
            })?;
        Ok(())
    }

    /// Loads the session header and all events in order. Reconstructs the
    /// current `Session` snapshot by folding events.
    ///
    /// # Errors
    ///
    /// `NotFound` if no transcript exists for `id`; `EmptyTranscript` if the
    /// file is empty; `Serde` on a malformed line; `Io` on read failure.
    pub fn load(&self, id: &SessionId) -> Result<(Session, Vec<SessionEvent>), SessionStoreError> {
        let path = self.session_path(id);
        if !path.exists() {
            return Err(SessionStoreError::NotFound { id: id.to_string() });
        }
        let file = fs::File::open(&path).map_err(|e| SessionStoreError::Io {
            path: path.clone(),
            source: e,
        })?;
        let reader = BufReader::new(file);
        let mut lines = reader.lines();

        let header_line = lines
            .next()
            .ok_or_else(|| SessionStoreError::EmptyTranscript { id: id.to_string() })?
            .map_err(|e| SessionStoreError::Io {
                path: path.clone(),
                source: e,
            })?;
        let init: SessionInit =
            serde_json::from_str(&header_line).map_err(|e| SessionStoreError::Serde {
                id: id.to_string(),
                source: e,
            })?;

        let mut session = Session::from_init(init);
        let mut events = Vec::new();
        for line in lines {
            let line = line.map_err(|e| SessionStoreError::Io {
                path: path.clone(),
                source: e,
            })?;
            if line.trim().is_empty() {
                continue;
            }
            let event: SessionEvent =
                serde_json::from_str(&line).map_err(|e| SessionStoreError::Serde {
                    id: id.to_string(),
                    source: e,
                })?;
            session.apply(&event);
            events.push(event);
        }
        Ok((session, events))
    }

    /// Lists all sessions (live, not archived) with light metadata. Skips
    /// files that fail to parse rather than failing the whole list call.
    ///
    /// # Errors
    ///
    /// `Io` if the store root cannot be read.
    pub fn list(&self) -> Result<Vec<SessionMeta>, SessionStoreError> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let entries = fs::read_dir(&self.root).map_err(|e| SessionStoreError::Io {
            path: self.root.clone(),
            source: e,
        })?;
        for entry in entries {
            let entry = entry.map_err(|e| SessionStoreError::Io {
                path: self.root.clone(),
                source: e,
            })?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(meta) = read_meta(&path) {
                out.push(meta);
            }
        }
        out.sort_by(|a, b| b.init.created_at.cmp(&a.init.created_at));
        Ok(out)
    }

    /// Appends a `StatusChanged` event marking the session as `Closed`.
    ///
    /// # Errors
    ///
    /// Same errors as `append_event`.
    pub fn close(
        &self,
        id: &SessionId,
        reason: CloseReason,
        at: DateTime<Utc>,
    ) -> Result<(), SessionStoreError> {
        let event = SessionEvent::StatusChanged {
            to: SessionStatus::Closed,
            reason: Some(reason),
            at,
        };
        self.append_event(id, &event)
    }

    /// Path of the live transcript for `id`.
    pub fn session_path(&self, id: &SessionId) -> PathBuf {
        self.root.join(format!("{}.jsonl", id.as_str()))
    }

    fn ensure_root(&self) -> Result<(), SessionStoreError> {
        if !self.root.exists() {
            fs::create_dir_all(&self.root).map_err(|e| SessionStoreError::Io {
                path: self.root.clone(),
                source: e,
            })?;
        }
        Ok(())
    }

    /// Moves closed sessions older than the configured threshold under
    /// `<root>/archive/<yyyy-mm>/`. Returns the list of moved session ids.
    ///
    /// # Errors
    ///
    /// Returns `SessionStoreError::Io` if directory creation or rename fails,
    /// or any error surfaced by `list` while scanning metadata.
    pub fn rotate(&self, now: DateTime<Utc>) -> Result<Vec<SessionId>, SessionStoreError> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let threshold = chrono::Duration::days(i64::from(self.retention.rotate_after_days));
        let mut moved = Vec::new();
        let metas = self.list()?;
        for meta in metas {
            let age_anchor = meta.last_event_at.unwrap_or(meta.init.created_at);
            let age = now.signed_duration_since(age_anchor);
            let eligible = matches!(meta.status, SessionStatus::Closed) && age >= threshold;
            if !eligible {
                continue;
            }
            let src = self.session_path(&meta.init.id);
            let bucket = age_anchor.format("%Y-%m").to_string();
            let archive_dir = self.root.join("archive").join(&bucket);
            fs::create_dir_all(&archive_dir).map_err(|e| SessionStoreError::Io {
                path: archive_dir.clone(),
                source: e,
            })?;
            let dst = archive_dir.join(format!("{}.jsonl", meta.init.id.as_str()));
            fs::rename(&src, &dst).map_err(|e| SessionStoreError::Io {
                path: src.clone(),
                source: e,
            })?;
            moved.push(meta.init.id);
        }
        Ok(moved)
    }
}

fn read_meta(path: &Path) -> Option<SessionMeta> {
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();
    let header_line = lines.next()?.ok()?;
    let init: SessionInit = serde_json::from_str(&header_line).ok()?;
    let mut status = SessionStatus::Active;
    let mut last_event_at: Option<DateTime<Utc>> = None;
    for line in lines {
        let Ok(line) = line else { continue };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<SessionEvent>(&line) else {
            continue;
        };
        if let Some(at) = event_timestamp(&event) {
            last_event_at = Some(at);
        }
        if let SessionEvent::StatusChanged { to, .. } = &event {
            status = *to;
        }
    }
    let byte_size = fs::metadata(path).ok().map_or(0, |m| m.len());
    Some(SessionMeta {
        init,
        status,
        last_event_at,
        byte_size,
    })
}

fn event_timestamp(event: &SessionEvent) -> Option<DateTime<Utc>> {
    match event {
        SessionEvent::UserMessage { at, .. }
        | SessionEvent::AssistantMessage { at, .. }
        | SessionEvent::Error { at, .. }
        | SessionEvent::Cancelled { at, .. }
        | SessionEvent::ContextReset { at, .. }
        | SessionEvent::StatusChanged { at, .. } => Some(*at),
        SessionEvent::AssistantDelta { .. }
        | SessionEvent::ToolStart { .. }
        | SessionEvent::ToolEnd { .. }
        | SessionEvent::Usage { .. }
        | SessionEvent::OrbSpawned { .. }
        | SessionEvent::OrbResult { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionUsage, ToolOutcome, TurnId};
    use tempfile::tempdir;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-05-21T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn fixture_init(id: &str) -> SessionInit {
        SessionInit {
            id: SessionId::from_raw(id),
            created_at: now(),
            model: "openrouter/free".into(),
            system_prompt: None,
            cwd: None,
            linked_orb: None,
        }
    }

    #[test]
    fn create_writes_header_line() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let init = fixture_init("session-a");
        store.create(&init).unwrap();
        let path = store.session_path(&init.id);
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.starts_with('{'));
        assert!(body.contains("session-a"));
        assert_eq!(body.lines().count(), 1);
    }

    #[test]
    fn create_refuses_duplicate() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let init = fixture_init("session-dup");
        store.create(&init).unwrap();
        let err = store.create(&init).unwrap_err();
        assert!(matches!(err, SessionStoreError::AlreadyExists { .. }));
    }

    #[test]
    fn append_event_then_load_round_trips() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let init = fixture_init("session-rt");
        store.create(&init).unwrap();

        let t1 = TurnId::from_raw("turn-1");
        let events = vec![
            SessionEvent::UserMessage {
                turn_id: t1.clone(),
                content: "hi".into(),
                at: now(),
            },
            SessionEvent::AssistantDelta {
                turn_id: t1.clone(),
                chunk: "he".into(),
            },
            SessionEvent::AssistantDelta {
                turn_id: t1.clone(),
                chunk: "llo".into(),
            },
            SessionEvent::AssistantMessage {
                turn_id: t1.clone(),
                content: "hello".into(),
                at: now(),
            },
            SessionEvent::Usage {
                turn_id: t1,
                usage: SessionUsage {
                    prompt_tokens: 5,
                    completion_tokens: 3,
                    total_tokens: 8,
                },
            },
        ];
        for ev in &events {
            store.append_event(&init.id, ev).unwrap();
        }

        let (snapshot, loaded) = store.load(&init.id).unwrap();
        assert_eq!(loaded, events);
        assert_eq!(snapshot.turn_count, 1);
        assert_eq!(snapshot.total_usage.total_tokens, 8);
        assert_eq!(snapshot.status, SessionStatus::Active);
    }

    #[test]
    fn append_event_on_missing_session_errors() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let err = store
            .append_event(
                &SessionId::from_raw("session-ghost"),
                &SessionEvent::UserMessage {
                    turn_id: TurnId::new(),
                    content: "no".into(),
                    at: now(),
                },
            )
            .unwrap_err();
        assert!(matches!(err, SessionStoreError::NotFound { .. }));
    }

    #[test]
    fn close_then_append_is_refused() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let init = fixture_init("session-cl");
        store.create(&init).unwrap();
        store.close(&init.id, CloseReason::UserExit, now()).unwrap();

        let err = store
            .append_event(
                &init.id,
                &SessionEvent::UserMessage {
                    turn_id: TurnId::new(),
                    content: "after close".into(),
                    at: now(),
                },
            )
            .unwrap_err();
        assert!(matches!(err, SessionStoreError::Closed { .. }));

        let (snapshot, _) = store.load(&init.id).unwrap();
        assert_eq!(snapshot.status, SessionStatus::Closed);
        assert_eq!(snapshot.close_reason, Some(CloseReason::UserExit));
    }

    #[test]
    fn size_cap_blocks_append() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path()).with_retention(SessionRetentionConfig {
            rotate_after_days: 30,
            max_file_bytes: 256, // tiny cap so the next append trips it
        });
        let init = fixture_init("session-cap");
        store.create(&init).unwrap();
        // Fill until we get the SizeCapExceeded error.
        let mut got_cap_err = false;
        for i in 0..50 {
            let res = store.append_event(
                &init.id,
                &SessionEvent::UserMessage {
                    turn_id: TurnId::from_raw(format!("turn-{i}")),
                    content: "padding-content-to-fill-the-cap".into(),
                    at: now(),
                },
            );
            if let Err(SessionStoreError::SizeCapExceeded { .. }) = res {
                got_cap_err = true;
                break;
            }
            res.unwrap();
        }
        assert!(got_cap_err, "expected SizeCapExceeded within 50 appends");
    }

    #[test]
    fn list_returns_metadata_sorted_by_created_at_desc() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path());

        let older = SessionInit {
            id: SessionId::from_raw("session-old"),
            created_at: DateTime::parse_from_rfc3339("2026-05-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            model: "m".into(),
            system_prompt: None,
            cwd: None,
            linked_orb: None,
        };
        let newer = SessionInit {
            id: SessionId::from_raw("session-new"),
            created_at: DateTime::parse_from_rfc3339("2026-05-20T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            model: "m".into(),
            system_prompt: None,
            cwd: None,
            linked_orb: None,
        };
        store.create(&older).unwrap();
        store.create(&newer).unwrap();

        let metas = store.list().unwrap();
        assert_eq!(metas.len(), 2);
        assert_eq!(metas[0].init.id.as_str(), "session-new");
        assert_eq!(metas[1].init.id.as_str(), "session-old");
    }

    #[test]
    fn list_ignores_non_jsonl_files_and_subdirs() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        store.create(&fixture_init("session-real")).unwrap();
        fs::write(dir.path().join("README.md"), "ignore me").unwrap();
        fs::create_dir_all(dir.path().join("archive").join("2026-04")).unwrap();
        let metas = store.list().unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].init.id.as_str(), "session-real");
    }

    #[test]
    fn rotate_moves_only_closed_sessions_older_than_threshold() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path()).with_retention(SessionRetentionConfig {
            rotate_after_days: 30,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
        });
        let created = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let old_closed = SessionInit {
            id: SessionId::from_raw("session-old-closed"),
            created_at: created,
            model: "m".into(),
            system_prompt: None,
            cwd: None,
            linked_orb: None,
        };
        let old_active = SessionInit {
            id: SessionId::from_raw("session-old-active"),
            created_at: created,
            model: "m".into(),
            system_prompt: None,
            cwd: None,
            linked_orb: None,
        };
        let recent_closed = SessionInit {
            id: SessionId::from_raw("session-recent-closed"),
            created_at: DateTime::parse_from_rfc3339("2026-05-15T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            model: "m".into(),
            system_prompt: None,
            cwd: None,
            linked_orb: None,
        };

        store.create(&old_closed).unwrap();
        store.create(&old_active).unwrap();
        store.create(&recent_closed).unwrap();
        store
            .close(&old_closed.id, CloseReason::UserExit, created)
            .unwrap();
        store
            .close(
                &recent_closed.id,
                CloseReason::UserExit,
                DateTime::parse_from_rfc3339("2026-05-20T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            )
            .unwrap();

        let now = DateTime::parse_from_rfc3339("2026-05-21T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let moved = store.rotate(now).unwrap();
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].as_str(), "session-old-closed");

        assert!(!store.session_path(&old_closed.id).exists());
        assert!(store.session_path(&old_active.id).exists());
        assert!(store.session_path(&recent_closed.id).exists());

        let archived = dir
            .path()
            .join("archive")
            .join("2026-01")
            .join("session-old-closed.jsonl");
        assert!(archived.exists());
    }

    #[test]
    fn load_errors_on_missing_session() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let err = store
            .load(&SessionId::from_raw("session-nope"))
            .unwrap_err();
        assert!(matches!(err, SessionStoreError::NotFound { .. }));
    }

    #[test]
    fn load_errors_on_corrupt_header() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let id = SessionId::from_raw("session-bad");
        fs::create_dir_all(dir.path()).unwrap();
        fs::write(store.session_path(&id), "{not-json}\n").unwrap();
        let err = store.load(&id).unwrap_err();
        assert!(matches!(err, SessionStoreError::Serde { .. }));
    }

    #[test]
    fn tool_end_event_round_trips_through_store() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let init = fixture_init("session-tool");
        store.create(&init).unwrap();
        let ev = SessionEvent::ToolEnd {
            turn_id: TurnId::from_raw("turn-1"),
            name: "bash".into(),
            outcome: ToolOutcome::Err {
                message: "exit 1".into(),
            },
        };
        store.append_event(&init.id, &ev).unwrap();
        let (_, events) = store.load(&init.id).unwrap();
        assert_eq!(events, vec![ev]);
    }
}

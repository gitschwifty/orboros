//! `orboros sessions` subcommand handlers.
//!
//! Reads from `SessionStore` and renders to a writer (stdout in the CLI,
//! a buffer in tests). `list` prints a table-ish summary; `show` replays a
//! session's transcript through the same `Renderer` used by live chat.

use std::io::{self, IsTerminal, Write};

use orbs::session::{SessionId, SessionStatus};
use orbs::session_store::{SessionMeta, SessionStore, SessionStoreError};

use crate::convo::render::Renderer;

/// Filter passed to `cmd_sessions_list`. `None` means no filter.
#[derive(Debug, Clone, Copy)]
pub struct SessionListFilter {
    pub status: Option<SessionStatus>,
}

/// Lists sessions with light metadata sorted newest-first.
///
/// # Errors
///
/// Returns `SessionStoreError::Io` if the store directory can't be read,
/// or `io::Error` if writing to `out` fails.
pub fn cmd_sessions_list<W: Write>(
    store: &SessionStore,
    filter: SessionListFilter,
    mut out: W,
) -> Result<(), SessionsCmdError> {
    let metas = store.list().map_err(SessionsCmdError::Store)?;
    let filtered: Vec<&SessionMeta> = metas
        .iter()
        .filter(|m| match filter.status {
            Some(s) => m.status == s,
            None => true,
        })
        .collect();

    if filtered.is_empty() {
        writeln!(out, "(no sessions)")?;
        return Ok(());
    }

    writeln!(
        out,
        "ID                           STATUS   CREATED                BYTES        MODEL"
    )?;
    for meta in filtered {
        writeln!(
            out,
            "{:<28} {:<8} {:<22} {:<12} {}",
            meta.init.id,
            status_label(meta.status),
            meta.init.created_at.format("%Y-%m-%d %H:%M:%S"),
            meta.byte_size,
            meta.init.model,
        )?;
    }
    Ok(())
}

/// Replays a session's transcript through `Renderer`.
///
/// # Errors
///
/// `Store` if the session can't be loaded; `Io` if rendering fails.
pub fn cmd_sessions_show<W: Write>(
    store: &SessionStore,
    id: &SessionId,
    use_color: bool,
    out: W,
) -> Result<(), SessionsCmdError> {
    let (snapshot, events) = store.load(id).map_err(SessionsCmdError::Store)?;
    let mut renderer = Renderer::new(out, use_color);

    renderer.notice(&format!(
        "session {} — model {} — {} turn(s) — {} tokens — status {}",
        snapshot.init.id,
        snapshot.init.model,
        snapshot.turn_count,
        snapshot.total_usage.total_tokens,
        status_label(snapshot.status),
    ))?;

    for event in &events {
        renderer.render(event)?;
    }
    Ok(())
}

/// Convenience used by `main.rs`: detects tty / `NO_COLOR` and forwards
/// to [`cmd_sessions_show`].
///
/// # Errors
///
/// Same as `cmd_sessions_show`.
pub fn cmd_sessions_show_stdout(
    store: &SessionStore,
    id: &SessionId,
) -> Result<(), SessionsCmdError> {
    let stdout = io::stdout();
    let use_color = stdout.is_terminal() && std::env::var_os("NO_COLOR").is_none();
    cmd_sessions_show(store, id, use_color, stdout.lock())
}

fn status_label(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Active => "active",
        SessionStatus::Idle => "idle",
        SessionStatus::Closed => "closed",
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionsCmdError {
    #[error("session store: {0}")]
    Store(#[from] SessionStoreError),

    #[error("io: {0}")]
    Io(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use orbs::session::{CloseReason, SessionEvent, SessionInit, TurnId};
    use tempfile::tempdir;

    fn ts(rfc: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn init(id: &str, created: DateTime<Utc>) -> SessionInit {
        SessionInit {
            id: SessionId::from_raw(id),
            created_at: created,
            model: "mock/test".into(),
            system_prompt: None,
            cwd: None,
            linked_orb: None,
        }
    }

    #[test]
    fn list_empty_store_prints_marker() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let mut buf = Vec::new();
        cmd_sessions_list(&store, SessionListFilter { status: None }, &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "(no sessions)\n");
    }

    #[test]
    fn list_renders_header_and_one_row_per_session() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        store
            .create(&init("session-a", ts("2026-05-21T12:00:00Z")))
            .unwrap();
        store
            .create(&init("session-b", ts("2026-05-20T12:00:00Z")))
            .unwrap();

        let mut buf = Vec::new();
        cmd_sessions_list(&store, SessionListFilter { status: None }, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        // header + 2 rows
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("ID"));
        assert!(lines[1].contains("session-a")); // newest first
        assert!(lines[2].contains("session-b"));
    }

    #[test]
    fn list_filter_active_excludes_closed_sessions() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let active = init("session-active", ts("2026-05-21T12:00:00Z"));
        let closed = init("session-closed", ts("2026-05-20T12:00:00Z"));
        store.create(&active).unwrap();
        store.create(&closed).unwrap();
        store
            .close(
                &closed.id,
                CloseReason::UserExit,
                ts("2026-05-20T13:00:00Z"),
            )
            .unwrap();

        let mut buf = Vec::new();
        cmd_sessions_list(
            &store,
            SessionListFilter {
                status: Some(SessionStatus::Active),
            },
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("session-active"));
        assert!(!out.contains("session-closed"));
    }

    #[test]
    fn list_filter_closed_includes_only_closed_sessions() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let active = init("session-active", ts("2026-05-21T12:00:00Z"));
        let closed = init("session-closed", ts("2026-05-20T12:00:00Z"));
        store.create(&active).unwrap();
        store.create(&closed).unwrap();
        store
            .close(
                &closed.id,
                CloseReason::UserExit,
                ts("2026-05-20T13:00:00Z"),
            )
            .unwrap();

        let mut buf = Vec::new();
        cmd_sessions_list(
            &store,
            SessionListFilter {
                status: Some(SessionStatus::Closed),
            },
            &mut buf,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("session-closed"));
        assert!(!out.contains("session-active"));
    }

    #[test]
    fn show_renders_header_and_events() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let init = init("session-show", ts("2026-05-21T12:00:00Z"));
        store.create(&init).unwrap();
        store
            .append_event(
                &init.id,
                &SessionEvent::UserMessage {
                    turn_id: TurnId::from_raw("turn-1"),
                    content: "hi".into(),
                    at: ts("2026-05-21T12:00:01Z"),
                },
            )
            .unwrap();
        store
            .append_event(
                &init.id,
                &SessionEvent::AssistantMessage {
                    turn_id: TurnId::from_raw("turn-1"),
                    content: "hello".into(),
                    at: ts("2026-05-21T12:00:02Z"),
                },
            )
            .unwrap();

        let mut buf = Vec::new();
        cmd_sessions_show(&store, &init.id, false, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("session-show"));
        assert!(out.contains("hi"));
        assert!(out.contains("hello"));
    }

    #[test]
    fn show_missing_session_returns_store_error() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path());
        let mut buf = Vec::new();
        let err = cmd_sessions_show(
            &store,
            &SessionId::from_raw("session-ghost"),
            false,
            &mut buf,
        )
        .unwrap_err();
        assert!(matches!(err, SessionsCmdError::Store(_)));
    }
}

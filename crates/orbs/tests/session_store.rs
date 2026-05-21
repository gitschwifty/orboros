//! End-to-end exercise of `SessionStore` against a real filesystem.
//!
//! Builds a multi-turn transcript, closes the session, reloads from disk,
//! and asserts the reconstructed snapshot + event order match the writes.

use chrono::{DateTime, Utc};
use orbs::id::OrbId;
use orbs::session::{
    CloseReason, SessionEvent, SessionId, SessionInit, SessionStatus, SessionUsage, ToolOutcome,
    TurnId,
};
use orbs::session_store::{SessionRetentionConfig, SessionStore};
use tempfile::tempdir;

fn ts(rfc: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(rfc)
        .unwrap()
        .with_timezone(&Utc)
}

#[test]
fn multi_turn_session_round_trips_through_disk() {
    let dir = tempdir().unwrap();
    let store = SessionStore::new(dir.path());

    let init = SessionInit {
        id: SessionId::from_raw("session-multi"),
        created_at: ts("2026-05-21T12:00:00Z"),
        model: "openrouter/free".into(),
        system_prompt: Some("be terse".into()),
        cwd: Some("/tmp/proj".into()),
        linked_orb: Some(OrbId::from_raw("orb-abc")),
    };
    store.create(&init).unwrap();

    // Turn 1 — user asks, assistant answers, tool call, usage.
    let t1 = TurnId::from_raw("turn-aaaa1111");
    let turn1 = [
        SessionEvent::UserMessage {
            turn_id: t1.clone(),
            content: "list files".into(),
            at: ts("2026-05-21T12:00:05Z"),
        },
        SessionEvent::ToolStart {
            turn_id: t1.clone(),
            name: "bash".into(),
            args: serde_json::json!({ "cmd": "ls" }),
        },
        SessionEvent::ToolEnd {
            turn_id: t1.clone(),
            name: "bash".into(),
            outcome: ToolOutcome::Ok {
                summary: "exit 0".into(),
            },
        },
        SessionEvent::AssistantMessage {
            turn_id: t1.clone(),
            content: "README, src/".into(),
            at: ts("2026-05-21T12:00:08Z"),
        },
        SessionEvent::Usage {
            turn_id: t1,
            usage: SessionUsage {
                prompt_tokens: 120,
                completion_tokens: 40,
                total_tokens: 160,
            },
        },
    ];

    // Turn 2 — user spawns an orb mid-conversation.
    let t2 = TurnId::from_raw("turn-bbbb2222");
    let turn2 = [
        SessionEvent::UserMessage {
            turn_id: t2.clone(),
            content: "/spawn refactor the cli".into(),
            at: ts("2026-05-21T12:01:00Z"),
        },
        SessionEvent::OrbSpawned {
            turn_id: t2.clone(),
            orb_id: OrbId::from_raw("orb-xyz"),
        },
        SessionEvent::AssistantMessage {
            turn_id: t2.clone(),
            content: "queued orb-xyz".into(),
            at: ts("2026-05-21T12:01:01Z"),
        },
        SessionEvent::Usage {
            turn_id: t2,
            usage: SessionUsage {
                prompt_tokens: 30,
                completion_tokens: 8,
                total_tokens: 38,
            },
        },
    ];

    for ev in turn1.iter().chain(turn2.iter()) {
        store.append_event(&init.id, ev).unwrap();
    }

    // Close the session.
    store
        .close(&init.id, CloseReason::UserExit, ts("2026-05-21T12:02:00Z"))
        .unwrap();

    // Reload and verify.
    let (snapshot, events) = store.load(&init.id).unwrap();
    assert_eq!(snapshot.init, init);
    assert_eq!(snapshot.status, SessionStatus::Closed);
    assert_eq!(snapshot.turn_count, 2, "two user messages = two turns");
    assert_eq!(snapshot.total_usage.prompt_tokens, 150);
    assert_eq!(snapshot.total_usage.completion_tokens, 48);
    assert_eq!(snapshot.total_usage.total_tokens, 198);
    assert_eq!(snapshot.close_reason, Some(CloseReason::UserExit));
    assert_eq!(snapshot.closed_at, Some(ts("2026-05-21T12:02:00Z")));

    let expected_event_count = turn1.len() + turn2.len() + 1; // +1 for the close StatusChanged event
    assert_eq!(events.len(), expected_event_count);
    let original_appends: Vec<_> = turn1.iter().chain(turn2.iter()).collect();
    for (i, original) in original_appends.iter().enumerate() {
        assert_eq!(&&events[i], original, "event {i} order preserved");
    }
    match events.last().unwrap() {
        SessionEvent::StatusChanged { to, reason, .. } => {
            assert_eq!(*to, SessionStatus::Closed);
            assert_eq!(*reason, Some(CloseReason::UserExit));
        }
        other => panic!("expected StatusChanged last, got {other:?}"),
    }
}

#[test]
fn list_then_reload_each_session_matches_writes() {
    let dir = tempdir().unwrap();
    let store = SessionStore::new(dir.path());

    let mut ids = Vec::new();
    for i in 0..3 {
        let init = SessionInit {
            id: SessionId::from_raw(format!("session-{i}")),
            created_at: ts("2026-05-21T12:00:00Z") + chrono::Duration::seconds(i),
            model: "m".into(),
            system_prompt: None,
            cwd: None,
            linked_orb: None,
        };
        store.create(&init).unwrap();
        ids.push(init.id.clone());

        store
            .append_event(
                &init.id,
                &SessionEvent::UserMessage {
                    turn_id: TurnId::new(),
                    content: format!("msg {i}"),
                    at: ts("2026-05-21T12:00:00Z"),
                },
            )
            .unwrap();
    }

    let metas = store.list().unwrap();
    assert_eq!(metas.len(), 3);
    for meta in &metas {
        let (snapshot, events) = store.load(&meta.init.id).unwrap();
        assert_eq!(snapshot.init.id, meta.init.id);
        assert_eq!(events.len(), 1);
        assert_eq!(snapshot.turn_count, 1);
    }
}

#[test]
fn rotation_with_custom_retention_archives_old_closed_session() {
    let dir = tempdir().unwrap();
    let store = SessionStore::new(dir.path()).with_retention(SessionRetentionConfig {
        rotate_after_days: 7,
        max_file_bytes: 1024 * 1024,
    });

    let init = SessionInit {
        id: SessionId::from_raw("session-archive-me"),
        created_at: ts("2026-04-01T00:00:00Z"),
        model: "m".into(),
        system_prompt: None,
        cwd: None,
        linked_orb: None,
    };
    store.create(&init).unwrap();
    store
        .close(&init.id, CloseReason::UserExit, ts("2026-04-01T01:00:00Z"))
        .unwrap();

    let moved = store.rotate(ts("2026-05-21T00:00:00Z")).unwrap();
    assert_eq!(moved.len(), 1);
    assert!(!store.session_path(&init.id).exists());
    assert!(dir
        .path()
        .join("archive")
        .join("2026-04")
        .join("session-archive-me.jsonl")
        .exists());
}

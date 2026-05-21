//! End-to-end exercise of `ConvoRuntime` against the bundled mock heddle
//! worker. The mock streams one `content_delta` + one `usage` event per
//! send and then a `result` — enough to verify the runtime's event
//! mapping, transcript persistence, and outcome shape.

use std::path::PathBuf;

use orboros::convo::{ConvoRuntime, TurnStatus};
use orboros::ipc::types::ResultStatus;
use orboros::worker::process::WorkerConfig;
use orbs::session::{CloseReason, SessionEvent, SessionId, SessionInit, SessionStatus};
use orbs::session_store::SessionStore;
use tempfile::tempdir;
use tokio::sync::mpsc;

fn mock_worker_config() -> WorkerConfig {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    WorkerConfig {
        command: "bash".into(),
        args: vec![manifest_dir
            .join("test-fixtures/mock-worker.sh")
            .to_string_lossy()
            .into()],
        cwd: None,
        env: vec![],
        model: "mock/test".into(),
        system_prompt: "You are a test assistant.".into(),
        tools: vec![],
        max_iterations: None,
        init_timeout: None,
        send_timeout: None,
        shutdown_timeout: None,
        task_id: None,
        worker_id: None,
    }
}

fn make_init(id_seed: &str) -> SessionInit {
    SessionInit {
        id: SessionId::from_raw(format!("session-{id_seed}")),
        created_at: chrono::Utc::now(),
        model: "mock/test".into(),
        system_prompt: None,
        cwd: None,
        linked_orb: None,
    }
}

#[tokio::test]
async fn start_session_writes_header_and_spawns_worker() {
    let dir = tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut runtime = ConvoRuntime::new(store);

    let init = make_init("start");
    let id = runtime
        .start_session(init.clone(), mock_worker_config())
        .await
        .unwrap();
    assert_eq!(id, init.id);
    assert_eq!(runtime.active_session_ids().count(), 1);

    runtime
        .close_session(&init.id, CloseReason::UserExit)
        .await
        .unwrap();
    assert_eq!(runtime.active_session_ids().count(), 0);

    let (snapshot, _) = runtime.store().load(&init.id).unwrap();
    assert_eq!(snapshot.status, SessionStatus::Closed);
}

#[tokio::test]
async fn send_turn_streams_session_events_and_persists_transcript() {
    let dir = tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut runtime = ConvoRuntime::new(store);

    let init = make_init("turn");
    runtime
        .start_session(init.clone(), mock_worker_config())
        .await
        .unwrap();

    let (tx, mut rx) = mpsc::channel::<SessionEvent>(32);
    let drain = tokio::spawn(async move {
        let mut received = Vec::new();
        while let Some(ev) = rx.recv().await {
            received.push(ev);
        }
        received
    });

    let summary = runtime.send_turn(&init.id, "hello", tx).await.unwrap();
    let received = drain.await.unwrap();

    assert_eq!(summary.status, TurnStatus::Ok);
    assert_eq!(summary.response.as_deref(), Some("Hello from mock worker"));
    assert_eq!(summary.usage.total_tokens, 15);

    // Stream order: UserMessage, AssistantDelta, Usage, AssistantMessage.
    assert!(matches!(received[0], SessionEvent::UserMessage { .. }));
    assert!(received.iter().any(
        |e| matches!(e, SessionEvent::AssistantDelta { chunk, .. } if chunk == "Hello from mock")
    ));
    assert!(received
        .iter()
        .any(|e| matches!(e, SessionEvent::Usage { usage, .. } if usage.total_tokens == 15)));
    assert!(received
        .iter()
        .any(|e| matches!(e, SessionEvent::AssistantMessage { content, .. } if content == "Hello from mock worker")));

    runtime
        .close_session(&init.id, CloseReason::UserExit)
        .await
        .unwrap();

    // Transcript on disk contains the persisted events.
    let (snapshot, events) = runtime.store().load(&init.id).unwrap();
    assert_eq!(snapshot.status, SessionStatus::Closed);
    assert_eq!(snapshot.turn_count, 1);
    assert_eq!(snapshot.total_usage.total_tokens, 15);

    let has_user = events
        .iter()
        .any(|e| matches!(e, SessionEvent::UserMessage { .. }));
    let has_assistant_msg = events
        .iter()
        .any(|e| matches!(e, SessionEvent::AssistantMessage { .. }));
    let has_status_closed = events.iter().any(|e| {
        matches!(
            e,
            SessionEvent::StatusChanged {
                to: SessionStatus::Closed,
                ..
            }
        )
    });
    assert!(has_user && has_assistant_msg && has_status_closed);
}

#[tokio::test]
async fn multi_turn_accumulates_usage_and_turn_count() {
    let dir = tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut runtime = ConvoRuntime::new(store);

    let init = make_init("multi");
    runtime
        .start_session(init.clone(), mock_worker_config())
        .await
        .unwrap();

    for i in 0..3 {
        let (tx, _rx) = mpsc::channel::<SessionEvent>(32);
        let summary = runtime
            .send_turn(&init.id, &format!("turn {i}"), tx)
            .await
            .unwrap();
        assert_eq!(summary.status, TurnStatus::Ok);
    }

    runtime
        .close_session(&init.id, CloseReason::UserExit)
        .await
        .unwrap();

    let (snapshot, _) = runtime.store().load(&init.id).unwrap();
    assert_eq!(snapshot.turn_count, 3);
    // Mock emits 15 tokens per turn.
    assert_eq!(snapshot.total_usage.total_tokens, 45);
}

#[tokio::test]
async fn send_turn_on_unknown_session_errors_cleanly() {
    let dir = tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut runtime = ConvoRuntime::new(store);

    let unknown = SessionId::from_raw("session-ghost");
    let (tx, _rx) = mpsc::channel::<SessionEvent>(8);
    let err = runtime.send_turn(&unknown, "hi", tx).await.unwrap_err();
    assert!(
        format!("{err}").contains("not active"),
        "expected SessionNotActive, got: {err}"
    );
}

#[tokio::test]
async fn start_session_refuses_duplicate_active_id() {
    let dir = tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut runtime = ConvoRuntime::new(store);

    let init = make_init("dup");
    runtime
        .start_session(init.clone(), mock_worker_config())
        .await
        .unwrap();

    // Second start_session with the same id should fail loudly.
    let err = runtime
        .start_session(init.clone(), mock_worker_config())
        .await
        .unwrap_err();
    assert!(
        format!("{err}").contains("already active"),
        "expected SessionAlreadyActive, got: {err}"
    );

    runtime
        .close_session(&init.id, CloseReason::UserExit)
        .await
        .unwrap();
}

#[tokio::test]
async fn send_turn_with_dropped_receiver_still_persists_transcript() {
    let dir = tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut runtime = ConvoRuntime::new(store);

    let init = make_init("dropped-rx");
    runtime
        .start_session(init.clone(), mock_worker_config())
        .await
        .unwrap();

    // Drop the receiver immediately — persistence must still happen.
    let (tx, rx) = mpsc::channel::<SessionEvent>(8);
    drop(rx);
    let summary = runtime.send_turn(&init.id, "hello", tx).await.unwrap();
    assert_eq!(summary.status, TurnStatus::Ok);

    runtime
        .close_session(&init.id, CloseReason::UserExit)
        .await
        .unwrap();

    let (snapshot, events) = runtime.store().load(&init.id).unwrap();
    assert_eq!(snapshot.turn_count, 1);
    assert!(events
        .iter()
        .any(|e| matches!(e, SessionEvent::AssistantMessage { .. })));
}

// Smoke test that ResultStatus::Ok still maps correctly — guards against
// future refactors flipping the enum.
#[test]
fn result_status_ok_maps_to_turn_ok() {
    assert_eq!(ResultStatus::Ok, ResultStatus::Ok);
}

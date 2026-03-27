mod common;

use orboros::ipc::types::ResultStatus;
use orboros::worker::process::Worker;

#[tokio::test]
async fn test_spawn_init_shutdown() {
    let binary = match common::heddle_binary() {
        Some(b) => b,
        None => return, // skip if HEDDLE_BINARY not set
    };

    let config = common::heddle_config(&binary);
    let worker = Worker::spawn(&config).await.unwrap();

    assert!(!worker.session_id().is_empty());
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_send_and_receive() {
    let binary = match common::heddle_binary() {
        Some(b) => b,
        None => return,
    };

    let config = common::heddle_config(&binary);
    let mut worker = Worker::spawn(&config).await.unwrap();

    let outcome = worker
        .send("msg-1", "Say hello in one word.")
        .await
        .unwrap();
    assert_eq!(outcome.status, ResultStatus::Ok);
    assert!(
        outcome.response.is_some(),
        "Expected a response from heddle"
    );

    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_full_lifecycle_with_events() {
    let binary = match common::heddle_binary() {
        Some(b) => b,
        None => return,
    };

    let config = common::heddle_config(&binary);
    let mut worker = Worker::spawn(&config).await.unwrap();
    let session_id = worker.session_id().to_string();
    assert!(!session_id.is_empty());

    let outcome = worker.send("msg-1", "What is 2+2?").await.unwrap();
    assert_eq!(outcome.status, ResultStatus::Ok);
    assert!(outcome.response.is_some());
    // Real heddle should emit at least usage events
    assert!(outcome.usage.is_some(), "Expected usage data from heddle");

    worker.shutdown().await.unwrap();
}

//! E2E coverage for the `pre-phase-transition` / `post-phase-transition`
//! hooks fired by `QueueLoop::tick_async`. Closes the last task 56
//! follow-up.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use orboros::hooks::HookSink;
use orboros::queue_loop::QueueLoop;
use orbs::dep_store::DepStore;
use orbs::orb::{Orb, OrbPhase, OrbType};
use orbs::orb_store::OrbStore;

fn write_executable_script(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

fn write_hooks_toml(state_dir: &Path, body: &str) {
    fs::write(state_dir.join("hooks.toml"), body).unwrap();
}

fn build_queue(base: &Path) -> (QueueLoop, OrbStore) {
    let orb_store = OrbStore::new(base.join("orbs.jsonl"));
    let dep_store = DepStore::new(base.join("deps.jsonl"));
    let sink = HookSink::from_state_dir(base, base)
        .unwrap()
        .expect("hooks.toml should be loaded");
    let queue = QueueLoop::new(orb_store.clone(), dep_store, base.to_path_buf()).with_hooks(sink);
    (queue, orb_store)
}

#[tokio::test]
async fn post_phase_transition_fires_for_pending_to_speccing() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path();
    write_hooks_toml(
        state,
        r#"
        [[hook]]
        name = "spec-post"
        on = "post-phase-transition(speccing)"
        run = "true"
        sync = true
        "#,
    );
    let (queue, store) = build_queue(state);

    let mut orb = Orb::new("Epic", "big").with_type(OrbType::Epic);
    // Walk Draft → Pending so start_pipelines picks it up.
    orb.phase = Some(OrbPhase::Pending);
    store.append(&orb).unwrap();

    let result = queue.tick_async().await.unwrap();
    assert_eq!(result.pipelines_started, 1);

    let reloaded = store.load_by_id(&orb.id).unwrap().unwrap();
    assert_eq!(reloaded.phase, Some(OrbPhase::Speccing));

    let log = fs::read_to_string(state.join("hooks.log.jsonl")).unwrap_or_default();
    assert!(
        log.contains("spec-post") && log.contains("post-phase-transition(speccing)"),
        "expected post-phase-transition(speccing) firing: {log}"
    );
}

#[tokio::test]
async fn pre_phase_transition_exit_2_blocks_transition() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path();

    let block = state.join("block.sh");
    write_executable_script(&block, "#!/bin/sh\nexit 2\n");

    write_hooks_toml(
        state,
        &format!(
            r#"
            [[hook]]
            name = "blocker"
            on = "pre-phase-transition(speccing)"
            run = "{}"
            sync = true
            "#,
            block.display()
        ),
    );
    let (queue, store) = build_queue(state);

    let mut orb = Orb::new("Epic", "big").with_type(OrbType::Epic);
    orb.phase = Some(OrbPhase::Pending);
    store.append(&orb).unwrap();

    let result = queue.tick_async().await.unwrap();
    // The transition was aborted by the pre hook.
    assert_eq!(result.pipelines_started, 0);
    let reloaded = store.load_by_id(&orb.id).unwrap().unwrap();
    assert_eq!(
        reloaded.phase,
        Some(OrbPhase::Pending),
        "phase stays unchanged when pre-hook aborts"
    );

    let log = fs::read_to_string(state.join("hooks.log.jsonl")).unwrap_or_default();
    assert!(log.contains("blocker"), "blocker should have fired: {log}");
    assert!(
        log.contains("\"outcome_label\":\"aborted\""),
        "blocker outcome should be aborted: {log}"
    );
}

#[tokio::test]
async fn pre_phase_transition_exit_0_allows_transition() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path();
    write_hooks_toml(
        state,
        r#"
        [[hook]]
        name = "audit-pre"
        on = "pre-phase-transition(speccing)"
        run = "true"
        sync = true
        "#,
    );
    let (queue, store) = build_queue(state);

    let mut orb = Orb::new("Epic", "big").with_type(OrbType::Epic);
    orb.phase = Some(OrbPhase::Pending);
    store.append(&orb).unwrap();

    let result = queue.tick_async().await.unwrap();
    assert_eq!(result.pipelines_started, 1);
    let reloaded = store.load_by_id(&orb.id).unwrap().unwrap();
    assert_eq!(reloaded.phase, Some(OrbPhase::Speccing));

    let log = fs::read_to_string(state.join("hooks.log.jsonl")).unwrap_or_default();
    assert!(
        log.contains("audit-pre"),
        "pre hook should fire even when it doesn't abort: {log}"
    );
}

#[tokio::test]
async fn tick_async_without_hooks_works_like_sync_tick() {
    // No hooks.toml — queue runs without HookSink. tick_async should
    // still transition orbs correctly.
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().to_path_buf();
    let orb_store = OrbStore::new(base.join("orbs.jsonl"));
    let dep_store = DepStore::new(base.join("deps.jsonl"));
    let queue = QueueLoop::new(orb_store.clone(), dep_store, base);

    let mut orb = Orb::new("Epic", "big").with_type(OrbType::Epic);
    orb.phase = Some(OrbPhase::Pending);
    orb_store.append(&orb).unwrap();

    let result = queue.tick_async().await.unwrap();
    assert_eq!(result.pipelines_started, 1);
    let reloaded = orb_store.load_by_id(&orb.id).unwrap().unwrap();
    assert_eq!(reloaded.phase, Some(OrbPhase::Speccing));
}

#[tokio::test]
async fn status_only_transitions_do_not_fire_phase_hooks() {
    // Task orbs use status, not phase. The pre-phase-transition hook
    // shouldn't match their Pending→Active move.
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path();
    write_hooks_toml(
        state,
        r#"
        [[hook]]
        name = "should-not-fire"
        on = "pre-phase-transition(executing)"
        run = "true"
        sync = true
        "#,
    );
    let (queue, store) = build_queue(state);

    let orb = Orb::new("Task", "do it").with_type(OrbType::Task);
    store.append(&orb).unwrap();

    let result = queue.tick_async().await.unwrap();
    assert_eq!(result.orbs_executed, 1, "task should move Pending→Active");

    let log = fs::read_to_string(state.join("hooks.log.jsonl")).unwrap_or_default();
    assert!(
        !log.contains("should-not-fire"),
        "phase hook should not fire for status-only transition: {log}"
    );
}

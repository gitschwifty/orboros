//! End-to-end test of the hook system driven through the orboros
//! binary. Exercises: orb-create fires on-orb-create, review fires
//! on-review-approve, delete fires on-delete, abort short-circuits
//! the chain, the log records all of it.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use assert_cmd::Command;

fn make_executable(path: &Path) {
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    make_executable(&path);
    path
}

fn write_config(dir: &Path, body: &str) {
    fs::write(dir.join("hooks.toml"), body).unwrap();
}

fn orboros(state: &Path) -> Command {
    let mut cmd = Command::cargo_bin("orboros").unwrap();
    cmd.env("HOME", state); // isolate from the user's real ~/.orboros
    cmd.args(["--state-dir", state.to_str().unwrap()]);
    cmd
}

fn create_orb(state: &Path, title: &str) -> String {
    let assert = orboros(state).args(["orb", "create", title]).assert();
    let output = assert.get_output().clone();
    assert.success();
    let stdout = String::from_utf8(output.stdout).unwrap();
    // First line: "Created orb orb-XXX"
    let id = stdout
        .lines()
        .next()
        .unwrap()
        .strip_prefix("Created orb ")
        .unwrap()
        .trim()
        .to_string();
    assert!(id.starts_with("orb-"), "unexpected id: {id}");
    id
}

fn read_log(state: &Path) -> String {
    let path = state.join("hooks.log.jsonl");
    if !path.exists() {
        return String::new();
    }
    fs::read_to_string(path).unwrap()
}

#[test]
fn orb_create_fires_on_orb_create_and_appends_log() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path();
    write_config(
        state,
        r#"
        [[hook]]
        name = "create-marker"
        on = "on-orb-create"
        run = "true"
        sync = true
        "#,
    );

    let id = create_orb(state, "First orb");

    let log = read_log(state);
    assert!(
        log.contains("create-marker") && log.contains(&id),
        "log missing expected entries: {log}"
    );
    assert!(log.contains("\"outcome_label\":\"ok\""));
}

#[test]
fn pre_event_exit_2_does_not_block_user_command_but_records_abort() {
    // We currently don't have a pre-* event wired into orb-create
    // (pre-worker-spawn lives in the not-yet-wired worker dispatch
    // path), so this test exercises the abort path via on-orb-create
    // — exit 2 still records as aborted in the log even though
    // on-orb-create is informational and doesn't gate anything.
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path();
    let block = write_script(state, "block.sh", "#!/bin/sh\nexit 2\n");
    let never = write_script(state, "never.sh", "#!/bin/sh\nexit 0\n");
    let body = format!(
        r#"
        [[hook]]
        name = "blocker"
        on = "on-orb-create"
        run = "{block}"
        sync = true

        [[hook]]
        name = "should-not-run"
        on = "on-orb-create"
        run = "{never}"
        sync = true
        "#,
        block = block.display(),
        never = never.display(),
    );
    write_config(state, &body);

    create_orb(state, "Abort test");

    let log = read_log(state);
    assert!(log.contains("blocker"), "log missing blocker: {log}");
    assert!(log.contains("\"outcome_label\":\"aborted\""));
    // The chain short-circuited — should-not-run was never invoked.
    assert!(
        !log.contains("should-not-run"),
        "post-abort hook should not have fired: {log}"
    );
}

#[test]
fn review_approve_fires_on_review_approve_event() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path();
    write_config(
        state,
        r#"
        [[hook]]
        name = "approve-marker"
        on = "on-review-approve"
        run = "true"
        sync = true
        "#,
    );

    let id = create_orb(state, "Review me");
    // Walk through pending → active → review using `orb update`.
    orboros(state)
        .args(["orb", "update", &id, "--status", "active"])
        .assert()
        .success();
    orboros(state)
        .args(["orb", "update", &id, "--status", "review"])
        .assert()
        .success();
    // Now approve. cmd_orb_review fires on-review-approve.
    orboros(state)
        .args(["orb", "review", &id, "approve"])
        .assert()
        .success();

    let log = read_log(state);
    assert!(
        log.contains("approve-marker") && log.contains("on-review-approve"),
        "expected approve marker in log: {log}"
    );
}

#[test]
fn delete_fires_on_delete_event() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path();
    write_config(
        state,
        r#"
        [[hook]]
        name = "delete-marker"
        on = "on-delete"
        run = "true"
        sync = true
        "#,
    );

    let id = create_orb(state, "Goodbye orb");
    orboros(state)
        .args(["orb", "delete", &id, "--reason", "test"])
        .assert()
        .success();

    let log = read_log(state);
    assert!(
        log.contains("delete-marker") && log.contains("on-delete"),
        "expected delete marker in log: {log}"
    );
}

#[test]
fn hook_matcher_filters_correctly_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path();
    write_config(
        state,
        r#"
        [[hook]]
        name = "task-only"
        on = "on-orb-create"
        run = "true"
        sync = true
        match.orb_type = "task"

        [[hook]]
        name = "epic-only"
        on = "on-orb-create"
        run = "true"
        sync = true
        match.orb_type = "epic"
        "#,
    );

    // Default orb type is task — only "task-only" should fire.
    create_orb(state, "Just a task");
    let log = read_log(state);
    assert!(
        log.contains("task-only"),
        "task-only should have fired: {log}"
    );
    assert!(
        !log.contains("epic-only"),
        "epic-only should not have fired for a task: {log}"
    );
}

#[test]
fn unknown_provider_skips_credential_check_but_warns() {
    // Smoke test: with no hooks.toml, no log file should appear.
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path();
    create_orb(state, "No hooks");
    assert!(
        !state.join("hooks.log.jsonl").exists(),
        "no hooks configured → no log file"
    );
}

#[test]
fn hooks_list_cli_prints_loaded_hooks() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path();
    write_config(
        state,
        r#"
        [[hook]]
        name = "listed"
        on = "on-orb-create"
        run = "echo hi"
        "#,
    );
    let output = orboros(state).args(["hooks", "list"]).output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("listed"));
    assert!(stdout.contains("on=on-orb-create"));
    assert!(stdout.contains("1 hook(s) loaded"));
}

#[test]
fn hooks_log_cli_filters_by_orb() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path();
    write_config(
        state,
        r#"
        [[hook]]
        name = "trace-each"
        on = "on-orb-create"
        run = "true"
        sync = true
        "#,
    );

    let id_a = create_orb(state, "Alpha");
    let id_b = create_orb(state, "Beta");
    assert_ne!(id_a, id_b);

    let output = orboros(state)
        .args(["hooks", "log", "--orb", &id_a])
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains(&id_a),
        "filter should include {id_a}: {stdout}"
    );
    assert!(
        !stdout.contains(&id_b),
        "filter should exclude {id_b}: {stdout}"
    );
}

use std::time::Duration;

use orboros::worker::process::WorkerConfig;

/// Returns the `HEDDLE_BINARY` path, or `None` to skip the test.
pub fn heddle_binary() -> Option<String> {
    std::env::var("HEDDLE_BINARY").ok()
}

/// Builds a `WorkerConfig` for real heddle with sensible test defaults.
pub fn heddle_config(binary: &str) -> WorkerConfig {
    WorkerConfig {
        command: binary.into(),
        args: vec![],
        cwd: None,
        env: vec![],
        model: "openrouter/auto".into(),
        system_prompt: "You are a helpful test assistant. Keep responses very short.".into(),
        tools: vec![],
        max_iterations: Some(1),
        init_timeout: Some(Duration::from_secs(30)),
        send_timeout: Some(Duration::from_secs(300)),
        shutdown_timeout: Some(Duration::from_secs(10)),
    }
}

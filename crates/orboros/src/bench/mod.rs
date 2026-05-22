//! Benchmark corpus + harness (task 59).
//!
//! Three tiers of orboros benchmarks:
//! - **T1** — Single-shot tasks (~10–20 cases). Self-contained prompts
//!   with known-good answers. Tests pure worker behavior.
//! - **T2** — Modify-existing-project (~5–10). Seed repo + expected
//!   diff or test pass.
//! - **T3** — Greenfield (~2–5). Prompt → expected artifacts, graded
//!   by a rubric grader (typically a cheaper model).
//!
//! Result store: JSONL under `.orbs/bench/`. CLI: `orboros bench
//! list / run / show / compare / calibration`.

pub mod case;
pub mod runner;
pub mod runner_t2t3;
pub mod store;

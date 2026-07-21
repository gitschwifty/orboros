//! Benchmark corpus + harness (task 59).
//!
//! Three tiers of orboros benchmarks:
//! - **T1** — Single-shot tasks (~10–20 cases). Self-contained prompts
//!   with known-good answers. Tests pure worker behavior.
//! - **T2** — Targeted Orboros capabilities in seeded repos (~5–10).
//!   Seed repo + expected diff or test pass.
//! - **T3** — Full end-to-end Orboros runs (~2–5). Short-prompt
//!   greenfield cases and prewritten plan/spec cases both use normal
//!   Orboros execution under benchmark isolation, graded by artifact
//!   checks or a rubric grader.
//!
//! Result store: JSONL under `.orbs/bench/`. CLI: `orboros bench
//! list / run / show / compare / calibration`.

pub mod calibration;
pub mod case;
pub mod cmd;
pub mod runner;
pub mod runner_t2t3;
pub mod store;

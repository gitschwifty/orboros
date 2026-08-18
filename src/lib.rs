#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]
// Public CLI and IPC boundaries deliberately use Result-returning helpers,
// compact enums, and a few orchestration functions whose shape is clearer
// than splitting them merely to satisfy style thresholds.
#![allow(
    clippy::large_enum_variant,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::too_many_arguments,
    clippy::too_many_lines
)]

pub mod bench;
pub mod config;
pub mod convo;
pub mod coordinator;
pub mod daemon;
pub mod execution;
pub mod hooks;
pub mod ipc;
pub mod notify;
pub mod orb_cmd;
pub mod phases;
pub mod plan;
pub mod prompt;
pub mod prompt_context;
pub mod queue_loop;
pub mod routing;
pub mod second_opinion_trigger;
pub mod slop;
pub mod startup_check;
pub mod tracing_ctx;
pub mod worker;

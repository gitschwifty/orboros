#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]

pub mod config;
pub mod coordinator;
pub mod ipc;
pub mod orchestrator;
pub mod phases;
pub mod plan;
pub mod queue_loop;
pub mod routing;
pub mod runner;
pub mod state;
pub mod trace;
pub mod worker;

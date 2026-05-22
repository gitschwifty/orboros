//! Lifecycle hooks — TOML-configured side effects fired at orb
//! lifecycle events.
//!
//! Design doc: `private/design/lifecycle-hooks.md`. Sub-tasks land
//! incrementally; this initial commit lands the config types and
//! loader. Matcher evaluation and the execution engine follow in
//! their own commits.

pub mod config;
pub mod event;

pub use config::{default_paths, ConfigLayer, HookEntry, HookMatch, HooksConfig};
pub use event::{HookEvent, HookEventParseError};

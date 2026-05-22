//! Lifecycle hooks — TOML-configured side effects fired at orb
//! lifecycle events.
//!
//! Design doc: `private/design/lifecycle-hooks.md`. Sub-tasks land
//! incrementally; this initial commit lands the config types and
//! loader. Matcher evaluation and the execution engine follow in
//! their own commits.

pub mod config;
pub mod event;
pub mod matcher;
pub mod runner;
pub mod sink;

pub use config::{default_paths, ConfigLayer, HookEntry, HookMatch, HooksConfig};
pub use event::{HookEvent, HookEventParseError};
pub use matcher::{matches, MatcherCtx};
pub use runner::{fire, preview, FireCtx, FireOutcome, HookInvocation, HookPreview};
pub use sink::HookSink;

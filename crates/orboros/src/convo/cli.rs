//! Interactive `orboros chat` REPL.
//!
//! Owns a `rustyline` editor, a `ConvoRuntime`, and a `Renderer`. Each
//! iteration: read a line, dispatch slash commands locally, otherwise
//! send the line as a user turn and stream events through the renderer
//! until the turn's `Result` arrives. Status info is appended once per
//! turn.

use std::io::{self, IsTerminal};
use std::time::Duration;

use orbs::id::OrbId;
use orbs::orb::{Orb, OrbStatus};
use orbs::orb_store::OrbStore;
use orbs::session::{CloseReason, SessionEvent, SessionId, SessionInit, TurnId};
use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;
use rustyline::Editor;
use tokio::sync::mpsc;

use crate::convo::render::Renderer;
use crate::convo::{ConvoError, ConvoRuntime};
use crate::worker::process::WorkerConfig;

const HELP_TEXT: &str = "/exit                    — close the session and quit\n\
                         /help                    — show this help\n\
                         /spawn <description>     — create an orb and continue chatting\n\
                         /await <orb-id>          — wait for an orb, inject its result\n\
                                                    into your next message";

/// Default time between polls when waiting on an orb via `/await`.
const AWAIT_POLL_INTERVAL: Duration = Duration::from_millis(750);

/// Default upper bound on `/await` wall time. The user can re-issue
/// `/await` if the orb is taking longer.
const AWAIT_TIMEOUT: Duration = Duration::from_secs(600);

/// Slash command parsed from a user line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Exit,
    Help,
    Spawn { description: String },
    Await { orb_id: String },
    MissingArg { name: String },
    Unknown(String),
}

/// Parses a slash command. Returns `Some(...)` only if the trimmed line
/// starts with `/`. Args (the rest of the line after the command name) are
/// captured for commands that need them; missing required args yield
/// `MissingArg` so the chat loop can surface a clear notice.
#[must_use]
pub fn parse_slash(line: &str) -> Option<SlashCommand> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix('/')?;
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let args_trimmed = args.trim();
    Some(match name {
        "exit" | "quit" => SlashCommand::Exit,
        "help" | "?" => SlashCommand::Help,
        "spawn" => {
            if args_trimmed.is_empty() {
                SlashCommand::MissingArg {
                    name: "spawn".into(),
                }
            } else {
                SlashCommand::Spawn {
                    description: args_trimmed.to_string(),
                }
            }
        }
        "await" => {
            if args_trimmed.is_empty() {
                SlashCommand::MissingArg {
                    name: "await".into(),
                }
            } else {
                SlashCommand::Await {
                    orb_id: args_trimmed.to_string(),
                }
            }
        }
        other => SlashCommand::Unknown(other.to_string()),
    })
}

/// Result returned by `await_orb` once polling finishes.
enum AwaitOutcome {
    Done(String),   // summary from the orb's `result` field (or default text)
    Failed(String), // failure summary
    TimedOut,       // wall clock budget exhausted
    NotFound,       // id never resolved
}

/// Drives an interactive chat session to completion.
///
/// Starts the session (writing the init header + spawning a worker),
/// loops on user input until `/exit` / Ctrl-D, then closes the session
/// cleanly.
///
/// # Errors
///
/// Returns `ConvoError` on session setup or per-turn failures. Returns
/// `anyhow::Error` for terminal/IO failures during interactive editing.
pub async fn run_chat(
    mut runtime: ConvoRuntime,
    init: SessionInit,
    worker_config: WorkerConfig,
    orb_store: Option<OrbStore>,
) -> anyhow::Result<()> {
    let session_id = init.id.clone();
    runtime.start_session(init, worker_config).await?;

    let stdout = io::stdout();
    let use_color = stdout.is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let mut renderer = Renderer::new(stdout.lock(), use_color);

    renderer.notice(&format!(
        "orboros chat — session {} — /help for commands",
        session_id.as_str()
    ))?;

    let mut editor: Editor<(), DefaultHistory> = Editor::new()?;
    let close_reason = chat_loop(
        &mut runtime,
        &session_id,
        &mut editor,
        &mut renderer,
        orb_store.as_ref(),
    )
    .await?;
    runtime.close_session(&session_id, close_reason).await?;
    renderer.notice("session closed.")?;
    Ok(())
}

async fn chat_loop<W: io::Write>(
    runtime: &mut ConvoRuntime,
    session_id: &SessionId,
    editor: &mut Editor<(), DefaultHistory>,
    renderer: &mut Renderer<W>,
    orb_store: Option<&OrbStore>,
) -> anyhow::Result<CloseReason> {
    // Buffered context to prepend to the next user turn (set by /await).
    let mut pending_context: Option<String> = None;

    loop {
        let line = match read_line_blocking(editor, "you> ").await? {
            ReadOutcome::Line(s) => s,
            ReadOutcome::Eof => return Ok(CloseReason::UserExit),
            ReadOutcome::Interrupted => {
                renderer.notice("(interrupted — type /exit to quit)")?;
                continue;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let _ = editor.add_history_entry(trimmed);

        if let Some(cmd) = parse_slash(trimmed) {
            match cmd {
                SlashCommand::Exit => return Ok(CloseReason::UserExit),
                SlashCommand::Help => renderer.notice(HELP_TEXT)?,
                SlashCommand::Spawn { description } => match orb_store {
                    Some(store) => match handle_spawn(runtime, session_id, store, &description) {
                        Ok(orb_id) => {
                            renderer.notice(&format!("spawned {orb_id}"))?;
                        }
                        Err(err) => {
                            renderer.notice(&format!("/spawn failed: {err}"))?;
                        }
                    },
                    None => {
                        renderer.notice("/spawn unavailable (no orb store configured)")?;
                    }
                },
                SlashCommand::Await { orb_id } => match orb_store {
                    Some(store) => {
                        match handle_await(runtime, session_id, store, &orb_id, renderer).await {
                            Ok(Some(block)) => {
                                pending_context = Some(block);
                                renderer
                                    .notice("orb result will be included in your next message.")?;
                            }
                            Ok(None) => {}
                            Err(err) => {
                                renderer.notice(&format!("/await failed: {err}"))?;
                            }
                        }
                    }
                    None => {
                        renderer.notice("/await unavailable (no orb store configured)")?;
                    }
                },
                SlashCommand::MissingArg { name } => {
                    renderer.notice(&format!("/{name} requires an argument (try /help)"))?;
                }
                SlashCommand::Unknown(name) => {
                    renderer.notice(&format!("unknown command: /{name} (try /help)"))?;
                }
            }
            continue;
        }

        let outgoing = compose_turn_message(pending_context.take(), trimmed);
        if let Err(err) = drive_turn(runtime, session_id, &outgoing, renderer).await {
            renderer.notice(&format!("turn failed: {err}"))?;
        }
    }
}

fn compose_turn_message(pending_context: Option<String>, user_line: &str) -> String {
    match pending_context {
        Some(block) => format!("{block}\n\n{user_line}"),
        None => user_line.to_string(),
    }
}

fn handle_spawn(
    runtime: &mut ConvoRuntime,
    session_id: &SessionId,
    orb_store: &OrbStore,
    description: &str,
) -> anyhow::Result<OrbId> {
    let title = description
        .lines()
        .next()
        .unwrap_or(description)
        .chars()
        .take(80)
        .collect::<String>();
    let orb = Orb::new(&title, description);
    orb_store
        .append(&orb)
        .map_err(|e| anyhow::anyhow!("failed to persist orb: {e}"))?;
    let orb_id = orb.id.clone();

    // Record the link in the transcript so the session has an auditable
    // pointer to the orb without coupling Orb's schema to sessions.
    let event = SessionEvent::OrbSpawned {
        turn_id: TurnId::new(),
        orb_id: orb_id.clone(),
    };
    runtime.append_session_event(session_id, &event)?;
    Ok(orb_id)
}

async fn handle_await<W: io::Write>(
    runtime: &mut ConvoRuntime,
    session_id: &SessionId,
    orb_store: &OrbStore,
    orb_id_raw: &str,
    renderer: &mut Renderer<W>,
) -> anyhow::Result<Option<String>> {
    let orb_id = OrbId::from_raw(orb_id_raw);
    renderer.notice(&format!("waiting for {orb_id_raw}... (Ctrl-C to skip)"))?;
    let outcome = await_orb(orb_store, &orb_id, AWAIT_POLL_INTERVAL, AWAIT_TIMEOUT).await;
    let (summary_event, injection) = match &outcome {
        AwaitOutcome::Done(summary) => {
            let block = format!("orb {orb_id_raw} completed:\n```\n{summary}\n```");
            (Some(summary.clone()), Some(block))
        }
        AwaitOutcome::Failed(summary) => {
            let block = format!("orb {orb_id_raw} failed:\n```\n{summary}\n```");
            (Some(summary.clone()), Some(block))
        }
        AwaitOutcome::TimedOut => {
            renderer.notice(&format!(
                "/await timed out after {}s — orb {orb_id_raw} is still in progress",
                AWAIT_TIMEOUT.as_secs()
            ))?;
            (None, None)
        }
        AwaitOutcome::NotFound => {
            renderer.notice(&format!("/await: orb {orb_id_raw} not found"))?;
            (None, None)
        }
    };
    if let Some(summary) = summary_event {
        let event = SessionEvent::OrbResult {
            turn_id: TurnId::new(),
            orb_id,
            summary,
        };
        runtime.append_session_event(session_id, &event)?;
        // Surface it on the terminal too.
        let _ = renderer.render(&event);
    }
    Ok(injection)
}

async fn await_orb(
    store: &OrbStore,
    orb_id: &OrbId,
    poll: Duration,
    budget: Duration,
) -> AwaitOutcome {
    let deadline = std::time::Instant::now() + budget;
    let mut ever_seen = false;
    loop {
        match store.load_by_id(orb_id) {
            Ok(Some(orb)) => {
                ever_seen = true;
                match orb.status {
                    Some(OrbStatus::Done) => {
                        return AwaitOutcome::Done(orb.result.unwrap_or_else(|| {
                            format!("(orb {} completed with no result text)", orb.id)
                        }));
                    }
                    Some(OrbStatus::Failed) => {
                        return AwaitOutcome::Failed(
                            orb.result
                                .unwrap_or_else(|| "(orb failed with no result text)".into()),
                        );
                    }
                    Some(OrbStatus::Cancelled | OrbStatus::Tombstone) => {
                        return AwaitOutcome::Failed(format!(
                            "(orb {} was {:?})",
                            orb.id,
                            orb.status.unwrap()
                        ));
                    }
                    _ => {}
                }
            }
            Ok(None) => {
                if !ever_seen && std::time::Instant::now() + poll > deadline {
                    return AwaitOutcome::NotFound;
                }
            }
            Err(_) => {
                // Treat transient read errors the same as "not yet visible".
            }
        }
        if std::time::Instant::now() >= deadline {
            return AwaitOutcome::TimedOut;
        }
        tokio::time::sleep(poll).await;
    }
}

async fn drive_turn<W: io::Write>(
    runtime: &mut ConvoRuntime,
    session_id: &SessionId,
    message: &str,
    renderer: &mut Renderer<W>,
) -> Result<(), ConvoError> {
    let (tx, mut rx) = mpsc::channel::<SessionEvent>(64);
    let mut send_fut = Box::pin(runtime.send_turn(session_id, message, tx));

    let summary = loop {
        tokio::select! {
            biased;
            maybe_event = rx.recv() => {
                if let Some(event) = maybe_event {
                    let _ = renderer.render(&event);
                } else {
                    // Sender dropped; the send future will resolve any moment.
                    let summary = (&mut send_fut).await?;
                    break summary;
                }
            }
            result = &mut send_fut => {
                break result?;
            }
        }
    };

    // Drain any events that arrived after the future resolved.
    while let Ok(event) = rx.try_recv() {
        let _ = renderer.render(&event);
    }

    let _ = renderer.render_status(&summary);
    Ok(())
}

enum ReadOutcome {
    Line(String),
    Eof,
    Interrupted,
}

async fn read_line_blocking(
    editor: &mut Editor<(), DefaultHistory>,
    prompt: &str,
) -> anyhow::Result<ReadOutcome> {
    // rustyline is blocking; run it on a blocking thread. We move the
    // editor in and out to keep history attached across turns without
    // needing an Arc<Mutex>.
    let editor_taken = std::mem::replace(editor, Editor::new()?);
    let prompt = prompt.to_string();
    let (editor_back, result) = tokio::task::spawn_blocking(move || {
        let mut ed = editor_taken;
        let r = ed.readline(&prompt);
        (ed, r)
    })
    .await?;
    *editor = editor_back;

    match result {
        Ok(line) => Ok(ReadOutcome::Line(line)),
        Err(ReadlineError::Eof) => Ok(ReadOutcome::Eof),
        Err(ReadlineError::Interrupted) => Ok(ReadOutcome::Interrupted),
        Err(other) => Err(anyhow::anyhow!("readline: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_slash_exit_variants() {
        assert_eq!(parse_slash("/exit"), Some(SlashCommand::Exit));
        assert_eq!(parse_slash("  /exit  "), Some(SlashCommand::Exit));
        assert_eq!(parse_slash("/quit"), Some(SlashCommand::Exit));
    }

    #[test]
    fn parse_slash_help_variants() {
        assert_eq!(parse_slash("/help"), Some(SlashCommand::Help));
        assert_eq!(parse_slash("/?"), Some(SlashCommand::Help));
    }

    #[test]
    fn parse_slash_unknown_preserves_name() {
        assert_eq!(
            parse_slash("/foo"),
            Some(SlashCommand::Unknown("foo".into()))
        );
    }

    #[test]
    fn parse_slash_spawn_captures_description() {
        assert_eq!(
            parse_slash("/spawn refactor the cli"),
            Some(SlashCommand::Spawn {
                description: "refactor the cli".into()
            })
        );
    }

    #[test]
    fn parse_slash_spawn_without_arg_yields_missing_arg() {
        assert_eq!(
            parse_slash("/spawn"),
            Some(SlashCommand::MissingArg {
                name: "spawn".into()
            })
        );
        assert_eq!(
            parse_slash("/spawn   "),
            Some(SlashCommand::MissingArg {
                name: "spawn".into()
            })
        );
    }

    #[test]
    fn parse_slash_await_captures_orb_id() {
        assert_eq!(
            parse_slash("/await orb-abc"),
            Some(SlashCommand::Await {
                orb_id: "orb-abc".into()
            })
        );
    }

    #[test]
    fn parse_slash_await_without_arg_yields_missing_arg() {
        assert_eq!(
            parse_slash("/await"),
            Some(SlashCommand::MissingArg {
                name: "await".into()
            })
        );
    }

    #[test]
    fn compose_turn_message_without_context_is_passthrough() {
        assert_eq!(compose_turn_message(None, "hello"), "hello");
    }

    #[test]
    fn compose_turn_message_with_context_prepends_block() {
        let composed = compose_turn_message(Some("orb-abc done: result".into()), "what next?");
        assert!(composed.starts_with("orb-abc done: result"));
        assert!(composed.ends_with("what next?"));
        assert!(composed.contains("\n\n"));
    }

    #[test]
    fn parse_slash_returns_none_on_non_slash_input() {
        assert_eq!(parse_slash("hello world"), None);
        assert_eq!(parse_slash(""), None);
        assert_eq!(parse_slash("  "), None);
        // A `/` in the middle of a line is not a slash command.
        assert_eq!(parse_slash("a / b"), None);
    }

    #[test]
    fn parse_slash_only_slash_with_no_name_is_unknown_empty() {
        assert_eq!(parse_slash("/"), Some(SlashCommand::Unknown(String::new())));
    }
}

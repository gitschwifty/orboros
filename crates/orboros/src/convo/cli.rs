//! Interactive `orboros chat` REPL.
//!
//! Owns a `rustyline` editor, a `ConvoRuntime`, and a `Renderer`. Each
//! iteration: read a line, dispatch slash commands locally, otherwise
//! send the line as a user turn and stream events through the renderer
//! until the turn's `Result` arrives. Status info is appended once per
//! turn.

use std::io::{self, IsTerminal};

use orbs::session::{CloseReason, SessionEvent, SessionId, SessionInit};
use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;
use rustyline::Editor;
use tokio::sync::mpsc;

use crate::convo::render::Renderer;
use crate::convo::{ConvoError, ConvoRuntime};
use crate::worker::process::WorkerConfig;

const HELP_TEXT: &str = "/exit       — close the session and quit\n\
                         /help       — show this help";

/// Slash command parsed from a user line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Exit,
    Help,
    Unknown(String),
}

/// Parses a slash command. Returns `Some(...)` only if the trimmed line
/// starts with `/`. Whitespace inside the line is preserved for future
/// arg-bearing commands.
#[must_use]
pub fn parse_slash(line: &str) -> Option<SlashCommand> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix('/')?;
    let (name, _args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    Some(match name {
        "exit" | "quit" => SlashCommand::Exit,
        "help" | "?" => SlashCommand::Help,
        other => SlashCommand::Unknown(other.to_string()),
    })
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
    let close_reason = chat_loop(&mut runtime, &session_id, &mut editor, &mut renderer).await?;
    runtime.close_session(&session_id, close_reason).await?;
    renderer.notice("session closed.")?;
    Ok(())
}

async fn chat_loop<W: io::Write>(
    runtime: &mut ConvoRuntime,
    session_id: &SessionId,
    editor: &mut Editor<(), DefaultHistory>,
    renderer: &mut Renderer<W>,
) -> anyhow::Result<CloseReason> {
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
                SlashCommand::Unknown(name) => {
                    renderer.notice(&format!("unknown command: /{name} (try /help)"))?;
                }
            }
            continue;
        }

        if let Err(err) = drive_turn(runtime, session_id, trimmed, renderer).await {
            renderer.notice(&format!("turn failed: {err}"))?;
        }
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
    fn parse_slash_unknown_strips_arg_tail() {
        // Args are dropped for now; v1 has no arg-bearing commands. The
        // command name still parses cleanly.
        assert_eq!(
            parse_slash("/spawn refactor the cli"),
            Some(SlashCommand::Unknown("spawn".into()))
        );
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

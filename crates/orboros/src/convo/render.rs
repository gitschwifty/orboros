//! Terminal renderer for `SessionEvent`s.
//!
//! Writes events to any `io::Write`. TTY-aware: emits ANSI styles when
//! `use_color` is true, otherwise plain text. Tool calls render as one-line
//! indented markers (`⟳ name` start, `✓ name` ok, `✗ name` err). Final
//! `AssistantMessage` events are followed by a dim per-turn status line
//! when a `TurnSummary` is rendered after — see `Renderer::render_status`.
//!
//! Status info rendered here is terminal output only. It is not persisted
//! into the transcript and not sent back to the worker as context.

use std::io::{self, Write};

use orbs::session::SessionEvent;

use crate::convo::TurnSummary;

const ANSI_RESET: &str = "\x1b[0m";
const ANSI_DIM: &str = "\x1b[2m";
const ANSI_BOLD: &str = "\x1b[1m";
const ANSI_YELLOW: &str = "\x1b[33m";
const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_RED: &str = "\x1b[31m";

/// Streaming terminal renderer.
pub struct Renderer<W: Write> {
    out: W,
    use_color: bool,
    /// True when the renderer is currently mid-stream of assistant deltas.
    /// Used to decide whether to insert a leading newline before a tool
    /// call interrupts the stream.
    mid_stream: bool,
}

impl<W: Write> Renderer<W> {
    pub fn new(out: W, use_color: bool) -> Self {
        Self {
            out,
            use_color,
            mid_stream: false,
        }
    }

    /// Renders one session event.
    ///
    /// # Errors
    ///
    /// Returns any `io::Error` produced by the underlying writer.
    pub fn render(&mut self, event: &SessionEvent) -> io::Result<()> {
        match event {
            SessionEvent::UserMessage { content, .. } => {
                self.break_stream()?;
                if self.use_color {
                    writeln!(self.out, "{ANSI_BOLD}you>{ANSI_RESET} {content}")?;
                } else {
                    writeln!(self.out, "you> {content}")?;
                }
            }
            SessionEvent::AssistantDelta { chunk, .. } => {
                write!(self.out, "{chunk}")?;
                self.mid_stream = true;
            }
            SessionEvent::AssistantMessage { content, .. } => {
                // If we already streamed the same content via deltas, just
                // close the line; otherwise print the full content.
                if self.mid_stream {
                    writeln!(self.out)?;
                    self.mid_stream = false;
                } else {
                    writeln!(self.out, "{content}")?;
                }
            }
            SessionEvent::ToolStart { name, .. } => {
                self.break_stream()?;
                if self.use_color {
                    writeln!(self.out, "  {ANSI_DIM}⟳ {name}{ANSI_RESET}")?;
                } else {
                    writeln!(self.out, "  [tool] {name}")?;
                }
            }
            SessionEvent::ToolEnd { name, outcome, .. } => {
                self.break_stream()?;
                let (marker_color, marker_plain, marker_symbol) = match outcome {
                    orbs::session::ToolOutcome::Ok { .. } => (ANSI_GREEN, "[done]", "✓"),
                    orbs::session::ToolOutcome::Err { .. } => (ANSI_RED, "[fail]", "✗"),
                };
                if self.use_color {
                    writeln!(
                        self.out,
                        "  {marker_color}{marker_symbol} {name}{ANSI_RESET}"
                    )?;
                } else {
                    writeln!(self.out, "  {marker_plain} {name}")?;
                }
            }
            SessionEvent::Error { message, .. } => {
                self.break_stream()?;
                if self.use_color {
                    writeln!(self.out, "{ANSI_RED}error:{ANSI_RESET} {message}")?;
                } else {
                    writeln!(self.out, "error: {message}")?;
                }
            }
            SessionEvent::Cancelled { .. } => {
                self.break_stream()?;
                if self.use_color {
                    writeln!(self.out, "{ANSI_YELLOW}cancelled{ANSI_RESET}")?;
                } else {
                    writeln!(self.out, "cancelled")?;
                }
            }
            SessionEvent::OrbSpawned { orb_id, .. } => {
                self.break_stream()?;
                writeln!(self.out, "  → spawned {orb_id}")?;
            }
            SessionEvent::OrbResult {
                orb_id, summary, ..
            } => {
                self.break_stream()?;
                writeln!(self.out, "  ← {orb_id}: {summary}")?;
            }
            // Usage events are not rendered inline — status line shows them.
            // StatusChanged events are control-plane and not rendered.
            SessionEvent::Usage { .. } | SessionEvent::StatusChanged { .. } => {}
        }
        self.out.flush()?;
        Ok(())
    }

    /// Renders the per-turn status info as a single dim line. Call once
    /// after a turn completes.
    ///
    /// # Errors
    ///
    /// Returns any `io::Error` produced by the underlying writer.
    pub fn render_status(&mut self, summary: &TurnSummary) -> io::Result<()> {
        self.break_stream()?;
        let usage = &summary.usage;
        let line = format!(
            "[tokens: {p} in / {c} out | tools: {t}]",
            p = usage.prompt_tokens,
            c = usage.completion_tokens,
            t = summary.tool_call_count,
        );
        if self.use_color {
            writeln!(self.out, "{ANSI_DIM}{line}{ANSI_RESET}")?;
        } else {
            writeln!(self.out, "{line}")?;
        }
        self.out.flush()?;
        Ok(())
    }

    /// Writes a one-shot notice line. Used by the CLI for slash command
    /// feedback ("unknown command", "session closed", etc.).
    ///
    /// # Errors
    ///
    /// Returns any `io::Error` produced by the underlying writer.
    pub fn notice(&mut self, text: &str) -> io::Result<()> {
        self.break_stream()?;
        if self.use_color {
            writeln!(self.out, "{ANSI_DIM}{text}{ANSI_RESET}")?;
        } else {
            writeln!(self.out, "{text}")?;
        }
        self.out.flush()?;
        Ok(())
    }

    fn break_stream(&mut self) -> io::Result<()> {
        if self.mid_stream {
            writeln!(self.out)?;
            self.mid_stream = false;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use orbs::session::{SessionUsage, ToolOutcome, TurnId};

    fn buf_renderer(color: bool) -> Renderer<Vec<u8>> {
        Renderer::new(Vec::new(), color)
    }

    fn output(r: Renderer<Vec<u8>>) -> String {
        String::from_utf8(r.out).unwrap()
    }

    #[test]
    fn delta_then_assistant_message_emits_single_line() {
        let mut r = buf_renderer(false);
        let t = TurnId::from_raw("turn-1");
        r.render(&SessionEvent::AssistantDelta {
            turn_id: t.clone(),
            chunk: "hel".into(),
        })
        .unwrap();
        r.render(&SessionEvent::AssistantDelta {
            turn_id: t.clone(),
            chunk: "lo".into(),
        })
        .unwrap();
        r.render(&SessionEvent::AssistantMessage {
            turn_id: t,
            content: "hello".into(),
            at: Utc::now(),
        })
        .unwrap();
        assert_eq!(output(r), "hello\n");
    }

    #[test]
    fn assistant_message_without_prior_stream_prints_content() {
        let mut r = buf_renderer(false);
        r.render(&SessionEvent::AssistantMessage {
            turn_id: TurnId::from_raw("turn-1"),
            content: "direct answer".into(),
            at: Utc::now(),
        })
        .unwrap();
        assert_eq!(output(r), "direct answer\n");
    }

    #[test]
    fn tool_start_during_stream_inserts_newline_break() {
        let mut r = buf_renderer(false);
        let t = TurnId::from_raw("turn-1");
        r.render(&SessionEvent::AssistantDelta {
            turn_id: t.clone(),
            chunk: "thinking...".into(),
        })
        .unwrap();
        r.render(&SessionEvent::ToolStart {
            turn_id: t,
            name: "bash".into(),
            args: serde_json::json!({"cmd": "ls"}),
        })
        .unwrap();
        let s = output(r);
        // Stream content, newline, tool line.
        assert!(s.contains("thinking...\n"));
        assert!(s.contains("[tool] bash"));
    }

    #[test]
    fn tool_end_ok_renders_done_marker() {
        let mut r = buf_renderer(false);
        r.render(&SessionEvent::ToolEnd {
            turn_id: TurnId::from_raw("turn-1"),
            name: "bash".into(),
            outcome: ToolOutcome::Ok {
                summary: "exit 0".into(),
            },
        })
        .unwrap();
        assert!(output(r).contains("[done] bash"));
    }

    #[test]
    fn tool_end_err_renders_fail_marker() {
        let mut r = buf_renderer(false);
        r.render(&SessionEvent::ToolEnd {
            turn_id: TurnId::from_raw("turn-1"),
            name: "bash".into(),
            outcome: ToolOutcome::Err {
                message: "exit 1".into(),
            },
        })
        .unwrap();
        assert!(output(r).contains("[fail] bash"));
    }

    #[test]
    fn error_event_renders_error_prefix() {
        let mut r = buf_renderer(false);
        r.render(&SessionEvent::Error {
            turn_id: Some(TurnId::from_raw("turn-1")),
            message: "rate limited".into(),
            at: Utc::now(),
        })
        .unwrap();
        assert!(output(r).contains("error: rate limited"));
    }

    #[test]
    fn usage_event_is_silent() {
        let mut r = buf_renderer(false);
        r.render(&SessionEvent::Usage {
            turn_id: TurnId::from_raw("turn-1"),
            usage: SessionUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            },
        })
        .unwrap();
        assert_eq!(output(r), "");
    }

    #[test]
    fn status_changed_is_silent() {
        let mut r = buf_renderer(false);
        r.render(&SessionEvent::StatusChanged {
            to: orbs::session::SessionStatus::Closed,
            reason: None,
            at: Utc::now(),
        })
        .unwrap();
        assert_eq!(output(r), "");
    }

    #[test]
    fn render_status_writes_one_line() {
        let mut r = buf_renderer(false);
        r.render_status(&TurnSummary {
            turn_id: TurnId::from_raw("turn-1"),
            response: None,
            usage: SessionUsage {
                prompt_tokens: 123,
                completion_tokens: 45,
                total_tokens: 168,
            },
            tool_call_count: 2,
            status: crate::convo::TurnStatus::Ok,
        })
        .unwrap();
        let s = output(r);
        assert!(s.contains("[tokens: 123 in / 45 out | tools: 2]"));
        assert!(s.ends_with('\n'));
        assert_eq!(s.lines().count(), 1);
    }

    #[test]
    fn color_mode_emits_ansi_escapes() {
        let mut r = buf_renderer(true);
        r.render(&SessionEvent::UserMessage {
            turn_id: TurnId::from_raw("turn-1"),
            content: "hi".into(),
            at: Utc::now(),
        })
        .unwrap();
        let s = output(r);
        assert!(s.contains("\x1b["));
    }

    #[test]
    fn notice_writes_line() {
        let mut r = buf_renderer(false);
        r.notice("unknown command: /foo").unwrap();
        assert_eq!(output(r), "unknown command: /foo\n");
    }
}

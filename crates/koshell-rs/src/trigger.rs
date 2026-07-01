//! Terminal-state processing: records PTY facts into the timeline, keeps the mirror and
//! snapshots up to date, tracks command spans from shell-integration markers, and detects
//! `#?` questions. On a `#?`, assembles the context package the AI daemon will consume.

use std::time::Duration;

use crate::context::{TerminalContextOptions, build_terminal_context};
use crate::mirror::TerminalMirror;
use crate::screen_diff::summarize_screen_diff;
use crate::shell_integration::{MarkerKind, ShellIntegrationMarker};
use crate::timeline::{InMemoryTimelineStore, TerminalEvent};

/// The stable AI context contract version (kept in sync with the daemon).
const AI_CONTEXT_CONTRACT_VERSION: &str = "koshell_ai_context_v1";

/// The `#?` trigger token.
const TRIGGER_TOKEN: &str = "#?";

/// How long the PTY output must stay idle before the S3 (quiescence) path treats a command
/// as complete, for programs that emit no bracketed-paste edges (e.g. node). See
/// `docs/design-0001-repl-command-completion.md`.
const REPL_QUIESCENCE_DEBOUNCE: Duration = Duration::from_millis(150);

/// The bracketed-paste enable/disable sequences share this 7-byte prefix; the final byte is
/// `h` (enable) or `l` (disable).
const BRACKETED_PASTE_PREFIX: &[u8] = b"\x1b[?2004";

/// A detected `#?` question with the terminal context to answer it.
#[derive(Debug, Clone)]
pub struct Trigger {
    pub question: String,
    pub context_package: serde_json::Value,
}

/// A bracketed-paste toggle observed in the PTY output stream. Line editors emit
/// `ESC[?2004h` when they begin reading a line (prompt ready) and `ESC[?2004l` when the line
/// is submitted (command running), so these edges bracket command execution for
/// readline/libedit/PyREPL-style programs — koshell's S1 completion signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BracketedPasteEdge {
    /// `ESC[?2004h` — entering line edit; the previous command completed.
    Enter,
    /// `ESC[?2004l` — line submitted; a command is now running.
    Leave,
}

/// Extracts bracketed-paste edges from a byte stream, buffering a partial sequence across
/// chunk boundaries. Does not consume or hide the bytes; they still flow to the terminal.
#[derive(Default)]
struct BracketedPasteScanner {
    carry: Vec<u8>,
}

impl BracketedPasteScanner {
    fn feed(&mut self, data: &[u8]) -> Vec<BracketedPasteEdge> {
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(data);
        let mut edges = Vec::new();
        let mut i = 0;
        while i < buf.len() {
            if buf[i] != 0x1b {
                i += 1;
                continue;
            }
            // A full sequence is the 7-byte prefix plus one terminator byte.
            if i + BRACKETED_PASTE_PREFIX.len() + 1 > buf.len() {
                break; // Incomplete at the tail; carry it for the next chunk.
            }
            if &buf[i..i + BRACKETED_PASTE_PREFIX.len()] == BRACKETED_PASTE_PREFIX {
                match buf[i + BRACKETED_PASTE_PREFIX.len()] {
                    b'h' => {
                        edges.push(BracketedPasteEdge::Enter);
                        i += BRACKETED_PASTE_PREFIX.len() + 1;
                        continue;
                    }
                    b'l' => {
                        edges.push(BracketedPasteEdge::Leave);
                        i += BRACKETED_PASTE_PREFIX.len() + 1;
                        continue;
                    }
                    _ => {}
                }
            }
            i += 1;
        }
        // Only an incomplete trailing ESC sequence is carried, so `carry` stays <= 7 bytes.
        self.carry = buf[i..].to_vec();
        edges
    }
}

/// Detects a `#?` typed inside a foreground CLI program (a REPL), and when that command
/// completed, so the trigger can be deferred until completion — the in-program analogue of
/// the shell's `command_end`. Prototype scope (S1 + S3 from
/// `docs/design-0001-repl-command-completion.md`):
/// - `#?` capture: the submitted line is reconstructed from keystrokes; robust mirror-read
///   is a follow-up. Only fed while gated inside a child span.
/// - completion: S1 bracketed-paste `ESC[?2004h` edge, or, for programs that emit no such
///   edge (e.g. node), S3 output quiescence.
#[derive(Default)]
struct ReplDetector {
    input_line: Vec<u8>,
    pending_question: Option<String>,
    bp_scanner: BracketedPasteScanner,
    bp_seen: bool,
}

impl ReplDetector {
    /// Accumulates keystrokes; on Enter, records a pending `#?` if the line contains one.
    fn on_input(&mut self, data: &[u8]) {
        for &b in data {
            match b {
                b'\r' | b'\n' => self.finish_line(),
                0x7f | 0x08 => {
                    self.input_line.pop();
                }
                _ => self.input_line.push(b),
            }
        }
    }

    fn finish_line(&mut self) {
        let line = String::from_utf8_lossy(&self.input_line).into_owned();
        self.input_line.clear();
        if let Some(question) = extract_question(&line) {
            self.pending_question = Some(question);
        }
    }

    /// Feeds visible output; returns true if a command just completed (S1) with a pending `#?`.
    fn on_output(&mut self, data: &[u8]) -> bool {
        let mut completed = false;
        for edge in self.bp_scanner.feed(data) {
            self.bp_seen = true;
            if edge == BracketedPasteEdge::Enter && self.pending_question.is_some() {
                completed = true;
            }
        }
        completed
    }

    /// True when the S3 (quiescence) path is eligible: a `#?` is pending and the program has
    /// emitted no bracketed-paste edges, so S1 can never fire for it.
    fn quiescence_armed(&self) -> bool {
        self.pending_question.is_some() && !self.bp_seen
    }

    fn take_pending(&mut self) -> Option<String> {
        self.pending_question.take()
    }

    /// Resets all state at a child-span boundary (`command_start` / `command_end`).
    fn reset(&mut self) {
        self.input_line.clear();
        self.pending_question = None;
        self.bp_scanner = BracketedPasteScanner::default();
        self.bp_seen = false;
    }
}

/// Owns the timeline and mirror and applies terminal events as they happen.
pub struct SessionState {
    timeline: InMemoryTimelineStore,
    mirror: TerminalMirror,
    previous_snapshot: Option<(String, String)>,
    next_snapshot_id: u64,
    next_command_id: u64,
    command_active: bool,
    repl: ReplDetector,
}

impl SessionState {
    pub fn new(columns: u16, rows: u16) -> Self {
        Self {
            timeline: InMemoryTimelineStore::new(),
            mirror: TerminalMirror::new(columns, rows),
            previous_snapshot: None,
            next_snapshot_id: 1,
            next_command_id: 1,
            command_active: false,
            repl: ReplDetector::default(),
        }
    }

    /// Whether the in-program `#?` detector should observe I/O: we are inside a foreground
    /// child (between `command_start` and `command_end`) and not on the alternate screen.
    /// Outside a child span the shell OSC path owns `#?`, so the two never overlap.
    fn repl_gated(&self) -> bool {
        self.command_active && !self.mirror.is_alt_screen()
    }

    /// Records human keystrokes sent to the shell.
    pub fn record_input(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.timeline.record(TerminalEvent::HumanInput {
            data: String::from_utf8_lossy(data).into_owned(),
            visible: true,
        });
        if self.repl_gated() {
            self.repl.on_input(data);
        }
    }

    /// Records visible PTY output, updates the mirror, and captures a snapshot. Returns a
    /// [`Trigger`] when a `#?` typed inside a foreground program completes via the S1
    /// (bracketed-paste) signal.
    pub fn record_output(&mut self, visible: &[u8]) -> Option<Trigger> {
        if visible.is_empty() {
            return None;
        }
        self.timeline.record(TerminalEvent::PtyOutput {
            data: String::from_utf8_lossy(visible).into_owned(),
        });
        self.mirror.write(visible);
        self.record_snapshot();
        if self.repl_gated()
            && self.repl.on_output(visible)
            && let Some(question) = self.repl.take_pending()
        {
            return Some(self.build_repl_trigger(question));
        }
        None
    }

    /// The debounce to wait for output quiescence when the S3 path is armed, or `None` when
    /// it is not (the caller should then block indefinitely).
    pub fn repl_quiescence_debounce(&self) -> Option<Duration> {
        if self.repl_gated() && self.repl.quiescence_armed() {
            Some(REPL_QUIESCENCE_DEBOUNCE)
        } else {
            None
        }
    }

    /// Called when the PTY output has been idle for [`Self::repl_quiescence_debounce`].
    /// Fires a pending in-program `#?` via the S3 (quiescence) signal.
    pub fn on_quiescence(&mut self) -> Option<Trigger> {
        if self.repl_gated()
            && self.repl.quiescence_armed()
            && let Some(question) = self.repl.take_pending()
        {
            return Some(self.build_repl_trigger(question));
        }
        None
    }

    fn build_repl_trigger(&mut self, question: String) -> Trigger {
        let context_package = self.build_context_package(&question);
        let request_id = format!("request-{}", self.next_command_id());
        self.timeline.record(TerminalEvent::AiRequest {
            request_id,
            question: question.clone(),
        });
        Trigger {
            question,
            context_package,
        }
    }

    /// Resizes the mirror and captures a snapshot at the new size.
    pub fn resize(&mut self, columns: u16, rows: u16) {
        self.mirror.resize(columns, rows);
        self.record_snapshot();
    }

    fn record_snapshot(&mut self) {
        let snapshot = self.mirror.snapshot();
        let snapshot_id = format!("snapshot-{}", self.next_snapshot_id);
        self.next_snapshot_id += 1;

        let (previous_snapshot_id, diff) = match &self.previous_snapshot {
            Some((prev_id, prev_screen)) => (
                Some(prev_id.clone()),
                Some(summarize_screen_diff(prev_screen, &snapshot.screen)),
            ),
            None => (None, None),
        };

        self.timeline.record(TerminalEvent::ScreenSnapshot {
            snapshot_id: snapshot_id.clone(),
            rows: snapshot.rows,
            columns: snapshot.columns,
            alt_screen: snapshot.alt_screen,
            screen: Some(snapshot.screen.clone()),
            previous_snapshot_id,
            diff,
        });
        self.previous_snapshot = Some((snapshot_id, snapshot.screen));
    }

    /// Applies a shell-integration marker, recording the command span and returning a
    /// [`Trigger`] when the command line contains `#?`.
    pub fn handle_marker(&mut self, marker: ShellIntegrationMarker) -> Option<Trigger> {
        match marker.kind {
            MarkerKind::CommandStart => {
                // Entering a foreground child: start a fresh in-program detection span.
                self.repl.reset();
                if let Some(command) = marker.command {
                    let command_id = self.next_command_id();
                    self.command_active = true;
                    self.timeline.record(TerminalEvent::CommandStart {
                        command_id,
                        command,
                        cwd: None,
                    });
                }
                None
            }
            MarkerKind::CommandEnd => {
                // The child exited; drop any pending in-program `#?` and leave the span.
                self.repl.reset();
                let command = marker.command.unwrap_or_default();
                let command_id = self.next_command_id();
                self.command_active = false;
                self.timeline.record(TerminalEvent::CommandEnd {
                    command_id,
                    command: command.clone(),
                    exit_code: marker.exit_code,
                    duration_ms: None,
                });
                extract_question(&command).map(|question| {
                    let context_package = self.build_context_package(&question);
                    let request_id = format!("request-{}", self.next_command_id());
                    self.timeline.record(TerminalEvent::AiRequest {
                        request_id,
                        question: question.clone(),
                    });
                    Trigger {
                        question,
                        context_package,
                    }
                })
            }
        }
    }

    fn next_command_id(&mut self) -> String {
        let id = format!("command-{}", self.next_command_id);
        self.next_command_id += 1;
        id
    }

    fn build_context_package(&self, question: &str) -> serde_json::Value {
        let context = build_terminal_context(&self.timeline, &TerminalContextOptions::default());
        serde_json::json!({
            "contractVersion": AI_CONTEXT_CONTRACT_VERSION,
            "question": question,
            "dynamicContext": serde_json::to_value(&context)
                .unwrap_or(serde_json::Value::Null),
        })
    }
}

/// Extracts a `#?` question from a command line, returning the trimmed remainder.
fn extract_question(command: &str) -> Option<String> {
    command
        .find(TRIGGER_TOKEN)
        .map(|index| command[index + TRIGGER_TOKEN.len()..].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_inline_question() {
        assert_eq!(
            extract_question("ls -la #? explain this output").as_deref(),
            Some("explain this output")
        );
        assert_eq!(
            extract_question("#? what happened").as_deref(),
            Some("what happened")
        );
        assert_eq!(extract_question("ls -la").as_deref(), None);
        assert_eq!(extract_question("echo #?").as_deref(), Some(""));
    }

    #[test]
    fn command_end_with_trigger_produces_context_package() {
        let mut state = SessionState::new(80, 24);
        state.record_output(b"file-a\r\nfile-b\r\n");

        let trigger = state
            .handle_marker(ShellIntegrationMarker {
                kind: MarkerKind::CommandEnd,
                command: Some("ls #? explain".to_string()),
                exit_code: Some(0),
            })
            .expect("expected a trigger");

        assert_eq!(trigger.question, "explain");
        assert_eq!(
            trigger.context_package["contractVersion"],
            AI_CONTEXT_CONTRACT_VERSION
        );
        assert_eq!(trigger.context_package["question"], "explain");
        assert!(trigger.context_package["dynamicContext"].is_object());
    }

    #[test]
    fn command_end_without_trigger_returns_none() {
        let mut state = SessionState::new(80, 24);
        let trigger = state.handle_marker(ShellIntegrationMarker {
            kind: MarkerKind::CommandEnd,
            command: Some("ls -la".to_string()),
            exit_code: Some(0),
        });
        assert!(trigger.is_none());
    }
}

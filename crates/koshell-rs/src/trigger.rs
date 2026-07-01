//! Terminal-state processing: records PTY facts into the timeline, keeps the mirror and
//! snapshots up to date, tracks command spans from shell-integration markers, and detects
//! `#?` questions. On a `#?`, assembles the context package the AI daemon will consume.

use crate::context::{TerminalContextOptions, build_terminal_context};
use crate::mirror::TerminalMirror;
use crate::screen_diff::summarize_screen_diff;
use crate::shell_integration::{MarkerKind, ShellIntegrationMarker};
use crate::timeline::{InMemoryTimelineStore, TerminalEvent};

/// The stable AI context contract version (kept in sync with the daemon).
const AI_CONTEXT_CONTRACT_VERSION: &str = "koshell_ai_context_v1";

/// The `#?` trigger token.
const TRIGGER_TOKEN: &str = "#?";

/// A detected `#?` question with the terminal context to answer it.
#[derive(Debug, Clone)]
pub struct Trigger {
    pub question: String,
    pub context_package: serde_json::Value,
}

/// Owns the timeline and mirror and applies terminal events as they happen.
pub struct SessionState {
    timeline: InMemoryTimelineStore,
    mirror: TerminalMirror,
    previous_snapshot: Option<(String, String)>,
    next_snapshot_id: u64,
    next_command_id: u64,
    command_active: bool,
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
        }
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
    }

    /// Records visible PTY output, updates the mirror, and captures a snapshot.
    pub fn record_output(&mut self, visible: &[u8]) {
        if visible.is_empty() {
            return;
        }
        self.timeline.record(TerminalEvent::PtyOutput {
            data: String::from_utf8_lossy(visible).into_owned(),
        });
        self.mirror.write(visible);
        self.record_snapshot();
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

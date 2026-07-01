//! Append-only terminal timeline and in-memory store, ported from
//! `reference/src/timeline.ts`. Pure data plus queries; no I/O.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::screen_diff::{ScreenDiffSummary, ScreenTextDiff, diff_screen_text};

/// Terminal control mode. Reserved for the policy/control-mode work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlMode {
    Human,
    Shared,
    Ai,
}

/// An append-only terminal fact. Timestamps live on [`TimelineEntry`], not the event.
#[derive(Debug, Clone, PartialEq)]
pub enum TerminalEvent {
    HumanInput {
        data: String,
        /// `false` marks hidden input (e.g. passwords), excluded from text context.
        visible: bool,
    },
    PtyOutput {
        data: String,
    },
    VisibleOutput {
        text: String,
    },
    CommandStart {
        command_id: String,
        command: String,
        cwd: Option<String>,
    },
    CommandEnd {
        command_id: String,
        command: String,
        exit_code: Option<i32>,
        duration_ms: Option<u64>,
    },
    ScreenSnapshot {
        snapshot_id: String,
        rows: u16,
        columns: u16,
        alt_screen: bool,
        screen: Option<String>,
        previous_snapshot_id: Option<String>,
        diff: Option<ScreenDiffSummary>,
    },
    AiRequest {
        request_id: String,
        question: String,
    },
    AiResponse {
        request_id: String,
        text: String,
    },
    ControlModeChange {
        from: ControlMode,
        to: ControlMode,
        reason: String,
    },
}

/// A recorded event with its assigned id and timestamp.
#[derive(Debug, Clone, PartialEq)]
pub struct TimelineEntry {
    pub id: String,
    pub ts: i64,
    pub event: TerminalEvent,
}

/// On-demand detailed diff between two screen snapshots.
#[derive(Debug, Clone, PartialEq)]
pub struct ScreenSnapshotDiff {
    pub from_snapshot_id: String,
    pub to_snapshot_id: String,
    pub diff: ScreenTextDiff,
}

type Clock = Box<dyn Fn() -> i64 + Send + Sync>;

/// In-memory, append-only timeline store with injectable clock.
pub struct InMemoryTimelineStore {
    entries: Vec<TimelineEntry>,
    now: Clock,
    next_id: u64,
}

impl Default for InMemoryTimelineStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryTimelineStore {
    /// Creates a store using the system wall clock (milliseconds since epoch).
    pub fn new() -> Self {
        Self::with_clock(default_now)
    }

    /// Creates a store with an injectable clock (used by tests).
    pub fn with_clock<F: Fn() -> i64 + Send + Sync + 'static>(now: F) -> Self {
        Self {
            entries: Vec::new(),
            now: Box::new(now),
            next_id: 1,
        }
    }

    /// Records an event, assigning it a deterministic id and the current timestamp.
    /// Returns the created entry.
    pub fn record(&mut self, event: TerminalEvent) -> TimelineEntry {
        let id = format!("event-{}", self.next_id);
        self.next_id += 1;
        let entry = TimelineEntry {
            id,
            ts: (self.now)(),
            event,
        };
        self.entries.push(entry.clone());
        entry
    }

    pub fn list_entries(&self) -> &[TimelineEntry] {
        &self.entries
    }

    pub fn list_events(&self) -> impl Iterator<Item = &TerminalEvent> {
        self.entries.iter().map(|entry| &entry.event)
    }

    /// Recent human-visible terminal text across event kinds, trimmed to `max` chars.
    pub fn get_recent_text(&self, max: usize) -> String {
        let text: String = self
            .entries
            .iter()
            .map(|e| event_to_text(&e.event))
            .collect();
        trim_start_to_max_characters(&text, max)
    }

    /// Recent raw PTY output, trimmed to `max` chars.
    pub fn get_recent_pty_output(&self, max: usize) -> String {
        let text: String = self
            .entries
            .iter()
            .filter_map(|e| match &e.event {
                TerminalEvent::PtyOutput { data } => Some(data.as_str()),
                _ => None,
            })
            .collect();
        trim_start_to_max_characters(&text, max)
    }

    pub fn list_screen_snapshots(&self) -> Vec<&TimelineEntry> {
        self.entries
            .iter()
            .filter(|e| matches!(e.event, TerminalEvent::ScreenSnapshot { .. }))
            .collect()
    }

    pub fn get_screen_snapshot(&self, snapshot_id: &str) -> Option<&TimelineEntry> {
        self.entries.iter().find(|e| match &e.event {
            TerminalEvent::ScreenSnapshot {
                snapshot_id: id, ..
            } => id == snapshot_id,
            _ => false,
        })
    }

    pub fn get_latest_screen_snapshot(&self) -> Option<&TimelineEntry> {
        self.find_latest_screen_snapshot(|_| true)
    }

    pub fn get_latest_alternate_screen_snapshot(&self) -> Option<&TimelineEntry> {
        self.find_latest_screen_snapshot(|alt_screen| alt_screen)
    }

    fn find_latest_screen_snapshot(
        &self,
        predicate: impl Fn(bool) -> bool,
    ) -> Option<&TimelineEntry> {
        self.entries.iter().rev().find(|e| match &e.event {
            TerminalEvent::ScreenSnapshot { alt_screen, .. } => predicate(*alt_screen),
            _ => false,
        })
    }

    /// Diffs two recorded snapshots by id. Errors if either id is missing.
    pub fn diff_screen_snapshots(
        &self,
        from_snapshot_id: &str,
        to_snapshot_id: &str,
    ) -> anyhow::Result<ScreenSnapshotDiff> {
        let from = self.required_snapshot_screen(from_snapshot_id)?;
        let to = self.required_snapshot_screen(to_snapshot_id)?;
        Ok(ScreenSnapshotDiff {
            from_snapshot_id: from_snapshot_id.to_string(),
            to_snapshot_id: to_snapshot_id.to_string(),
            diff: diff_screen_text(&from, &to),
        })
    }

    fn required_snapshot_screen(&self, snapshot_id: &str) -> anyhow::Result<String> {
        match self.get_screen_snapshot(snapshot_id) {
            Some(TimelineEntry {
                event: TerminalEvent::ScreenSnapshot { screen, .. },
                ..
            }) => Ok(screen.clone().unwrap_or_default()),
            _ => anyhow::bail!("Screen snapshot {snapshot_id:?} was not found."),
        }
    }

    pub fn reset(&mut self) {
        self.entries.clear();
    }
}

fn default_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Maps an event to its contribution to recent human-visible text.
fn event_to_text(event: &TerminalEvent) -> &str {
    match event {
        TerminalEvent::HumanInput { data, visible } => {
            if *visible {
                data
            } else {
                ""
            }
        }
        TerminalEvent::PtyOutput { data } => data,
        TerminalEvent::VisibleOutput { text } => text,
        TerminalEvent::ScreenSnapshot { screen, .. } => screen.as_deref().unwrap_or(""),
        TerminalEvent::AiRequest { question, .. } => question,
        TerminalEvent::AiResponse { text, .. } => text,
        TerminalEvent::CommandStart { .. }
        | TerminalEvent::CommandEnd { .. }
        | TerminalEvent::ControlModeChange { .. } => "",
    }
}

/// Keeps only the last `max` characters of `text`.
pub(crate) fn trim_start_to_max_characters(text: &str, max: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max {
        text.to_string()
    } else {
        text.chars().skip(char_count - max).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(snapshot_id: &str, rows: u16, alt_screen: bool, screen: &str) -> TerminalEvent {
        TerminalEvent::ScreenSnapshot {
            snapshot_id: snapshot_id.to_string(),
            rows,
            columns: 80,
            alt_screen,
            screen: Some(screen.to_string()),
            previous_snapshot_id: None,
            diff: None,
        }
    }

    #[test]
    fn records_events_with_deterministic_ids_and_timestamps() {
        let mut timeline = InMemoryTimelineStore::with_clock(|| 1_000);
        let first = timeline.record(TerminalEvent::HumanInput {
            data: "ls\r".to_string(),
            visible: true,
        });
        assert_eq!(first.id, "event-1");
        assert_eq!(first.ts, 1_000);
        let second = timeline.record(TerminalEvent::PtyOutput {
            data: "file.txt\r\n".to_string(),
        });
        assert_eq!(second.id, "event-2");
        assert_eq!(timeline.list_entries().len(), 2);
    }

    #[test]
    fn returns_recent_text_and_pty_output_with_limits() {
        let mut timeline = InMemoryTimelineStore::with_clock(|| 1);
        timeline.record(TerminalEvent::HumanInput {
            data: "echo hello\r".to_string(),
            visible: true,
        });
        timeline.record(TerminalEvent::PtyOutput {
            data: "hello\r\n".to_string(),
        });
        timeline.record(TerminalEvent::VisibleOutput {
            text: "visible".to_string(),
        });
        timeline.record(TerminalEvent::HumanInput {
            data: "secret".to_string(),
            visible: false,
        });

        assert_eq!(
            timeline.get_recent_text(8_000),
            "echo hello\rhello\r\nvisible"
        );
        assert_eq!(timeline.get_recent_text(5), "sible");
        assert_eq!(timeline.get_recent_pty_output(8_000), "hello\r\n");
    }

    #[test]
    fn tracks_snapshots_latest_alternate_and_reset() {
        let mut timeline = InMemoryTimelineStore::with_clock(|| 1);
        timeline.record(snapshot("snapshot-1", 24, false, "first"));
        timeline.record(snapshot("snapshot-2", 30, true, "second"));

        let ids: Vec<&str> = timeline
            .list_screen_snapshots()
            .iter()
            .map(|e| match &e.event {
                TerminalEvent::ScreenSnapshot { snapshot_id, .. } => snapshot_id.as_str(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(ids, vec!["snapshot-1", "snapshot-2"]);

        assert!(timeline.get_screen_snapshot("snapshot-1").is_some());
        let latest = timeline.get_latest_screen_snapshot().unwrap();
        assert!(
            matches!(&latest.event, TerminalEvent::ScreenSnapshot { snapshot_id, .. } if snapshot_id == "snapshot-2")
        );
        let alt = timeline.get_latest_alternate_screen_snapshot().unwrap();
        assert!(
            matches!(&alt.event, TerminalEvent::ScreenSnapshot { snapshot_id, .. } if snapshot_id == "snapshot-2")
        );

        timeline.reset();
        assert!(timeline.list_entries().is_empty());
        assert!(timeline.get_latest_screen_snapshot().is_none());
    }

    #[test]
    fn diffs_screen_snapshots_on_demand() {
        let mut timeline = InMemoryTimelineStore::with_clock(|| 1);
        timeline.record(snapshot("snapshot-1", 24, false, "one\ntwo"));
        timeline.record(snapshot("snapshot-2", 24, false, "one\nTWO\nthree"));

        let diff = timeline
            .diff_screen_snapshots("snapshot-1", "snapshot-2")
            .unwrap();
        assert_eq!(diff.from_snapshot_id, "snapshot-1");
        assert_eq!(diff.to_snapshot_id, "snapshot-2");
        assert_eq!(diff.diff.added_lines, 2);
        assert_eq!(diff.diff.removed_lines, 1);
        assert_eq!(diff.diff.changed_lines, 3);

        let err = timeline
            .diff_screen_snapshots("missing", "snapshot-2")
            .unwrap_err();
        assert!(err.to_string().contains("\"missing\" was not found"));
    }
}

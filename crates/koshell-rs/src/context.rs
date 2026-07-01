//! Terminal context selection, ported from `reference/src/terminal-context.ts`.
//!
//! Collapses the timeline into a bounded, prioritized view of "what is on screen now",
//! choosing a single primary text field per the prototype's heuristics.

use serde::{Deserialize, Serialize};

use crate::timeline::{
    InMemoryTimelineStore, TerminalEvent, TimelineEntry, trim_start_to_max_characters,
};

const DEFAULT_RECENT_INPUT_MAX_CHARACTERS: usize = 2_000;
const DEFAULT_RECENT_PTY_OUTPUT_MAX_CHARACTERS: usize = 8_000;
const DEFAULT_RECENT_VISIBLE_OUTPUT_MAX_CHARACTERS: usize = 8_000;
const DEFAULT_CURRENT_SCREEN_MAX_CHARACTERS: usize = 8_000;
const DEFAULT_RECENT_SCREEN_CHANGES_LIMIT: usize = 20;

/// Which source provided the primary context text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrimarySource {
    ScreenSnapshot,
    VisibleOutput,
    PtyOutput,
    Empty,
}

/// A summarized screen change derived from a snapshot's diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalScreenChange {
    pub snapshot_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_snapshot_id: Option<String>,
    pub alt_screen: bool,
    pub rows: u16,
    pub added_lines: usize,
    pub removed_lines: usize,
    pub changed_lines: usize,
    pub large_change: bool,
    pub summary: String,
}

/// A bounded, prioritized snapshot of terminal context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalContext {
    pub recent_input: String,
    pub recent_pty_output: String,
    pub recent_visible_output: String,
    pub recent_screen_changes: Vec<TerminalScreenChange>,
    pub alt_screen: bool,
    pub primary_text: String,
    pub primary_source: PrimarySource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_screen: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_rows: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_columns: Option<u16>,
}

/// Character/count budgets for context assembly. `None` uses the default.
#[derive(Debug, Clone, Default)]
pub struct TerminalContextOptions {
    pub recent_input_max_characters: Option<usize>,
    pub recent_pty_output_max_characters: Option<usize>,
    pub recent_visible_output_max_characters: Option<usize>,
    pub current_screen_max_characters: Option<usize>,
    pub recent_screen_changes_limit: Option<usize>,
}

/// Fields extracted from the latest screen snapshot event.
struct LatestSnapshot {
    rows: u16,
    columns: u16,
    alt_screen: bool,
    screen: Option<String>,
}

fn latest_snapshot(timeline: &InMemoryTimelineStore) -> Option<LatestSnapshot> {
    match timeline.get_latest_screen_snapshot()?.event {
        TerminalEvent::ScreenSnapshot {
            rows,
            columns,
            alt_screen,
            ref screen,
            ..
        } => Some(LatestSnapshot {
            rows,
            columns,
            alt_screen,
            screen: screen.clone(),
        }),
        _ => None,
    }
}

/// Builds the terminal context view from the timeline.
pub fn build_terminal_context(
    timeline: &InMemoryTimelineStore,
    options: &TerminalContextOptions,
) -> TerminalContext {
    let latest = latest_snapshot(timeline);

    let recent_input = get_recent_input(
        timeline,
        options
            .recent_input_max_characters
            .unwrap_or(DEFAULT_RECENT_INPUT_MAX_CHARACTERS),
    );
    let recent_visible_output = get_recent_visible_output(
        timeline,
        options
            .recent_visible_output_max_characters
            .unwrap_or(DEFAULT_RECENT_VISIBLE_OUTPUT_MAX_CHARACTERS),
    );
    let recent_pty_output = timeline.get_recent_pty_output(
        options
            .recent_pty_output_max_characters
            .unwrap_or(DEFAULT_RECENT_PTY_OUTPUT_MAX_CHARACTERS),
    );

    let current_screen = latest.as_ref().and_then(|snapshot| {
        snapshot
            .screen
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| {
                trim_start_to_max_characters(
                    s,
                    options
                        .current_screen_max_characters
                        .unwrap_or(DEFAULT_CURRENT_SCREEN_MAX_CHARACTERS),
                )
            })
    });

    let alt_screen = latest.as_ref().is_some_and(|s| s.alt_screen);
    let recent_screen_changes = get_recent_screen_changes(
        &timeline.list_screen_snapshots(),
        options
            .recent_screen_changes_limit
            .unwrap_or(DEFAULT_RECENT_SCREEN_CHANGES_LIMIT),
    );

    let (primary_text, primary_source) = choose_primary_text(
        alt_screen,
        current_screen.as_deref(),
        &recent_visible_output,
        &recent_pty_output,
    );

    TerminalContext {
        recent_input,
        recent_pty_output,
        recent_visible_output,
        recent_screen_changes,
        alt_screen,
        primary_text,
        primary_source,
        current_screen,
        screen_rows: latest.as_ref().map(|s| s.rows),
        screen_columns: latest.as_ref().map(|s| s.columns),
    }
}

fn get_recent_input(timeline: &InMemoryTimelineStore, max: usize) -> String {
    let text: String = timeline
        .list_events()
        .filter_map(|event| match event {
            TerminalEvent::HumanInput { data, visible } if *visible => Some(data.as_str()),
            _ => None,
        })
        .collect();
    trim_start_to_max_characters(&text, max)
}

fn get_recent_visible_output(timeline: &InMemoryTimelineStore, max: usize) -> String {
    let text: String = timeline
        .list_events()
        .filter_map(|event| match event {
            TerminalEvent::VisibleOutput { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    trim_start_to_max_characters(&text, max)
}

fn get_recent_screen_changes(
    snapshots: &[&TimelineEntry],
    limit: usize,
) -> Vec<TerminalScreenChange> {
    if limit == 0 {
        return Vec::new();
    }

    let changed: Vec<&&TimelineEntry> = snapshots
        .iter()
        .filter(|entry| {
            matches!(
                &entry.event,
                TerminalEvent::ScreenSnapshot { diff: Some(_), .. }
            )
        })
        .collect();

    let start = changed.len().saturating_sub(limit);
    changed[start..]
        .iter()
        .filter_map(|entry| match &entry.event {
            TerminalEvent::ScreenSnapshot {
                snapshot_id,
                rows,
                alt_screen,
                previous_snapshot_id,
                diff: Some(diff),
                ..
            } => Some(TerminalScreenChange {
                snapshot_id: snapshot_id.clone(),
                previous_snapshot_id: previous_snapshot_id.clone(),
                alt_screen: *alt_screen,
                rows: *rows,
                added_lines: diff.added_lines,
                removed_lines: diff.removed_lines,
                changed_lines: diff.changed_lines,
                large_change: diff.changed_lines >= (*rows as usize).div_ceil(2),
                summary: format!("+{}, -{}", diff.added_lines, diff.removed_lines),
            }),
            _ => None,
        })
        .collect()
}

fn choose_primary_text(
    alt_screen: bool,
    current_screen: Option<&str>,
    recent_visible_output: &str,
    recent_pty_output: &str,
) -> (String, PrimarySource) {
    if alt_screen && has_text(current_screen) {
        return (
            current_screen.unwrap_or_default().to_string(),
            PrimarySource::ScreenSnapshot,
        );
    }
    if has_text(Some(recent_visible_output)) {
        return (
            recent_visible_output.to_string(),
            PrimarySource::VisibleOutput,
        );
    }
    if !alt_screen && has_text(Some(recent_pty_output)) {
        return (recent_pty_output.to_string(), PrimarySource::PtyOutput);
    }
    if has_text(current_screen) {
        return (
            current_screen.unwrap_or_default().to_string(),
            PrimarySource::ScreenSnapshot,
        );
    }
    (String::new(), PrimarySource::Empty)
}

fn has_text(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen_diff::ScreenDiffSummary;

    fn store() -> InMemoryTimelineStore {
        InMemoryTimelineStore::with_clock(|| 1)
    }

    fn input(data: &str) -> TerminalEvent {
        TerminalEvent::HumanInput {
            data: data.to_string(),
            visible: true,
        }
    }

    #[test]
    fn uses_recent_pty_output_as_primary_in_normal_shell() {
        let mut timeline = store();
        timeline.record(input("echo hello\r"));
        timeline.record(TerminalEvent::PtyOutput {
            data: "hello\r\n".to_string(),
        });
        timeline.record(TerminalEvent::ScreenSnapshot {
            snapshot_id: "snapshot-1".to_string(),
            rows: 24,
            columns: 80,
            alt_screen: false,
            screen: Some("hello".to_string()),
            previous_snapshot_id: None,
            diff: None,
        });

        let ctx = build_terminal_context(&timeline, &TerminalContextOptions::default());
        assert_eq!(ctx.recent_input, "echo hello\r");
        assert_eq!(ctx.recent_pty_output, "hello\r\n");
        assert_eq!(ctx.recent_visible_output, "");
        assert!(ctx.recent_screen_changes.is_empty());
        assert_eq!(ctx.current_screen.as_deref(), Some("hello"));
        assert!(!ctx.alt_screen);
        assert_eq!(ctx.primary_text, "hello\r\n");
        assert_eq!(ctx.primary_source, PrimarySource::PtyOutput);
        assert_eq!(ctx.screen_rows, Some(24));
        assert_eq!(ctx.screen_columns, Some(80));
    }

    #[test]
    fn prefers_visible_output_over_raw_pty() {
        let mut timeline = store();
        timeline.record(TerminalEvent::PtyOutput {
            data: "\u{1b}[31mred\u{1b}[0m".to_string(),
        });
        timeline.record(TerminalEvent::VisibleOutput {
            text: "red".to_string(),
        });

        let ctx = build_terminal_context(&timeline, &TerminalContextOptions::default());
        assert_eq!(ctx.primary_text, "red");
        assert_eq!(ctx.primary_source, PrimarySource::VisibleOutput);
    }

    #[test]
    fn prefers_current_screen_in_alternate_screen_mode() {
        let mut timeline = store();
        timeline.record(input("vim file.txt\r"));
        timeline.record(TerminalEvent::PtyOutput {
            data: "\u{1b}[?1049h\u{1b}[2Jfile contents".to_string(),
        });
        timeline.record(TerminalEvent::ScreenSnapshot {
            snapshot_id: "snapshot-1".to_string(),
            rows: 24,
            columns: 80,
            alt_screen: true,
            screen: Some("file contents".to_string()),
            previous_snapshot_id: None,
            diff: None,
        });

        let ctx = build_terminal_context(&timeline, &TerminalContextOptions::default());
        assert!(ctx.alt_screen);
        assert_eq!(ctx.current_screen.as_deref(), Some("file contents"));
        assert_eq!(ctx.primary_text, "file contents");
        assert_eq!(ctx.primary_source, PrimarySource::ScreenSnapshot);
    }

    #[test]
    fn exposes_recent_screen_change_summaries() {
        let mut timeline = store();
        timeline.record(TerminalEvent::ScreenSnapshot {
            snapshot_id: "snapshot-1".to_string(),
            rows: 4,
            columns: 80,
            alt_screen: false,
            screen: Some("one".to_string()),
            previous_snapshot_id: None,
            diff: None,
        });
        timeline.record(TerminalEvent::ScreenSnapshot {
            snapshot_id: "snapshot-2".to_string(),
            rows: 4,
            columns: 80,
            alt_screen: true,
            screen: Some("one\ntwo\nthree".to_string()),
            previous_snapshot_id: Some("snapshot-1".to_string()),
            diff: Some(ScreenDiffSummary {
                added_lines: 2,
                removed_lines: 0,
                changed_lines: 2,
            }),
        });

        let ctx = build_terminal_context(&timeline, &TerminalContextOptions::default());
        assert_eq!(
            ctx.recent_screen_changes,
            vec![TerminalScreenChange {
                snapshot_id: "snapshot-2".to_string(),
                previous_snapshot_id: Some("snapshot-1".to_string()),
                alt_screen: true,
                rows: 4,
                added_lines: 2,
                removed_lines: 0,
                changed_lines: 2,
                large_change: true,
                summary: "+2, -0".to_string(),
            }]
        );
    }

    #[test]
    fn hides_hidden_human_input() {
        let mut timeline = store();
        timeline.record(input("visible\r"));
        timeline.record(TerminalEvent::HumanInput {
            data: "secret".to_string(),
            visible: false,
        });

        let ctx = build_terminal_context(&timeline, &TerminalContextOptions::default());
        assert_eq!(ctx.recent_input, "visible\r");
    }

    #[test]
    fn honors_character_limits() {
        let mut timeline = store();
        timeline.record(input("abcdef"));
        timeline.record(TerminalEvent::PtyOutput {
            data: "123456".to_string(),
        });
        timeline.record(TerminalEvent::VisibleOutput {
            text: "uvwxyz".to_string(),
        });
        timeline.record(TerminalEvent::ScreenSnapshot {
            snapshot_id: "snapshot-1".to_string(),
            rows: 24,
            columns: 80,
            alt_screen: true,
            screen: Some("screen-text".to_string()),
            previous_snapshot_id: None,
            diff: None,
        });

        let ctx = build_terminal_context(
            &timeline,
            &TerminalContextOptions {
                recent_input_max_characters: Some(3),
                recent_pty_output_max_characters: Some(2),
                recent_visible_output_max_characters: Some(4),
                current_screen_max_characters: Some(4),
                recent_screen_changes_limit: None,
            },
        );
        assert_eq!(ctx.recent_input, "def");
        assert_eq!(ctx.recent_pty_output, "56");
        assert_eq!(ctx.recent_visible_output, "wxyz");
        assert_eq!(ctx.current_screen.as_deref(), Some("text"));
        assert_eq!(ctx.primary_text, "text");
    }
}

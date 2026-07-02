//! Headless terminal mirror backed by `alacritty_terminal`, replacing the prototype's
//! `@xterm/headless` mirror (`reference/src/terminal-mirror.ts`).
//!
//! Snapshot semantics match the prototype: only the visible viewport is captured, each
//! line is right-trimmed, and the whole screen is trailing-trimmed. `alt_screen` reflects
//! the terminal's alternate-screen mode.

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::Processor;

/// Matches the prototype's xterm scrollback of 2000 lines.
const SCROLLBACK_LINES: usize = 2_000;

/// A plain-text snapshot of the visible terminal screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalSnapshot {
    pub rows: u16,
    pub columns: u16,
    pub cursor_x: u16,
    pub cursor_y: u16,
    pub alt_screen: bool,
    pub screen: String,
}

/// Terminal dimensions passed to `alacritty_terminal`. No scrollback in the viewport
/// calculation; history comes from [`Config::scrolling_history`].
struct MirrorSize {
    columns: usize,
    screen_lines: usize,
}

impl Dimensions for MirrorSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// A headless terminal that consumes raw PTY bytes and exposes screen snapshots.
pub struct TerminalMirror {
    term: Term<VoidListener>,
    parser: Processor,
    columns: u16,
    rows: u16,
}

impl TerminalMirror {
    /// Creates a mirror sized `columns` x `rows`.
    pub fn new(columns: u16, rows: u16) -> Self {
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            ..Config::default()
        };
        let size = MirrorSize {
            columns: columns as usize,
            screen_lines: rows as usize,
        };
        let term = Term::new(config, &size, VoidListener);
        Self {
            term,
            parser: Processor::default(),
            columns,
            rows,
        }
    }

    /// Feeds raw PTY bytes into the terminal emulator.
    pub fn write(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.parser.advance(&mut self.term, data);
    }

    /// Resizes the mirrored terminal.
    pub fn resize(&mut self, columns: u16, rows: u16) {
        self.term.resize(MirrorSize {
            columns: columns as usize,
            screen_lines: rows as usize,
        });
        self.columns = columns;
        self.rows = rows;
    }

    /// Whether the terminal is currently on the alternate screen (a full-screen TUI).
    /// Cheap enough to call per keystroke, unlike [`Self::snapshot`].
    pub fn is_alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// The right-trimmed text of the cursor's row plus the cursor column. Used by the
    /// prompt-shape debounce modulator, which needs "what line is the cursor resting on".
    pub fn cursor_row(&self) -> (String, u16) {
        let cursor = self.term.grid().cursor.point;
        let text = self.row_text(cursor.line.0.max(0));
        (text.trim_end().to_string(), cursor.column.0 as u16)
    }

    /// The cursor's logical line: its row joined with any soft-wrapped neighbours (a row
    /// whose last cell carries `WRAPLINE` continues into the row below). This is the
    /// submit-time `#?` capture read — it reflects the final rendered input line, so it is
    /// robust to line editing, and input that is never echoed never appears here.
    pub fn cursor_logical_line(&self) -> String {
        let cursor_row = self.term.grid().cursor.point.line.0.max(0);
        let mut start = cursor_row;
        while start > 0 && self.row_wraps(start - 1) {
            start -= 1;
        }
        let mut end = cursor_row;
        let last_row = self.rows as i32 - 1;
        while end < last_row && self.row_wraps(end) {
            end += 1;
        }

        let mut text = String::new();
        for row in start..=end {
            let row_text = self.row_text(row);
            if row < end {
                // A wrapped row is full width by construction; keep it untrimmed.
                text.push_str(&row_text);
            } else {
                text.push_str(row_text.trim_end());
            }
        }
        text
    }

    fn row_text(&self, row: i32) -> String {
        let grid = self.term.grid();
        let line = &grid[Line(row)];
        let mut text = String::with_capacity(self.columns as usize);
        for col in 0..self.columns as usize {
            let cell = &line[Column(col)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            text.push(cell.c);
        }
        text
    }

    fn row_wraps(&self, row: i32) -> bool {
        let last_col = (self.columns as usize).saturating_sub(1);
        self.term.grid()[Line(row)][Column(last_col)]
            .flags
            .contains(Flags::WRAPLINE)
    }

    /// Captures the current visible screen as a plain-text snapshot.
    pub fn snapshot(&self) -> TerminalSnapshot {
        let grid = self.term.grid();
        let mut lines: Vec<String> = Vec::with_capacity(self.rows as usize);

        for row in 0..self.rows as i32 {
            let line = &grid[Line(row)];
            let mut text = String::with_capacity(self.columns as usize);
            for col in 0..self.columns as usize {
                let cell = &line[Column(col)];
                // A wide char occupies two cells; skip the trailing spacer to avoid
                // duplicating a blank after it.
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                text.push(cell.c);
            }
            lines.push(text.trim_end().to_string());
        }

        let cursor = grid.cursor.point;
        TerminalSnapshot {
            rows: self.rows,
            columns: self.columns,
            cursor_x: cursor.column.0 as u16,
            cursor_y: cursor.line.0.max(0) as u16,
            alt_screen: self.term.mode().contains(TermMode::ALT_SCREEN),
            screen: lines.join("\n").trim_end().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirrors_plain_text() {
        let mut mirror = TerminalMirror::new(80, 24);
        mirror.write(b"hello");
        let snapshot = mirror.snapshot();
        assert_eq!(snapshot.screen, "hello");
        assert!(!snapshot.alt_screen);
        assert_eq!(snapshot.rows, 24);
        assert_eq!(snapshot.columns, 80);
    }

    #[test]
    fn mirrors_multiple_lines() {
        let mut mirror = TerminalMirror::new(80, 24);
        mirror.write(b"first\r\nsecond");
        assert_eq!(mirror.snapshot().screen, "first\nsecond");
    }

    #[test]
    fn detects_alternate_screen_toggle() {
        let mut mirror = TerminalMirror::new(20, 5);
        mirror.write(b"normal");
        assert!(!mirror.snapshot().alt_screen);
        assert_eq!(mirror.snapshot().screen, "normal");

        // Enter alternate screen, clear, home the cursor, draw, then leave; the primary
        // buffer content is restored on exit.
        mirror.write(b"\x1b[?1049h\x1b[2J\x1b[Halternate");
        let alt = mirror.snapshot();
        assert!(alt.alt_screen);
        assert_eq!(alt.screen, "alternate");

        mirror.write(b"\x1b[?1049l");
        let restored = mirror.snapshot();
        assert!(!restored.alt_screen);
        assert_eq!(restored.screen, "normal");
    }

    #[test]
    fn resizes_mirror() {
        let mut mirror = TerminalMirror::new(20, 5);
        mirror.resize(100, 30);
        let snapshot = mirror.snapshot();
        assert_eq!(snapshot.columns, 100);
        assert_eq!(snapshot.rows, 30);
    }

    #[test]
    fn empty_write_is_noop() {
        let mut mirror = TerminalMirror::new(80, 24);
        mirror.write(b"");
        assert_eq!(mirror.snapshot().screen, "");
    }

    #[test]
    fn reads_cursor_row_text_and_column() {
        let mut mirror = TerminalMirror::new(20, 5);
        mirror.write(b"first\r\n>>> ");
        let (text, cursor_x) = mirror.cursor_row();
        assert_eq!(text, ">>>");
        assert_eq!(cursor_x, 4);
    }

    #[test]
    fn cursor_logical_line_joins_soft_wrapped_rows() {
        let mut mirror = TerminalMirror::new(10, 5);
        mirror.write(b"abcdefghij#? hello");
        assert_eq!(mirror.cursor_logical_line(), "abcdefghij#? hello");
    }

    #[test]
    fn cursor_logical_line_stops_at_hard_line_breaks() {
        let mut mirror = TerminalMirror::new(20, 5);
        mirror.write(b"previous line\r\n$ ls #? explain");
        assert_eq!(mirror.cursor_logical_line(), "$ ls #? explain");
    }
}

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
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::{Color, Processor};

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

/// The cursor's logical line sampled for presentation's anchored streaming (see
/// [`TerminalMirror::live_region`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveRegionSnapshot {
    /// Styled text per row, top to bottom: wrapped rows full width, the last row
    /// right-trimmed. Writing them joined (no separators) reproduces the region,
    /// including its soft wrapping.
    pub styled_rows: Vec<String>,
    /// Plain character count of the trimmed last row, for cursor-column padding.
    pub last_row_chars: usize,
    /// Cursor column on the last row.
    pub cursor_col: u16,
    /// Terminal pending-wrap state at the cursor.
    pub needs_wrap: bool,
    /// Plain right-trimmed text of the row directly above the region; `None` when
    /// the region starts at the top of the viewport.
    pub row_above: Option<String>,
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

    /// Whether the terminal is in the pending-wrap state (the cursor sits on the
    /// last column after filling it; the next printed character wraps). Presentation
    /// must know this to resume an exactly-full line without losing a character —
    /// cursor-movement sequences clear the pending wrap.
    pub fn cursor_needs_wrap(&self) -> bool {
        self.term.grid().cursor.input_needs_wrap
    }

    /// Samples the cursor's logical line (the "live region": prompt plus echoed
    /// input, spanning soft-wrapped rows) for presentation's anchored streaming —
    /// the rows to erase and rewrite when inserting AI content above the user's
    /// live input line. `None` on the alternate screen, and in the rare state where
    /// the cursor rests mid-logical-line (its row wraps onward), which a
    /// rewrite-below could not restore.
    pub fn live_region(&self) -> Option<LiveRegionSnapshot> {
        if self.is_alt_screen() {
            return None;
        }
        let cursor = self.term.grid().cursor.point;
        let cursor_row = cursor.line.0.max(0);
        if self.row_wraps(cursor_row) {
            return None;
        }
        let mut top = cursor_row;
        while top > 0 && self.row_wraps(top - 1) {
            top -= 1;
        }
        let mut styled_rows = Vec::with_capacity((cursor_row - top + 1) as usize);
        for row in top..=cursor_row {
            let visible_chars = if row < cursor_row {
                // A wrapped row is full width by construction; keep it whole so
                // rewriting the joined rows reproduces the same wrapping.
                self.columns as usize
            } else {
                self.row_text(row).trim_end().chars().count()
            };
            styled_rows.push(self.row_styled(row, visible_chars));
        }
        Some(LiveRegionSnapshot {
            styled_rows,
            last_row_chars: self.row_text(cursor_row).trim_end().chars().count(),
            cursor_col: cursor.column.0 as u16,
            needs_wrap: self.term.grid().cursor.input_needs_wrap,
            row_above: (top > 0).then(|| self.row_text(top - 1).trim_end().to_string()),
        })
    }

    /// One row with the cells' SGR styling (colors and the common attributes)
    /// re-synthesized into the text, covering the first `visible_chars` characters;
    /// ends with a reset when any styling was emitted.
    ///
    /// The original byte stream is not retained per row, so this reconstructs
    /// escape sequences from the grid cells. Attributes without a cell
    /// representation here (underline colors, hyperlinks) are accepted misses.
    fn row_styled(&self, row: i32, visible_chars: usize) -> String {
        let grid = self.term.grid();
        let line = &grid[Line(row)];
        let mut text = String::new();
        let mut emitted = 0usize;
        // The SGR parameter list currently applied to `text` ("" = default).
        let mut active = String::new();
        for col in 0..self.columns as usize {
            if emitted >= visible_chars {
                break;
            }
            let cell = &line[Column(col)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let params = sgr_params(cell);
            if params != active {
                if !active.is_empty() {
                    text.push_str("\x1b[0m");
                }
                if !params.is_empty() {
                    text.push_str(&format!("\x1b[{params}m"));
                }
                active = params;
            }
            text.push(cell.c);
            emitted += 1;
        }
        if !active.is_empty() {
            text.push_str("\x1b[0m");
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

/// The SGR parameter list (joined with `;`) that reproduces a cell's styling from the
/// default state; empty for an unstyled cell. Covers the common attribute subset
/// (bold, dim, italic, underline, inverse, strikeout) plus foreground/background
/// colors in their named, indexed, and truecolor forms.
fn sgr_params(cell: &Cell) -> String {
    let mut params: Vec<String> = Vec::new();
    for (flag, code) in [
        (Flags::BOLD, "1"),
        (Flags::DIM, "2"),
        (Flags::ITALIC, "3"),
        (Flags::UNDERLINE, "4"),
        (Flags::INVERSE, "7"),
        (Flags::STRIKEOUT, "9"),
    ] {
        if cell.flags.contains(flag) {
            params.push(code.to_string());
        }
    }
    color_params(cell.fg, true, &mut params);
    color_params(cell.bg, false, &mut params);
    params.join(";")
}

/// Appends the SGR parameters for one color. Named colors past the 16-color range
/// (the terminal's default foreground/background and palette specials) are the
/// default state and emit nothing.
fn color_params(color: Color, foreground: bool, params: &mut Vec<String>) {
    let (base, bright_base, extended) = if foreground {
        (30, 90, 38)
    } else {
        (40, 100, 48)
    };
    match color {
        Color::Named(named) => {
            let index = named as usize;
            if index < 8 {
                params.push((base + index).to_string());
            } else if index < 16 {
                params.push((bright_base + index - 8).to_string());
            }
        }
        Color::Indexed(index) => {
            let index = index as usize;
            if index < 8 {
                params.push((base + index).to_string());
            } else if index < 16 {
                params.push((bright_base + index - 8).to_string());
            } else {
                params.push(format!("{extended};5;{index}"));
            }
        }
        Color::Spec(rgb) => {
            params.push(format!("{extended};2;{};{};{}", rgb.r, rgb.g, rgb.b));
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
    fn live_region_reconstructs_styled_sgr_runs() {
        let mut mirror = TerminalMirror::new(20, 5);
        // A two-tone prompt: bold green marker, default-colored space, plain tail.
        mirror.write(b"first\r\n\x1b[1;32m>\x1b[0m x");
        let region = mirror.live_region().expect("live region");
        assert_eq!(region.styled_rows, vec!["\x1b[1;32m>\x1b[0m x".to_string()]);
        assert_eq!(region.last_row_chars, 3);
        assert_eq!(region.cursor_col, 3);
        assert!(!region.needs_wrap);
        assert_eq!(region.row_above.as_deref(), Some("first"));

        // Replaying the styled text through a fresh mirror reproduces the row.
        let mut replay = TerminalMirror::new(20, 5);
        replay.write(region.styled_rows[0].as_bytes());
        assert_eq!(replay.cursor_row().0, "> x");
    }

    #[test]
    fn live_region_keeps_indexed_and_truecolor() {
        let mut mirror = TerminalMirror::new(40, 5);
        mirror.write(b"\x1b[38;5;208ma\x1b[0m\x1b[38;2;1;2;3mb\x1b[0m");
        let region = mirror.live_region().expect("live region");
        let styled = &region.styled_rows[0];
        assert!(
            styled.contains("38;5;208"),
            "indexed color kept: {styled:?}"
        );
        assert!(styled.contains("38;2;1;2;3"), "truecolor kept: {styled:?}");
        assert!(styled.ends_with("\x1b[0m"), "trailing reset: {styled:?}");
        assert_eq!(region.row_above, None, "region starts at the viewport top");
    }

    #[test]
    fn live_region_spans_soft_wrapped_input() {
        let mut mirror = TerminalMirror::new(10, 5);
        mirror.write(b"before\r\n>>> abcdefXY");
        let region = mirror.live_region().expect("live region");
        assert_eq!(
            region.styled_rows,
            vec![">>> abcdef".to_string(), "XY".to_string()],
            "wrapped row kept full width, last row trimmed"
        );
        assert_eq!(region.last_row_chars, 2);
        assert_eq!(region.cursor_col, 2);
        assert_eq!(region.row_above.as_deref(), Some("before"));
    }

    #[test]
    fn live_region_reports_pending_wrap() {
        let mut mirror = TerminalMirror::new(10, 5);
        mirror.write(b"0123456789");
        let region = mirror.live_region().expect("live region");
        assert!(region.needs_wrap);
        assert_eq!(region.cursor_col, 9, "cursor parked on the last column");
        assert!(mirror.cursor_needs_wrap());
    }

    #[test]
    fn live_region_is_unavailable_on_the_alternate_screen() {
        let mut mirror = TerminalMirror::new(20, 5);
        mirror.write(b"\x1b[?1049h\x1b[2J\x1b[Halt");
        assert_eq!(mirror.live_region(), None);
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

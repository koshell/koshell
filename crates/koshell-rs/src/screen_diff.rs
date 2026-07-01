//! Line-level screen diffing, ported from the frozen `reference/src/screen-diff.ts`.
//!
//! The summary counts (`addedLines`/`removedLines`) are LCS-invariant, so they match the
//! prototype exactly regardless of the underlying diff library. Detailed hunks replicate
//! the prototype's removed-then-added ordering and 1-based line numbering.

use serde::{Deserialize, Serialize};
use similar::{Algorithm, DiffOp, capture_diff_slices};

/// Line-count summary of a screen change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenDiffSummary {
    pub added_lines: usize,
    pub removed_lines: usize,
    pub changed_lines: usize,
}

/// The kind of a detailed hunk line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HunkLineKind {
    Added,
    Removed,
}

/// One line within a detailed diff hunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenDiffHunkLine {
    #[serde(rename = "type")]
    pub kind: HunkLineKind,
    pub line: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_line_number: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_line_number: Option<usize>,
}

/// A contiguous block of changed lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenDiffHunk {
    pub old_start: usize,
    pub old_lines: usize,
    pub new_start: usize,
    pub new_lines: usize,
    pub lines: Vec<ScreenDiffHunkLine>,
}

/// A full screen text diff: summary counts plus detailed hunks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenTextDiff {
    pub added_lines: usize,
    pub removed_lines: usize,
    pub changed_lines: usize,
    pub hunks: Vec<ScreenDiffHunk>,
}

/// Splits a screen into lines. An empty screen is zero lines (not one empty line).
fn split_screen_lines(screen: &str) -> Vec<&str> {
    if screen.is_empty() {
        Vec::new()
    } else {
        screen.split('\n').collect()
    }
}

/// Computes the summary-only diff between two screens.
pub fn summarize_screen_diff(before: &str, after: &str) -> ScreenDiffSummary {
    let diff = diff_screen_text(before, after);
    ScreenDiffSummary {
        added_lines: diff.added_lines,
        removed_lines: diff.removed_lines,
        changed_lines: diff.changed_lines,
    }
}

/// Computes the full diff (summary + hunks) between two screens.
pub fn diff_screen_text(before: &str, after: &str) -> ScreenTextDiff {
    let old = split_screen_lines(before);
    let new = split_screen_lines(after);
    let ops = capture_diff_slices(Algorithm::Myers, &old, &new);

    let mut added_lines = 0;
    let mut removed_lines = 0;
    for op in &ops {
        match op {
            DiffOp::Insert { new_len, .. } => added_lines += new_len,
            DiffOp::Delete { old_len, .. } => removed_lines += old_len,
            DiffOp::Replace {
                old_len, new_len, ..
            } => {
                removed_lines += old_len;
                added_lines += new_len;
            }
            DiffOp::Equal { .. } => {}
        }
    }

    ScreenTextDiff {
        added_lines,
        removed_lines,
        changed_lines: added_lines + removed_lines,
        hunks: build_hunks(&ops, &old, &new),
    }
}

/// Accumulates hunks while walking the diff ops, mirroring the prototype's flush logic.
struct HunkBuilder {
    hunks: Vec<ScreenDiffHunk>,
    pending: Vec<ScreenDiffHunkLine>,
    old_start: Option<usize>,
    new_start: Option<usize>,
    old_lines: usize,
    new_lines: usize,
    old_line_number: usize,
    new_line_number: usize,
}

impl HunkBuilder {
    fn new() -> Self {
        Self {
            hunks: Vec::new(),
            pending: Vec::new(),
            old_start: None,
            new_start: None,
            old_lines: 0,
            new_lines: 0,
            old_line_number: 1,
            new_line_number: 1,
        }
    }

    fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        self.hunks.push(ScreenDiffHunk {
            old_start: self.old_start.unwrap_or(self.old_line_number),
            old_lines: self.old_lines,
            new_start: self.new_start.unwrap_or(self.new_line_number),
            new_lines: self.new_lines,
            lines: std::mem::take(&mut self.pending),
        });
        self.old_start = None;
        self.new_start = None;
        self.old_lines = 0;
        self.new_lines = 0;
    }

    fn removed(&mut self, line: &str) {
        self.old_start.get_or_insert(self.old_line_number);
        self.new_start.get_or_insert(self.new_line_number);
        self.pending.push(ScreenDiffHunkLine {
            kind: HunkLineKind::Removed,
            line: line.to_string(),
            old_line_number: Some(self.old_line_number),
            new_line_number: None,
        });
        self.old_line_number += 1;
        self.old_lines += 1;
    }

    fn added(&mut self, line: &str) {
        self.old_start.get_or_insert(self.old_line_number);
        self.new_start.get_or_insert(self.new_line_number);
        self.pending.push(ScreenDiffHunkLine {
            kind: HunkLineKind::Added,
            line: line.to_string(),
            old_line_number: None,
            new_line_number: Some(self.new_line_number),
        });
        self.new_line_number += 1;
        self.new_lines += 1;
    }

    fn equal(&mut self, len: usize) {
        self.flush();
        self.old_line_number += len;
        self.new_line_number += len;
    }
}

fn build_hunks(ops: &[DiffOp], old: &[&str], new: &[&str]) -> Vec<ScreenDiffHunk> {
    let mut builder = HunkBuilder::new();

    for op in ops {
        match *op {
            DiffOp::Equal { len, .. } => builder.equal(len),
            DiffOp::Delete {
                old_index, old_len, ..
            } => {
                for line in &old[old_index..old_index + old_len] {
                    builder.removed(line);
                }
            }
            DiffOp::Insert {
                new_index, new_len, ..
            } => {
                for line in &new[new_index..new_index + new_len] {
                    builder.added(line);
                }
            }
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                for line in &old[old_index..old_index + old_len] {
                    builder.removed(line);
                }
                for line in &new[new_index..new_index + new_len] {
                    builder.added(line);
                }
            }
        }
    }

    builder.flush();
    builder.hunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_no_changes_for_identical_screens() {
        assert_eq!(
            summarize_screen_diff("one\ntwo", "one\ntwo"),
            ScreenDiffSummary {
                added_lines: 0,
                removed_lines: 0,
                changed_lines: 0,
            }
        );
        assert!(diff_screen_text("one\ntwo", "one\ntwo").hunks.is_empty());
    }

    #[test]
    fn counts_added_and_removed_lines() {
        assert_eq!(
            summarize_screen_diff("one\ntwo\nthree", "zero\none\nthree\nfour"),
            ScreenDiffSummary {
                added_lines: 2,
                removed_lines: 1,
                changed_lines: 3,
            }
        );
    }

    #[test]
    fn returns_detailed_hunks() {
        let diff = diff_screen_text("one\ntwo\nthree", "one\nTWO\nthree\nfour");
        assert_eq!(diff.added_lines, 2);
        assert_eq!(diff.removed_lines, 1);
        assert_eq!(diff.changed_lines, 3);
        assert_eq!(
            diff.hunks,
            vec![
                ScreenDiffHunk {
                    old_start: 2,
                    old_lines: 1,
                    new_start: 2,
                    new_lines: 1,
                    lines: vec![
                        ScreenDiffHunkLine {
                            kind: HunkLineKind::Removed,
                            line: "two".to_string(),
                            old_line_number: Some(2),
                            new_line_number: None,
                        },
                        ScreenDiffHunkLine {
                            kind: HunkLineKind::Added,
                            line: "TWO".to_string(),
                            old_line_number: None,
                            new_line_number: Some(2),
                        },
                    ],
                },
                ScreenDiffHunk {
                    old_start: 4,
                    old_lines: 0,
                    new_start: 4,
                    new_lines: 1,
                    lines: vec![ScreenDiffHunkLine {
                        kind: HunkLineKind::Added,
                        line: "four".to_string(),
                        old_line_number: None,
                        new_line_number: Some(4),
                    }],
                },
            ]
        );
    }

    #[test]
    fn treats_empty_screen_as_no_lines() {
        let added = diff_screen_text("", "first");
        assert_eq!(added.added_lines, 1);
        assert_eq!(added.removed_lines, 0);
        assert_eq!(added.changed_lines, 1);

        let removed = diff_screen_text("first", "");
        assert_eq!(removed.added_lines, 0);
        assert_eq!(removed.removed_lines, 1);
        assert_eq!(removed.changed_lines, 1);
    }
}

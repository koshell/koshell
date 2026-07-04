//! Terminal-state processing: records PTY facts into the timeline, keeps the mirror and
//! snapshots up to date, tracks command spans from shell-integration markers, and detects
//! `#?` questions. On a fire, assembles the context package the AI daemon will consume.
//!
//! Implements the revised `#?` design (`docs/design-0001-repl-command-completion.md`):
//!
//! - **Capture** is a mirror read of the cursor's logical line at the submit instant
//!   (Enter). The same read is the echo-verification arming check: input that is never
//!   echoed never appears in the mirror, so it can never trigger. The alternate screen
//!   disarms capture entirely. Capture runs inside command spans and in shells without
//!   integration; at the integrated shell prompt the marker layer owns `#?` instead,
//!   because rendered UI text (a fuzzy history finder's list and query line) is
//!   indistinguishable from typed input in the mirror.
//! - **Suppression**: a lightweight quote-parity tracker (single quote, double quote,
//!   backslash) ignores `#?` inside unclosed quotes.
//! - **Firing** follows the layered authority: a shell `command_end` marker is
//!   authoritative (including failures); otherwise output stabilization fires — quiescence
//!   with escalating debounce tiers, a prompt-shape heuristic that only modulates debounce
//!   speed (never gates), and a bounded max-wait fallback so a pending question is never
//!   silently lost.
//! - **Pending-trigger interaction**: a delayed receipt notice (~1 s), user-typed Ctrl+C
//!   cancels pending questions (autonomous failures still fire via `command_end`), and a
//!   bare Esc cancels the most recent pending question without killing the command.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::context::{TerminalContextOptions, build_terminal_context};
use crate::mirror::TerminalMirror;
use crate::screen_diff::summarize_screen_diff;
use crate::shell_integration::{MarkerKind, ShellIntegrationMarker};
use crate::timeline::{InMemoryTimelineStore, TerminalEvent};

/// The stable AI context contract version (kept in sync with the daemon).
const AI_CONTEXT_CONTRACT_VERSION: &str = "koshell_ai_context_v1";

/// The `#?` trigger token.
const TRIGGER_TOKEN: &str = "#?";

// Stabilization debounce tiers. The prompt-shape heuristic selects the tier; it never
// gates firing. All values are indicative and dogfooding-tunable (see design 0001).
//
// In-program tiers apply to questions submitted inside a foreground child (REPLs, remote
// shells), where no `command_end` marker will ever arrive.
const IN_PROGRAM_TIER_PROMPT: Duration = Duration::from_millis(150);
const IN_PROGRAM_TIER_SHORT: Duration = Duration::from_millis(500);
const IN_PROGRAM_TIER_OTHER: Duration = Duration::from_secs(3);
const IN_PROGRAM_MAX_WAIT: Duration = Duration::from_secs(30);
// Prompt-line tiers apply to questions submitted at the shell prompt. They are
// conservative so that for terminating commands the authoritative `command_end` marker
// wins the race, and stabilization only fires for non-terminating commands (`pnpm dev`,
// `ssh`, watchers) — annotated that the command may still be running.
const PROMPT_LINE_TIER_PROMPT: Duration = Duration::from_millis(750);
const PROMPT_LINE_TIER_SHORT: Duration = Duration::from_secs(3);
const PROMPT_LINE_TIER_OTHER: Duration = Duration::from_secs(10);
const PROMPT_LINE_MAX_WAIT: Duration = Duration::from_secs(120);

/// How long a question may stay pending before presentation prints the one dim
/// "waiting for output to settle" receipt line.
const RECEIPT_NOTICE_DELAY: Duration = Duration::from_secs(1);

/// A resting line longer than this never counts as prompt-like or short.
const PROMPT_LIKE_MAX_CHARS: usize = 40;

/// Characters that commonly end a prompt (`$`, `#`, `>`, `%`, `:`), plus interactive
/// question/confirmation tails. Shapes only — no learned prompt templates.
const PROMPT_TAIL_CHARS: &[char] = &['$', '#', '%', '>', ':', '?', '❯', '»', '›'];

/// Which of the two emergent `#?` forms a question took. One firing rule covers both;
/// this is annotation for the AI, not a code path selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerForm {
    Standalone,
    Inline,
}

impl TriggerForm {
    fn as_str(self) -> &'static str {
        match self {
            TriggerForm::Standalone => "standalone",
            TriggerForm::Inline => "inline",
        }
    }
}

/// Which completion authority fired a question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    /// Authoritative shell `command_end` marker (including failures).
    CommandEnd,
    /// Output quiescence reached the debounce tier for the resting screen shape.
    Stabilized,
    /// Output never went quiet; the bounded max-wait fired so the question is not lost.
    MaxWait,
}

impl CompletionKind {
    fn as_str(self) -> &'static str {
        match self {
            CompletionKind::CommandEnd => "command_end",
            CompletionKind::Stabilized => "stabilized",
            CompletionKind::MaxWait => "max_wait",
        }
    }
}

/// A detected `#?` question with the terminal context to answer it.
#[derive(Debug, Clone)]
pub struct Trigger {
    pub question: String,
    pub completion: CompletionKind,
    /// True when the triggering command had not returned to the prompt at fire time.
    pub still_running: bool,
    pub context_package: serde_json::Value,
}

/// What the session loop should do as a result of processing terminal events.
#[derive(Debug)]
pub enum Action {
    /// Send the question to the AI daemon and print the local feedback line.
    Fire(Trigger),
    /// Print a one-line presentation notice (receipt delay, cancellation).
    Notice(String),
}

/// Where a pending question was submitted; selects the stabilization tier set and the
/// still-running annotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingOrigin {
    /// A shell command line: created from a `command_start` marker (integrated shells)
    /// or Enter-captured at the prompt (shells without integration). A `command_end`
    /// marker, when it exists, is expected to win the race against stabilization.
    PromptLine,
    /// Inside a foreground child (REPL, remote shell): stabilization is the only signal.
    InProgram,
}

/// A submitted `#?` waiting for its line's completion or stabilization point.
#[derive(Debug)]
struct PendingQuestion {
    question: String,
    form: TriggerForm,
    origin: PendingOrigin,
    submitted_at: Instant,
    receipt_notified: bool,
}

/// The shape of the line the cursor is resting on; modulates debounce speed only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestingShape {
    /// Cursor at the end of a short line ending in a prompt-like character.
    Prompt,
    /// Cursor at the end of a short non-prompt line (e.g. an inline question).
    Short,
    /// Anything else: empty line, long line, or cursor resting mid-text.
    Other,
}

fn stabilization_tier(origin: PendingOrigin, shape: RestingShape) -> Duration {
    match (origin, shape) {
        (PendingOrigin::InProgram, RestingShape::Prompt) => IN_PROGRAM_TIER_PROMPT,
        (PendingOrigin::InProgram, RestingShape::Short) => IN_PROGRAM_TIER_SHORT,
        (PendingOrigin::InProgram, RestingShape::Other) => IN_PROGRAM_TIER_OTHER,
        (PendingOrigin::PromptLine, RestingShape::Prompt) => PROMPT_LINE_TIER_PROMPT,
        (PendingOrigin::PromptLine, RestingShape::Short) => PROMPT_LINE_TIER_SHORT,
        (PendingOrigin::PromptLine, RestingShape::Other) => PROMPT_LINE_TIER_OTHER,
    }
}

fn max_wait(origin: PendingOrigin) -> Duration {
    match origin {
        PendingOrigin::InProgram => IN_PROGRAM_MAX_WAIT,
        PendingOrigin::PromptLine => PROMPT_LINE_MAX_WAIT,
    }
}

/// The instant a pending question's quiescence is measured from: the later of the
/// last PTY output and the question's submission. Output that predates the submit
/// (the echo of the user's own typing before a thinking pause) must not count as
/// settled — otherwise the question fires before the program has had any chance to
/// respond to the Enter (echo the newline, print the next prompt), the dispatch
/// samples a mid-line cursor instead of the resting prompt, and the echo ends up
/// buffered behind the response (fix 0003).
fn quiet_from(last_output_at: Option<Instant>, pending: &PendingQuestion) -> Instant {
    last_output_at.map_or(pending.submitted_at, |last_output| {
        last_output.max(pending.submitted_at)
    })
}

/// Owns the timeline and mirror and applies terminal events as they happen.
pub struct SessionState {
    timeline: InMemoryTimelineStore,
    mirror: TerminalMirror,
    previous_snapshot: Option<(String, String)>,
    next_snapshot_id: u64,
    next_command_id: u64,
    command_active: bool,
    /// Whether shell-integration markers exist in this session. When they do, the marker
    /// layer owns `#?` at the shell prompt exclusively and submit-time mirror capture is
    /// armed only inside command spans: the mirror cannot tell typed input from
    /// program-rendered text (a fuzzy history finder renders `#?` history entries and its
    /// own query line right where the cursor rests), but a marker only ever exists for a
    /// line the shell really accepted.
    shell_integrated: bool,
    pending: VecDeque<PendingQuestion>,
    last_output_at: Option<Instant>,
    /// Guards against a repeated Enter (key repeat, `\r\n` pairs) re-capturing the same
    /// rendered line; cleared whenever new PTY output changes the mirror.
    captured_since_output: bool,
    /// True when the current command span's question was already settled outside the
    /// `command_end` path (fired at stabilization, or cancelled by Ctrl+C / Esc), so the
    /// marker must not extract and fire it again.
    span_settled: bool,
}

impl SessionState {
    pub fn new(columns: u16, rows: u16, shell_integrated: bool) -> Self {
        Self {
            timeline: InMemoryTimelineStore::new(),
            mirror: TerminalMirror::new(columns, rows),
            previous_snapshot: None,
            next_snapshot_id: 1,
            next_command_id: 1,
            command_active: false,
            shell_integrated,
            pending: VecDeque::new(),
            last_output_at: None,
            captured_since_output: false,
            span_settled: false,
        }
    }

    /// Whether a bare Esc currently cancels a pending question. Armed only while at least
    /// one question is pending and the alternate screen is not active (`vim file #? q`
    /// must never lose vim's Esc).
    pub fn esc_cancellable(&self) -> bool {
        !self.pending.is_empty() && !self.mirror.is_alt_screen()
    }

    /// Whether the alternate screen is active (a full-screen program owns the
    /// keys, so koshell must not claim interrupts or cancels).
    pub fn alt_screen(&self) -> bool {
        self.mirror.is_alt_screen()
    }

    /// Records human keystrokes sent to the shell. Detects submits (Enter → mirror-read
    /// capture) and user interrupts (Ctrl+C → cancel pending questions).
    pub fn record_input(&mut self, data: &[u8], now: Instant) -> Vec<Action> {
        if data.is_empty() {
            return Vec::new();
        }
        self.timeline.record(TerminalEvent::HumanInput {
            data: String::from_utf8_lossy(data).into_owned(),
            visible: true,
        });
        let mut actions = Vec::new();
        for &byte in data {
            match byte {
                // A user-typed interrupt withdraws the line's future output, so pending
                // questions are cancelled; autonomous failures still fire via the
                // authoritative `command_end` marker. Not applied on the alternate screen,
                // where Ctrl+C belongs to the full-screen program.
                0x03 if !self.mirror.is_alt_screen() => {
                    if self.command_active {
                        self.span_settled = true;
                    }
                    actions.extend(self.cancel_all_pending("^C"));
                }
                // Submit-time capture is armed inside command spans (REPLs, remote
                // shells) and in shells without integration hooks. At the integrated
                // shell prompt the marker layer owns `#?` (see `shell_integrated`).
                b'\r' | b'\n'
                    if (self.command_active || !self.shell_integrated)
                        && !self.mirror.is_alt_screen()
                        && !self.captured_since_output =>
                {
                    self.capture_submitted_line(now);
                }
                _ => {}
            }
        }
        actions
    }

    /// Reads the cursor's logical line from the mirror at the submit instant and records
    /// a pending question when it contains `#?` outside quotes. Reading the rendered line
    /// (not keystrokes) is robust to history recall, arrow edits, and multibyte input —
    /// and doubles as the echo-verification arming check.
    fn capture_submitted_line(&mut self, now: Instant) {
        let line = self.mirror.cursor_logical_line();
        let Some(split) = extract_question(&line) else {
            return;
        };
        self.captured_since_output = true;
        let origin = if self.command_active {
            PendingOrigin::InProgram
        } else {
            PendingOrigin::PromptLine
        };
        self.pending.push_back(PendingQuestion {
            question: split.question,
            form: question_form(&split.left),
            origin,
            submitted_at: now,
            receipt_notified: false,
        });
    }

    fn cancel_all_pending(&mut self, reason: &str) -> Vec<Action> {
        self.pending
            .drain(..)
            .map(|pending| Action::Notice(format!("#? cancelled ({reason}): {}", pending.question)))
            .collect()
    }

    /// Cancels the most recently submitted pending question (bare-Esc path, LIFO).
    /// Returns `None` when nothing is pending — the caller should forward the Esc.
    pub fn cancel_latest(&mut self) -> Option<Action> {
        if self.mirror.is_alt_screen() {
            return None;
        }
        let pending = self.pending.pop_back()?;
        if self.command_active && pending.origin == PendingOrigin::PromptLine {
            self.span_settled = true;
        }
        Some(Action::Notice(format!(
            "#? cancelled: {}",
            pending.question
        )))
    }

    /// Records visible PTY output, updates the mirror, and captures a snapshot. Output
    /// arrival resets the quiescence clock that stabilization firing debounces on.
    pub fn record_output(&mut self, visible: &[u8], now: Instant) {
        if visible.is_empty() {
            return;
        }
        self.timeline.record(TerminalEvent::PtyOutput {
            data: String::from_utf8_lossy(visible).into_owned(),
        });
        self.mirror.write(visible);
        self.record_snapshot();
        self.last_output_at = Some(now);
        self.captured_since_output = false;
    }

    /// Feeds presentation (koshell/AI) output into the mirror, keeping snapshots truthful
    /// to what the user sees (the mirror-feed invariant, design 0002). Presentation output
    /// is not PTY output: it is excluded from terminal text context and it does not reset
    /// the stabilization quiescence clock (otherwise our own notices would delay firing).
    pub fn record_presentation_output(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.mirror.write(data);
        self.record_snapshot();
    }

    /// The delay until the nearest pending-question deadline (receipt notice,
    /// stabilization tier, or max-wait), or `None` when there is nothing to wait for.
    /// Deadlines are suspended on the alternate screen; leaving it produces output,
    /// which re-enters this computation.
    pub fn next_deadline(&self, now: Instant) -> Option<Duration> {
        if self.pending.is_empty() || self.mirror.is_alt_screen() {
            return None;
        }
        let shape = self.resting_shape();
        let mut nearest: Option<Instant> = None;
        let mut consider = |candidate: Instant| {
            nearest = Some(match nearest {
                Some(current) => current.min(candidate),
                None => candidate,
            });
        };
        for pending in &self.pending {
            if !pending.receipt_notified {
                consider(pending.submitted_at + RECEIPT_NOTICE_DELAY);
            }
            consider(pending.submitted_at + max_wait(pending.origin));
            consider(
                quiet_from(self.last_output_at, pending)
                    + stabilization_tier(pending.origin, shape),
            );
        }
        nearest.map(|deadline| {
            deadline
                .saturating_duration_since(now)
                .max(Duration::from_millis(1))
        })
    }

    /// Applies time-based transitions: stabilization and max-wait fires, then receipt
    /// notices for questions that stay pending. Suspended on the alternate screen.
    pub fn poll(&mut self, now: Instant) -> Vec<Action> {
        if self.pending.is_empty() || self.mirror.is_alt_screen() {
            return Vec::new();
        }
        let shape = self.resting_shape();
        let last_output_at = self.last_output_at;

        let mut fired = Vec::new();
        let mut remaining = VecDeque::new();
        for pending in self.pending.drain(..) {
            let quiet_from = quiet_from(last_output_at, &pending);
            let stabilized = now.saturating_duration_since(quiet_from)
                >= stabilization_tier(pending.origin, shape);
            let maxed =
                now.saturating_duration_since(pending.submitted_at) >= max_wait(pending.origin);
            if stabilized {
                fired.push((pending, CompletionKind::Stabilized));
            } else if maxed {
                fired.push((pending, CompletionKind::MaxWait));
            } else {
                remaining.push_back(pending);
            }
        }
        self.pending = remaining;

        let mut actions = Vec::new();
        for (pending, completion) in fired {
            let still_running = self.command_active && pending.origin == PendingOrigin::PromptLine;
            if still_running {
                self.span_settled = true;
            }
            actions.push(self.fire(
                pending.question,
                pending.form,
                completion,
                None,
                still_running,
            ));
        }
        for pending in &mut self.pending {
            if !pending.receipt_notified
                && now.saturating_duration_since(pending.submitted_at) >= RECEIPT_NOTICE_DELAY
            {
                pending.receipt_notified = true;
                actions.push(Action::Notice(format!(
                    "#? waiting for output to settle: {}",
                    pending.question
                )));
            }
        }
        actions
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

    /// Applies a shell-integration marker.
    ///
    /// `command_start` carries the full typed line (zsh `preexec`; bash reads it from
    /// history inside the `DEBUG` trap) and is where a prompt-layer `#?` becomes a
    /// pending question — enabling stabilization firing on non-terminating commands.
    ///
    /// `command_end` is the authoritative completion signal: it fires every pending
    /// question (including in-program ones — the child has exited, so their completion
    /// points are certainly reached). Extraction from the end marker's command text
    /// remains the fallback when no pending exists (comment-only precmd fallbacks, or a
    /// start marker whose text was unavailable) — unless the span's question was already
    /// settled by stabilization or a cancel.
    pub fn handle_marker(&mut self, marker: ShellIntegrationMarker, now: Instant) -> Vec<Action> {
        match marker.kind {
            MarkerKind::CommandStart => {
                // Only a fresh span (transition from the prompt) resets the settled flag
                // and may carry a new span question.
                if !self.command_active {
                    self.span_settled = false;
                    if let Some(command) = &marker.command
                        && let Some(split) = extract_question(command)
                    {
                        self.pending.push_back(PendingQuestion {
                            question: split.question,
                            form: question_form(&split.left),
                            origin: PendingOrigin::PromptLine,
                            submitted_at: now,
                            receipt_notified: false,
                        });
                    }
                }
                if let Some(command) = marker.command {
                    let command_id = self.next_command_id();
                    self.command_active = true;
                    self.timeline.record(TerminalEvent::CommandStart {
                        command_id,
                        command,
                        cwd: None,
                    });
                }
                Vec::new()
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

                let mut actions = Vec::new();
                if self.pending.is_empty() {
                    if !self.span_settled
                        && let Some(split) = extract_question(&command)
                    {
                        let form = question_form(&split.left);
                        actions.push(self.fire(
                            split.question,
                            form,
                            CompletionKind::CommandEnd,
                            marker.exit_code,
                            false,
                        ));
                    }
                } else {
                    let drained: Vec<PendingQuestion> = self.pending.drain(..).collect();
                    for pending in drained {
                        actions.push(self.fire(
                            pending.question,
                            pending.form,
                            CompletionKind::CommandEnd,
                            marker.exit_code,
                            false,
                        ));
                    }
                }
                self.span_settled = false;
                actions
            }
        }
    }

    /// True when the mirror cursor rests at column 0 of an empty row, so presentation
    /// output can start on the current line without a leading blank line.
    pub fn at_line_start(&self) -> bool {
        let (text, cursor_x) = self.mirror.cursor_row();
        cursor_x == 0 && text.is_empty()
    }

    /// True when the cursor rests on a prompt-shaped line outside the alternate
    /// screen. Presentation uses this as the gate for anchored streaming and for
    /// notices that keep the prompt as the last line: stabilization fires only
    /// after the prompt has rendered, so the prompt cannot be buffered the way the
    /// shell-integrated path does — AI content is instead inserted above the live
    /// input line (see design 0005).
    pub fn resting_prompt(&self) -> bool {
        !self.mirror.is_alt_screen() && self.resting_shape() == RestingShape::Prompt
    }

    /// The live region (the cursor's logical line, styled) for presentation's
    /// anchored streaming; `None` on the alternate screen or when the cursor rests
    /// mid-logical-line. See [`TerminalMirror::live_region`].
    pub fn live_region(&self) -> Option<crate::mirror::LiveRegionSnapshot> {
        self.mirror.live_region()
    }

    /// Cursor facts sampled after presentation writes AI text: column, terminal
    /// pending-wrap state, and the right-trimmed plain text of the cursor row (the
    /// AI tail used by the anchored-streaming invariant check).
    pub fn cursor_probe(&self) -> (u16, bool, String) {
        let (text, cursor_x) = self.mirror.cursor_row();
        (cursor_x, self.mirror.cursor_needs_wrap(), text)
    }

    /// The shape of the line the cursor rests on, for the debounce modulator.
    fn resting_shape(&self) -> RestingShape {
        let (text, cursor_x) = self.mirror.cursor_row();
        if text.is_empty() {
            return RestingShape::Other;
        }
        let char_count = text.chars().count();
        // The cursor must rest at (or just past) the end of the text; a cursor mid-line
        // means output is being drawn, not awaited.
        if (cursor_x as usize) < char_count || char_count > PROMPT_LIKE_MAX_CHARS {
            return RestingShape::Other;
        }
        let last = text.chars().next_back().unwrap_or(' ');
        if PROMPT_TAIL_CHARS.contains(&last) {
            RestingShape::Prompt
        } else {
            RestingShape::Short
        }
    }

    fn fire(
        &mut self,
        question: String,
        form: TriggerForm,
        completion: CompletionKind,
        exit_code: Option<i32>,
        still_running: bool,
    ) -> Action {
        let context_package =
            self.build_context_package(&question, form, completion, exit_code, still_running);
        let request_id = format!("request-{}", self.next_command_id());
        self.timeline.record(TerminalEvent::AiRequest {
            request_id,
            question: question.clone(),
        });
        Action::Fire(Trigger {
            question,
            completion,
            still_running,
            context_package,
        })
    }

    fn next_command_id(&mut self) -> String {
        let id = format!("command-{}", self.next_command_id);
        self.next_command_id += 1;
        id
    }

    fn build_context_package(
        &self,
        question: &str,
        form: TriggerForm,
        completion: CompletionKind,
        exit_code: Option<i32>,
        still_running: bool,
    ) -> serde_json::Value {
        let context = build_terminal_context(&self.timeline, &TerminalContextOptions::default());
        let mut trigger = serde_json::json!({
            "form": form.as_str(),
            "completion": completion.as_str(),
            "stillRunning": still_running,
        });
        if let Some(exit_code) = exit_code {
            trigger["exitCode"] = serde_json::Value::Number(exit_code.into());
        }
        serde_json::json!({
            "contractVersion": AI_CONTEXT_CONTRACT_VERSION,
            "question": question,
            "trigger": trigger,
            "dynamicContext": serde_json::to_value(&context)
                .unwrap_or(serde_json::Value::Null),
        })
    }
}

/// A line split at its first `#?` outside quotes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct QuestionSplit {
    left: String,
    question: String,
}

/// Extracts a `#?` question from a line, tracking quote parity (single quote, double
/// quote, backslash — the common lexical subset across shells and REPL languages) so that
/// `echo "#? not a question"` does not trigger. Heredocs and triple quotes are accepted
/// misses.
fn extract_question(line: &str) -> Option<QuestionSplit> {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for (index, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if !in_single => escaped = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '#' if !in_single && !in_double && line[index..].starts_with(TRIGGER_TOKEN) => {
                return Some(QuestionSplit {
                    left: line[..index].to_string(),
                    question: line[index + TRIGGER_TOKEN.len()..].trim().to_string(),
                });
            }
            _ => {}
        }
    }
    None
}

/// Standalone when the left part is empty or carries no word characters (only a comment
/// prefix such as `//` or `--`, or a bare prompt); inline otherwise. Annotation only.
fn question_form(left: &str) -> TriggerForm {
    if left.trim().chars().all(|c| !c.is_alphanumeric()) {
        TriggerForm::Standalone
    } else {
        TriggerForm::Inline
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn end_marker(command: &str, exit_code: i32) -> ShellIntegrationMarker {
        ShellIntegrationMarker {
            kind: MarkerKind::CommandEnd,
            command: Some(command.to_string()),
            exit_code: Some(exit_code),
        }
    }

    fn start_marker(command: &str) -> ShellIntegrationMarker {
        ShellIntegrationMarker {
            kind: MarkerKind::CommandStart,
            command: Some(command.to_string()),
            exit_code: None,
        }
    }

    fn fires(actions: &[Action]) -> Vec<&Trigger> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::Fire(trigger) => Some(trigger),
                Action::Notice(_) => None,
            })
            .collect()
    }

    fn notices(actions: &[Action]) -> Vec<&str> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::Notice(text) => Some(text.as_str()),
                Action::Fire(_) => None,
            })
            .collect()
    }

    #[test]
    fn extracts_question_with_quote_parity() {
        let split = extract_question("ls -la #? explain this output").unwrap();
        assert_eq!(split.left, "ls -la ");
        assert_eq!(split.question, "explain this output");

        assert_eq!(
            extract_question("#? what happened").unwrap().question,
            "what happened"
        );
        assert_eq!(extract_question("echo #?").unwrap().question, "");
        assert_eq!(extract_question("ls -la"), None);

        // Inside unclosed quotes: suppressed.
        assert_eq!(extract_question("echo \"#? not a question\""), None);
        assert_eq!(extract_question("echo '#? nope'"), None);
        // Quotes closed before the token: fires.
        assert_eq!(
            extract_question("echo 'done' #? real question")
                .unwrap()
                .question,
            "real question"
        );
        // Escaped quote does not open a string.
        assert_eq!(
            extract_question("echo \\\" #? escaped").unwrap().question,
            "escaped"
        );
    }

    #[test]
    fn classifies_question_form() {
        assert_eq!(question_form(""), TriggerForm::Standalone);
        assert_eq!(question_form("// "), TriggerForm::Standalone);
        assert_eq!(question_form("-- "), TriggerForm::Standalone);
        assert_eq!(question_form(">>> "), TriggerForm::Standalone);
        assert_eq!(question_form("ls -la "), TriggerForm::Inline);
    }

    #[test]
    fn command_end_with_trigger_produces_context_package() {
        let t0 = Instant::now();
        let mut state = SessionState::new(80, 24, true);
        state.record_output(b"file-a\r\nfile-b\r\n", t0);

        let actions = state.handle_marker(end_marker("ls #? explain", 0), t0);
        let fired = fires(&actions);
        assert_eq!(fired.len(), 1);
        let trigger = fired[0];
        assert_eq!(trigger.question, "explain");
        assert_eq!(trigger.completion, CompletionKind::CommandEnd);
        assert!(!trigger.still_running);
        assert_eq!(
            trigger.context_package["contractVersion"],
            AI_CONTEXT_CONTRACT_VERSION
        );
        assert_eq!(trigger.context_package["question"], "explain");
        assert_eq!(trigger.context_package["trigger"]["form"], "inline");
        assert_eq!(
            trigger.context_package["trigger"]["completion"],
            "command_end"
        );
        assert_eq!(trigger.context_package["trigger"]["exitCode"], 0);
        assert!(trigger.context_package["dynamicContext"].is_object());
    }

    #[test]
    fn command_end_without_trigger_returns_no_actions() {
        let mut state = SessionState::new(80, 24, true);
        let actions = state.handle_marker(end_marker("ls -la", 0), Instant::now());
        assert!(actions.is_empty());
    }

    #[test]
    fn quoted_trigger_in_markers_is_suppressed() {
        let t0 = Instant::now();
        let mut state = SessionState::new(80, 24, true);
        state.handle_marker(start_marker("echo \"#? not a question\""), t0);
        assert!(!state.esc_cancellable());
        let actions = state.handle_marker(end_marker("echo \"#? not a question\"", 0), t0);
        assert!(actions.is_empty());
    }

    #[test]
    fn span_question_from_start_marker_fires_once_at_command_end() {
        let t0 = Instant::now();
        let mut state = SessionState::new(80, 24, true);
        assert!(
            state
                .handle_marker(start_marker("ls #? explain"), t0)
                .is_empty()
        );
        assert!(state.esc_cancellable());

        let actions = state.handle_marker(end_marker("ls #? explain", 0), t0);
        let fired = fires(&actions);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].question, "explain");
        assert_eq!(fired[0].completion, CompletionKind::CommandEnd);
        assert_eq!(fired[0].context_package["trigger"]["form"], "inline");

        // The pending was consumed; nothing is left to fire or cancel.
        assert!(state.poll(t0 + Duration::from_secs(60)).is_empty());
        assert!(!state.esc_cancellable());
    }

    #[test]
    fn rendered_ui_text_at_integrated_prompt_does_not_trigger() {
        // Regression: fzf's Ctrl+R history widget paints `#?` history entries and its
        // own query line right where the cursor rests. Confirming the selection with
        // Enter happens at the prompt (no command span), so the marker layer owns `#?`
        // and the rendered UI text must not be captured.
        let t0 = Instant::now();
        let mut state = SessionState::new(80, 24, true);
        state.record_output(b"  #? a\r\n  2/2\r\n> #?", t0);
        state.record_input(b"\r", t0);
        assert!(!state.esc_cancellable());
        assert!(state.poll(t0 + Duration::from_secs(60)).is_empty());
    }

    #[test]
    fn non_integrated_prompt_capture_fires_via_stabilization() {
        // Without integration hooks no marker will ever come, so the prompt line is
        // Enter-captured and fired by stabilization.
        let t0 = Instant::now();
        let mut state = SessionState::new(80, 24, false);
        state.record_output(b"% ls #? explain", t0);
        state.record_input(b"\r", t0);
        state.record_output(b"\r\nfile-a\r\n% ", t0 + Duration::from_millis(30));

        // Resting on a short prompt-like line: the 750 ms prompt-line tier applies.
        let actions = state.poll(t0 + Duration::from_secs(1));
        let fired = fires(&actions);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].question, "explain");
        assert_eq!(fired[0].completion, CompletionKind::Stabilized);
        assert!(!fired[0].still_running);
    }

    #[test]
    fn repeated_enter_without_new_output_captures_once() {
        let t0 = Instant::now();
        let mut state = SessionState::new(80, 24, true);
        state.handle_marker(start_marker("python3"), t0);
        state.record_output(b">>> f() #? explain", t0);
        state.record_input(b"\r\r\n", t0);
        assert!(state.cancel_latest().is_some());
        assert!(state.cancel_latest().is_none());
    }

    #[test]
    fn in_program_question_fires_at_stabilization() {
        let t0 = Instant::now();
        let mut state = SessionState::new(80, 24, true);
        state.handle_marker(start_marker("python3"), t0);
        state.record_output(b">>> print(1) #? why", t0);
        state.record_input(b"\r", t0);

        // Resting line ends in "why" (short, non-prompt): the 500 ms tier applies.
        assert!(state.poll(t0 + Duration::from_millis(300)).is_empty());
        let actions = state.poll(t0 + Duration::from_millis(600));
        let fired = fires(&actions);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].question, "why");
        assert_eq!(fired[0].completion, CompletionKind::Stabilized);
        assert!(!fired[0].still_running);
        assert_eq!(
            fired[0].context_package["trigger"]["completion"],
            "stabilized"
        );
    }

    #[test]
    fn thinking_pause_before_enter_does_not_fire_before_the_echo() {
        // Regression (fix 0003): the user types the question, pauses to think, then
        // presses Enter. The pre-submit silence (last output = the echo of their own
        // typing) must not count as stabilization — otherwise the question fires
        // before the REPL echoes the newline, the dispatch samples a mid-line cursor
        // (no anchored streaming), and the echo gets buffered behind the response.
        let t0 = Instant::now();
        let mut state = SessionState::new(80, 24, true);
        state.handle_marker(start_marker("python3"), t0);
        state.record_output(b">>> #? why", t0);

        let submit = t0 + Duration::from_secs(2);
        assert!(fires(&state.record_input(b"\r", submit)).is_empty());
        assert!(
            state.poll(submit).is_empty(),
            "the question must not fire at the submit instant"
        );

        // The echo arrives; the prompt-shaped resting line stabilizes from there.
        state.record_output(b"\r\n>>> ", submit + Duration::from_millis(20));
        let actions = state.poll(submit + Duration::from_millis(300));
        let fired = fires(&actions);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].completion, CompletionKind::Stabilized);
    }

    #[test]
    fn silent_program_still_fires_a_tier_after_submission() {
        // A program that never responds to the Enter must still get its answer:
        // with no output after the submit, quiescence is measured from the
        // submission itself.
        let t0 = Instant::now();
        let mut state = SessionState::new(80, 24, true);
        state.handle_marker(start_marker("python3"), t0);
        state.record_output(b">>> #? why", t0);

        let submit = t0 + Duration::from_secs(2);
        state.record_input(b"\r", submit);
        // Resting line ends in "why" (short, non-prompt): the 500 ms tier applies.
        assert!(state.poll(submit + Duration::from_millis(300)).is_empty());
        let actions = state.poll(submit + Duration::from_millis(600));
        let fired = fires(&actions);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].completion, CompletionKind::Stabilized);
    }

    #[test]
    fn prompt_shaped_resting_line_selects_the_fast_tier() {
        let t0 = Instant::now();
        let mut state = SessionState::new(80, 24, true);
        state.handle_marker(start_marker("python3"), t0);
        state.record_output(b">>> #? what does this error mean", t0);
        state.record_input(b"\r", t0);
        // Echo of the newline; python prints nothing and the prompt returns.
        state.record_output(b"\r\n>>> ", t0 + Duration::from_millis(20));

        let actions = state.poll(t0 + Duration::from_millis(200));
        let fired = fires(&actions);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].completion, CompletionKind::Stabilized);
        assert_eq!(fired[0].context_package["trigger"]["form"], "standalone");
    }

    #[test]
    fn span_question_fires_at_stabilization_with_still_running() {
        let t0 = Instant::now();
        let mut state = SessionState::new(80, 24, true);
        // The start marker carries the full typed line (zsh preexec; bash history read).
        state.handle_marker(start_marker("pnpm dev #? explain the startup log"), t0);
        state.record_output(b"server ready on :3000\r\n", t0 + Duration::from_millis(50));

        // Resting on an empty line below the log: the conservative 10 s tier applies.
        let actions = state.poll(t0 + Duration::from_secs(11));
        let fired = fires(&actions);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].question, "explain the startup log");
        assert_eq!(fired[0].completion, CompletionKind::Stabilized);
        assert!(fired[0].still_running);
        assert_eq!(fired[0].context_package["trigger"]["stillRunning"], true);

        // The later command_end must not re-fire the settled question.
        let end_actions = state.handle_marker(
            end_marker("pnpm dev #? explain the startup log", 130),
            t0 + Duration::from_secs(60),
        );
        assert!(fires(&end_actions).is_empty());
    }

    #[test]
    fn max_wait_fires_when_output_never_settles() {
        let t0 = Instant::now();
        let mut state = SessionState::new(80, 24, true);
        state.handle_marker(start_marker("python3"), t0);
        state.record_output(b">>> stream() #? summarize", t0);
        state.record_input(b"\r", t0);

        // Keep output arriving so quiescence never happens; max-wait (30 s) fires.
        state.record_output(b"tick\r\n", t0 + Duration::from_secs(29));
        let actions = state.poll(t0 + Duration::from_secs(31));
        let fired = fires(&actions);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].completion, CompletionKind::MaxWait);
        assert_eq!(
            fired[0].context_package["trigger"]["completion"],
            "max_wait"
        );
    }

    #[test]
    fn receipt_notice_prints_once_after_one_second() {
        let t0 = Instant::now();
        let mut state = SessionState::new(80, 24, true);
        state.handle_marker(
            start_marker("some-quiet-batch-job --with --many --flags #? why is this slow"),
            t0,
        );
        // A long resting line selects the slow tier, keeping the question pending.
        state.record_output(b"working on a long step, please wait patiently...", t0);

        assert!(state.poll(t0 + Duration::from_millis(500)).is_empty());
        let first = state.poll(t0 + Duration::from_millis(1100));
        let first_notices = notices(&first);
        assert_eq!(first_notices.len(), 1);
        assert!(first_notices[0].contains("waiting for output to settle"));
        // Only once.
        assert!(state.poll(t0 + Duration::from_millis(1500)).is_empty());
    }

    #[test]
    fn ctrl_c_cancels_pending_and_suppresses_command_end_refire() {
        let t0 = Instant::now();
        let mut state = SessionState::new(80, 24, true);
        state.handle_marker(start_marker("sleep 30 #? why slow"), t0);

        let actions = state.record_input(&[0x03], t0 + Duration::from_secs(2));
        let cancel_notices = notices(&actions);
        assert_eq!(cancel_notices.len(), 1);
        assert!(cancel_notices[0].contains("cancelled (^C)"));

        // The interrupted command's end marker (exit 130) must not extract and re-fire.
        let end_actions = state.handle_marker(end_marker("sleep 30 #? why slow", 130), t0);
        assert!(fires(&end_actions).is_empty());
        // A later span behaves normally again.
        let next = state.handle_marker(end_marker("ls #? explain", 0), t0);
        assert_eq!(fires(&next).len(), 1);
    }

    #[test]
    fn esc_cancels_most_recent_pending_first() {
        let t0 = Instant::now();
        let mut state = SessionState::new(80, 24, true);
        state.handle_marker(start_marker("python3"), t0);
        state.record_output(b">>> first() #? first question", t0);
        state.record_input(b"\r", t0);
        state.record_output(b"\r\n>>> second() #? second question", t0);
        state.record_input(b"\r", t0);

        let first_cancel = state.cancel_latest().expect("one pending to cancel");
        match first_cancel {
            Action::Notice(text) => assert!(text.contains("second question")),
            Action::Fire(_) => panic!("expected a notice"),
        }
        let second_cancel = state.cancel_latest().expect("another pending to cancel");
        match second_cancel {
            Action::Notice(text) => assert!(text.contains("first question")),
            Action::Fire(_) => panic!("expected a notice"),
        }
        assert!(state.cancel_latest().is_none());
    }

    #[test]
    fn alternate_screen_disarms_capture_and_suspends_deadlines() {
        let t0 = Instant::now();
        let mut state = SessionState::new(80, 24, true);
        // Pending question exists from before the program went full-screen.
        state.handle_marker(start_marker("vim notes.txt #? what is in this file"), t0);
        state.record_output(b"\x1b[?1049h\x1b[2Jfile contents #? decoy", t0);

        // Enter inside the full-screen program captures nothing.
        state.record_input(b"\r", t0);
        // Deadlines and Esc-cancel are suspended while the alternate screen is active.
        assert!(state.next_deadline(t0 + Duration::from_secs(60)).is_none());
        assert!(state.poll(t0 + Duration::from_secs(60)).is_empty());
        assert!(!state.esc_cancellable());
        assert!(state.cancel_latest().is_none());

        // Leaving the alternate screen re-arms the pending question.
        state.record_output(b"\x1b[?1049l", t0 + Duration::from_secs(61));
        assert!(state.esc_cancellable());
        assert!(state.next_deadline(t0 + Duration::from_secs(61)).is_some());
    }

    #[test]
    fn at_line_start_tracks_the_mirror_cursor() {
        let t0 = Instant::now();
        let mut state = SessionState::new(80, 24, true);
        assert!(state.at_line_start());
        state.record_output(b">>> ", t0);
        assert!(!state.at_line_start());
        state.record_output(b"\r\n", t0);
        assert!(state.at_line_start());
    }

    #[test]
    fn resting_prompt_matches_prompt_shapes_only() {
        let t0 = Instant::now();
        let mut state = SessionState::new(80, 24, true);
        assert!(!state.resting_prompt());

        // A prompt-shaped resting line.
        state.record_output(b"2\r\n>>> ", t0);
        assert!(state.resting_prompt());

        // A short non-prompt resting line is not a prompt.
        state.record_output(b"\r\ndownloading", t0);
        assert!(!state.resting_prompt());

        // Suspended on the alternate screen.
        state.record_output(b"\x1b[?1049h\x1b[2J\x1b[H% ", t0);
        assert!(!state.resting_prompt());
    }

    #[test]
    fn cursor_probe_reports_the_ai_tail_facts() {
        let t0 = Instant::now();
        let mut state = SessionState::new(80, 24, true);
        state.record_output(b"answer line", t0);
        let (col, needs_wrap, tail) = state.cursor_probe();
        assert_eq!(col, 11);
        assert!(!needs_wrap);
        assert_eq!(tail, "answer line");
    }

    #[test]
    fn non_echoing_input_never_triggers() {
        let t0 = Instant::now();
        let mut state = SessionState::new(80, 24, true);
        state.handle_marker(start_marker("read -s answer"), t0);
        state.record_output(b"Password:", t0);
        // The typed `#?` is never echoed, so the mirror read finds nothing.
        state.record_input(b"#? is this armed\r", t0);
        assert!(!state.esc_cancellable());
        assert!(state.poll(t0 + Duration::from_secs(60)).is_empty());
    }
}

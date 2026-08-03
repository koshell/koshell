//! AI response presentation: renders streamed daemon replies into the user's terminal
//! while preserving the mirror-feed invariant (the mirror consumes exactly the bytes
//! written to the terminal, PTY plus presentation output, in the same order).
//!
//! Buffering follows design 0002's "buffer the bounded side" rule, in its prototype
//! simplification (revised by design 0010):
//! - **Command ended** at fire time: PTY visible output (the returning prompt) is held
//!   back from the moment the request is dispatched, the AI response streams first,
//!   and the held output flushes after it — so `#?` reads like a command that prints
//!   its answer before the next prompt. The answer and command output are kept in
//!   separate, labeled blocks — never line-interleaved. A size fuse bounds memory:
//!   when the held output nears the fuse, it is released as one labeled block behind a
//!   dim boundary notice and the answer resumes under a fresh `[koshell ai]` header,
//!   with subsequent command output buffered again. A stall deadline bounds waiting:
//!   after 30 seconds without an AI delta or visible tool activity, one dim notice
//!   tells the user the command output is held and Ctrl+C releases it — the hold is
//!   not force-flushed, so the answer and the held output never mix without the
//!   user's say-so. If nothing has
//!   rendered shortly after dispatch, one dim waiting notice tells the user the answer
//!   is coming.
//! - **Command still running**: program output keeps flowing in real time; deltas
//!   accumulate and the whole response is inserted as one block when it completes.
//!   Quiescence-gap insertion and its max-wait are deferred to dogfooding.
//!
//! Two line-position rules keep the chrome tight:
//! - Presentation lines start with a leading newline only when the cursor is not
//!   already resting at the start of an empty line, so the `#?` line is followed
//!   directly by koshell output instead of a blank line.
//! - When stabilization fired with an already-rendered prompt under the cursor (the
//!   REPL case — stabilization by definition waits for the prompt to settle, so the
//!   prompt cannot be buffered like the shell-integrated path), the response runs in
//!   **anchored streaming** (design 0005): the cursor's live input line stays usable
//!   and PTY output writes through in real time, while each delta is inserted into
//!   the free zone directly above it — erase the live region, continue the AI text
//!   where it left off (position and pending-wrap resumed from the mirror), then
//!   rewrite the live region below, styling and cursor column intact. A pre-redraw
//!   invariant check (the row above the live region must still be the AI tail)
//!   detects intervening program output — a mid-stream command's result, a screen
//!   clear — and degrades the rest of the response to one block at the end, so
//!   program output and AI text never interleave line-by-line.
//!
//! The AI output style (a dim `[koshell ai]` header) is a placeholder; design 0002
//! leaves the final prefix/style open. The block-release-and-stall-notice behavior is
//! specified in design 0010.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::time::{Duration, Instant};

use koshell_proto::ServerMessage;

use crate::event_log::{Event, EventLog};
use crate::mirror::LiveRegionSnapshot;
use crate::trigger::SessionState;

/// Buffered-PTY fuse: as the held output nears this, it is released as one labeled
/// block and the answer resumes buffering, so a user launching a new command
/// mid-response can neither grow the buffer without bound nor line-interleave the
/// answer with command output (design 0010).
const PTY_BUFFER_FUSE_BYTES: usize = 256 * 1024;

/// If nothing of the response has rendered this long after dispatch, print one dim
/// waiting notice (same philosophy as the pending-trigger receipt in design 0001).
/// Tunable during dogfooding.
const RECEIPT_NOTICE_DELAY: Duration = Duration::from_secs(1);

/// How long a stream-mode response may make no progress while holding PTY output before
/// telling the user it is stalled. Every AI delta and visible tool-activity event resets
/// the deadline. Past it, one dim notice reports that command output is held and that
/// Ctrl+C releases it; the hold is not force-flushed (design 0010), so the answer and
/// the held output never mix without the user's say-so — memory is bounded instead by
/// the fuse above. Tunable during dogfooding.
const STALL_NOTICE_DELAY: Duration = Duration::from_secs(30);

const AI_HEADER: &str = "\x1b[2m[koshell ai]\x1b[0m\r\n";

/// Erases the cursor's row (used to lift the live input line out of the way before
/// inserting presentation content above it).
const ERASE_LINE: &[u8] = b"\r\x1b[K";

/// How an in-flight response is rendered, decided by the trigger's completion state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Command ended: stream deltas, buffer PTY output until the response ends.
    Stream,
    /// Command still running: PTY flows, accumulate deltas, insert one block at end.
    Block,
}

/// The AI text's resume point for anchored streaming, sampled from the mirror after
/// each delta (the mirror cursor is the real cursor, so this is exact — no local
/// width bookkeeping, CJK safe).
#[derive(Debug)]
struct AiEnd {
    /// Cursor column at the end of the AI text.
    col: u16,
    /// Terminal pending-wrap state at the end of the AI text: the line is exactly
    /// full and must be resumed without cursor movement (movement clears the
    /// pending wrap and would overwrite the last column).
    needs_wrap: bool,
    /// Plain text of the AI tail row, for the pre-redraw invariant check.
    tail: String,
}

#[derive(Debug)]
struct ActiveResponse {
    request_id: String,
    mode: Mode,
    dispatched_at: Instant,
    /// Most recent meaningful AI progress: dispatch, a streamed delta, or visible
    /// tool activity. The stall deadline is measured from this point rather than
    /// total response age, so a healthy long answer is not mislabeled as stalled.
    last_progress_at: Instant,
    /// Whether the delayed waiting notice was printed.
    receipt_shown: bool,
    /// Stream mode: whether the header was written (deferred to the first delta).
    started: bool,
    /// Stream mode: whether the last written delta byte was a carriage return,
    /// so `\r\n` split across deltas is not doubled by normalization.
    last_was_cr: bool,
    /// Block mode: accumulated response text.
    accumulated: String,
    /// Non-anchored stream mode: PTY visible bytes held back while the response is
    /// in flight.
    buffered_pty: Vec<u8>,
    /// Non-anchored stream mode: the stall notice (design 0010) has fired, so it
    /// prints at most once.
    stall_notice_shown: bool,
    /// Non-anchored stream mode: a held block was just released mid-answer, so the
    /// next delta reprints the `[koshell ai]` header to relabel the resumed answer.
    resume_header_pending: bool,
    /// Anchored streaming (design 0005): the response was dispatched onto an
    /// already-rendered prompt, so deltas insert above the live input line and PTY
    /// output writes through in real time (never buffered).
    anchored: bool,
    /// Anchored streaming: the AI resume point; `None` until the first delta.
    ai_end: Option<AiEnd>,
    /// Dogfooding bookkeeping (design 0007): whether the response began anchored
    /// (`anchored` flips off on degrade), when the first delta arrived, how many
    /// deltas arrived, why the response degraded to block (if it did), whether the
    /// degrade event was emitted, and how many stdin chunks arrived mid-stream.
    began_anchored: bool,
    first_delta_at: Option<Instant>,
    delta_count: u32,
    degrade_reason: Option<&'static str>,
    degrade_emitted: bool,
    mid_stream_input_chunks: u32,
    /// Tool calls started and failed during this response (design 0021). Counted
    /// per response so "did this answer use tools, and did the user interrupt it"
    /// is one `response_end` line rather than a join across the log.
    tool_calls: u32,
    tool_failures: u32,
}

impl ActiveResponse {
    fn new(request_id: String, mode: Mode, dispatched_at: Instant) -> Self {
        Self {
            request_id,
            mode,
            dispatched_at,
            last_progress_at: dispatched_at,
            receipt_shown: false,
            started: false,
            last_was_cr: false,
            accumulated: String::new(),
            buffered_pty: Vec::new(),
            stall_notice_shown: false,
            resume_header_pending: false,
            anchored: false,
            ai_end: None,
            began_anchored: false,
            first_delta_at: None,
            delta_count: 0,
            degrade_reason: None,
            degrade_emitted: false,
            mid_stream_input_chunks: 0,
            tool_calls: 0,
            tool_failures: 0,
        }
    }

    /// The `response_end` dogfooding event for this response's bookkeeping.
    fn end_event(&self, status: &'static str, now: Instant) -> Event {
        Event::ResponseEnd {
            request_id: self.request_id.clone(),
            status,
            total_ms: now
                .saturating_duration_since(self.dispatched_at)
                .as_millis() as u64,
            first_delta_ms: self
                .first_delta_at
                .map(|at| at.saturating_duration_since(self.dispatched_at).as_millis() as u64),
            delta_count: self.delta_count,
            began_anchored: self.began_anchored,
            degraded_to_block: self.degrade_reason.is_some(),
            mid_stream_input_chunks: self.mid_stream_input_chunks,
            tool_calls: self.tool_calls,
            tool_failures: self.tool_failures,
        }
    }

    /// True while nothing of the response has been rendered to the terminal.
    fn nothing_rendered(&self) -> bool {
        !self.started && self.accumulated.is_empty()
    }

    /// True while stream-mode buffering is holding PTY output. Anchored streaming
    /// never buffers: the live input line stays usable in real time. Holding stays on
    /// for the whole stream response — the fuse releases a block and keeps buffering
    /// rather than giving up (design 0010).
    fn holding_pty(&self) -> bool {
        self.mode == Mode::Stream && !self.anchored
    }

    /// True once the response has made no progress long enough that held command
    /// output must be reported. The notice itself is conditional on output actually
    /// being held: a quiet response that holds nothing says nothing.
    fn stall_deadline_passed(&self, now: Instant) -> bool {
        !self.stall_notice_shown && now >= self.last_progress_at + STALL_NOTICE_DELAY
    }
}

/// Borrowed tool-activity payload used inside the presentation layer. Grouping the
/// protocol fields keeps the activity handler focused on one domain event rather than
/// a growing list of parallel string arguments.
struct ToolActivity<'a> {
    request_id: &'a str,
    tool_name: &'a str,
    phase: &'a str,
    message: &'a str,
}

/// Renders daemon server messages into the terminal and decides when PTY output is
/// written through versus buffered. One instance per session, driven by the
/// processor thread.
pub struct Presentation {
    /// Requests dispatched to the daemon that have not finished: request id ->
    /// still-running at fire time.
    dispatched: HashMap<String, bool>,
    active: Option<ActiveResponse>,
    /// Requests aborted by a user interrupt (Ctrl+C): rendering stopped locally,
    /// so late deltas and the daemon's terminal marker are dropped silently. The
    /// local stop is authoritative — the daemon-side cancel is only best-effort.
    aborted: HashSet<String>,
    /// Dogfooding event log (design 0007); inert by default.
    event_log: EventLog,
}

/// Normalizes `\n` to `\r\n` for a terminal in raw mode. `last_was_cr` carries the
/// final-byte state across chunks so a `\r\n` split between deltas stays intact.
fn normalize_newlines(text: &str, last_was_cr: &mut bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    for byte in text.bytes() {
        if byte == b'\n' && !*last_was_cr {
            out.push(b'\r');
        }
        *last_was_cr = byte == b'\r';
        out.push(byte);
    }
    out
}

/// The prefix that puts presentation output at the start of a line: empty when the
/// cursor already rests at the start of an empty line, a newline otherwise — so a
/// `#?` line is followed directly by koshell output instead of a blank line.
fn line_prefix(state: &SessionState) -> &'static str {
    if state.at_line_start() { "" } else { "\r\n" }
}

/// Normalizes the embedded newlines of a self-contained notice body to `\r\n` so a
/// multi-line message (e.g. the config-error example block) does not staircase in
/// raw mode, where a lone `\n` drops a row without returning to column 0. Distinct
/// from `normalize_newlines`, which threads CR state across streaming deltas; a
/// notice is emitted whole, so the CR state starts fresh each call.
fn notice_body(text: &str) -> String {
    let mut last_was_cr = false;
    // normalize_newlines only inserts a `\r` (0x0D) before a `\n` (0x0A), so the
    // result stays valid UTF-8.
    String::from_utf8(normalize_newlines(text, &mut last_was_cr))
        .unwrap_or_else(|_| text.to_owned())
}

/// Prints a dim presentation notice (single- or multi-line), mirror-fed like all
/// output. Internal newlines are CR-normalized so a multi-line body wraps cleanly.
pub(crate) fn notice<W: Write>(text: &str, out: &mut W, state: &mut SessionState) {
    let body = notice_body(text);
    let bytes = format!("{}\x1b[2m[koshell] {body}\x1b[0m\r\n", line_prefix(state));
    let _ = out.write_all(bytes.as_bytes());
    let _ = out.flush();
    state.record_presentation_output(bytes.as_bytes());
}

/// Releases the response's held PTY output as one block behind a dim boundary notice,
/// so the resumed command output is never mistaken for answer text (design 0010). If
/// the block is released mid-answer, the next delta reprints the `[koshell ai]` header
/// to relabel the answer. `reason` is the boundary line shown above the block. A no-op
/// when nothing is held.
fn release_held_block<W: Write>(
    active: &mut ActiveResponse,
    reason: &str,
    out: &mut W,
    state: &mut SessionState,
    now: Instant,
) {
    let buffered = std::mem::take(&mut active.buffered_pty);
    if buffered.is_empty() {
        return;
    }
    notice(reason, out, state);
    let _ = out.write_all(&buffered);
    let _ = out.flush();
    state.record_output(&buffered, now);
    // If the answer had already started rendering, relabel its continuation.
    if active.started {
        active.resume_header_pending = true;
    }
}

/// Reports that command output is held behind a stalled answer and that Ctrl+C
/// releases it (design 0010). Fires at most once per response, and only while output
/// is actually held — the invariant is that a hung daemon cannot hold command output
/// *silently*, not that every slow answer is announced twice.
fn stall_notice<W: Write>(active: &mut ActiveResponse, out: &mut W, state: &mut SessionState) {
    active.stall_notice_shown = true;
    active.receipt_shown = true;
    notice(
        "no AI progress for 30 seconds — press Ctrl+C to stop the AI and release the held command output",
        out,
        state,
    );
}

/// Appends the erase sequence for a live region: bottom-up from the cursor row to
/// the region's top row, ending at column 0 of the top row.
fn erase_region(bytes: &mut Vec<u8>, region: &LiveRegionSnapshot) {
    bytes.extend_from_slice(ERASE_LINE);
    for _ in 1..region.styled_rows.len() {
        bytes.extend_from_slice(b"\x1b[A\x1b[K");
    }
}

/// Appends the live region's rows (styled, joined so soft wrapping reproduces
/// itself) plus the spaces that put the cursor back on its live column. The caller
/// must have the cursor at column 0 of the row where the region should start.
fn restore_region(bytes: &mut Vec<u8>, region: &LiveRegionSnapshot) {
    for row in &region.styled_rows {
        bytes.extend_from_slice(row.as_bytes());
    }
    let padding = (region.cursor_col as usize).saturating_sub(region.last_row_chars);
    bytes.extend_from_slice(" ".repeat(padding).as_bytes());
}

/// Inserts presentation content (one or more complete lines, `\r\n`-terminated)
/// directly above the cursor's live input line, restoring the line — echoed input,
/// styling, soft wrapping, cursor column — below it. Returns `false` without
/// writing when the live region cannot be sampled (alternate screen, cursor
/// mid-logical-line); the caller falls back to a plain notice.
fn insert_above_live<W: Write>(lines: &[u8], out: &mut W, state: &mut SessionState) -> bool {
    let Some(region) = state.live_region() else {
        return false;
    };
    let mut bytes = Vec::with_capacity(lines.len() + 64);
    erase_region(&mut bytes, &region);
    bytes.extend_from_slice(lines);
    restore_region(&mut bytes, &region);
    let _ = out.write_all(&bytes);
    let _ = out.flush();
    state.record_presentation_output(&bytes);
    true
}

/// Renders one anchored-streaming delta: erase the live input line, resume the AI
/// text exactly where it left off in the free zone above, then rewrite the live
/// line below it — echoed input, styling, soft wrapping, and cursor column intact.
///
/// Before redrawing, the row directly above the live region must still be the AI
/// tail. When it is not (a mid-stream command printed its result, the screen was
/// cleared) or the live region cannot be sampled, the rest of the response degrades
/// to block mode — one seam at the end, never line-level interleaving with program
/// output.
fn anchored_delta<W: Write>(
    active: &mut ActiveResponse,
    delta: &str,
    out: &mut W,
    state: &mut SessionState,
) {
    let region = match state.live_region() {
        Some(region) => region,
        None => return degrade_to_block(active, delta, "live_region_unavailable"),
    };
    if let Some(ai_end) = &active.ai_end
        && region.row_above.as_deref() != Some(ai_end.tail.as_str())
    {
        return degrade_to_block(active, delta, "tail_mismatch");
    }

    // Phase 1: lift the live region and resume the AI text.
    let mut bytes = Vec::with_capacity(delta.len() + 64);
    erase_region(&mut bytes, &region);
    match &active.ai_end {
        // First delta: the header takes the erased top row; the AI text starts on
        // the line below it.
        None => bytes.extend_from_slice(AI_HEADER.as_bytes()),
        // The AI line is exactly full: the erased row below it is its natural wrap
        // continuation, and any cursor movement would clear the pending state.
        Some(ai_end) if ai_end.needs_wrap => {}
        Some(ai_end) => {
            bytes.extend_from_slice(format!("\x1b[A\x1b[{}G", ai_end.col + 1).as_bytes());
        }
    }
    active.started = true;
    bytes.extend(normalize_newlines(delta, &mut active.last_was_cr));
    let _ = out.write_all(&bytes);
    state.record_presentation_output(&bytes);

    // The mirror consumed the same bytes, so its cursor is the exact resume point.
    let (col, needs_wrap, tail) = state.cursor_probe();
    active.ai_end = Some(AiEnd {
        col,
        needs_wrap,
        tail,
    });

    // Phase 2: put the live region back below the AI text.
    let mut bytes = Vec::with_capacity(64);
    bytes.extend_from_slice(b"\r\n");
    restore_region(&mut bytes, &region);
    let _ = out.write_all(&bytes);
    let _ = out.flush();
    state.record_presentation_output(&bytes);
}

/// Switches the rest of an anchored response to block accumulation (the already
/// streamed part stays in the free zone; the remainder is inserted as one block at
/// response end). The reason is bookkeeping for the dogfooding degrade-frequency
/// metric; the caller emits the event (this function has no log access).
fn degrade_to_block(active: &mut ActiveResponse, delta: &str, reason: &'static str) {
    active.mode = Mode::Block;
    active.anchored = false;
    active.degrade_reason = Some(reason);
    active.accumulated.push_str(delta);
}

/// Prints a notice inserted above the live input line (echoed input and cursor
/// restored below it), falling back to a plain notice when the live region cannot
/// be sampled.
fn notice_above_live<W: Write>(text: &str, out: &mut W, state: &mut SessionState) {
    let body = notice_body(text);
    let line = format!("\x1b[2m[koshell] {body}\x1b[0m\r\n");
    if !insert_above_live(line.as_bytes(), out, state) {
        notice(text, out, state);
    }
}

/// Prints a notice keeping the live input line as the last line: when the cursor
/// rests on a prompt-shaped line, the notice is inserted above the live region, so
/// the program's input line stays where the user expects it.
pub(crate) fn notice_before_prompt<W: Write>(text: &str, out: &mut W, state: &mut SessionState) {
    if state.resting_prompt() {
        notice_above_live(text, out, state);
    } else {
        notice(text, out, state);
    }
}

impl Presentation {
    pub fn new() -> Self {
        Self {
            dispatched: HashMap::new(),
            active: None,
            aborted: HashSet::new(),
            event_log: EventLog::default(),
        }
    }

    /// Injects the session's event log; emit points stay inert without one.
    pub fn set_event_log(&mut self, event_log: EventLog) {
        self.event_log = event_log;
    }

    /// Counts a stdin chunk that arrived while a stream-mode response was in
    /// flight — the mid-stream typing usage metric (design 0007). The bytes
    /// themselves are never recorded.
    pub fn note_mid_stream_input(&mut self) {
        if let Some(active) = self.active.as_mut()
            && active.mode == Mode::Stream
        {
            active.mid_stream_input_chunks += 1;
        }
    }

    /// Whether a Ctrl+C currently belongs to the AI response rather than the
    /// child program: a stream-mode response is in flight, meaning the triggering
    /// command already ended and the AI is the only thing occupying the terminal.
    /// Block mode (command still running) never claims Ctrl+C — killing the
    /// foreground program is inviolable.
    pub fn owns_interrupt(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.mode == Mode::Stream)
    }

    /// Aborts the active response on a user interrupt (Ctrl+C): rendering stops
    /// immediately and everything the daemon still sends for this request is
    /// dropped. Returns the request id so the caller can send a best-effort
    /// `ai_cancel` to the daemon. `None` when no response is active (the caller
    /// treats the interrupt as ordinary input).
    pub fn user_interrupt<W: Write>(
        &mut self,
        out: &mut W,
        state: &mut SessionState,
        now: Instant,
    ) -> Option<String> {
        let active = self.active.take()?;
        self.dispatched.remove(&active.request_id);
        self.aborted.insert(active.request_id.clone());
        self.event_log.emit(active.end_event("interrupted", now));
        match active.mode {
            Mode::Stream if active.anchored => {
                // The streamed part stays in the free zone; close it with an
                // interrupted marker and keep the live input line last.
                notice_above_live("answer interrupted (^C)", out, state);
            }
            Mode::Stream => {
                if active.started {
                    let tail = b"\r\n";
                    let _ = out.write_all(tail);
                    state.record_presentation_output(tail);
                }
                // The interrupt notice doubles as the boundary before the released
                // command output, so the two are never confused (design 0010).
                if active.buffered_pty.is_empty() {
                    notice("answer interrupted (^C)", out, state);
                } else {
                    notice(
                        "answer interrupted (^C); releasing held command output",
                        out,
                        state,
                    );
                    let _ = out.write_all(&active.buffered_pty);
                    state.record_output(&active.buffered_pty, now);
                }
                let _ = out.flush();
            }
            Mode::Block => {
                // The accumulated text was never rendered; drop it. The Ctrl+C
                // was forwarded to the program, so only the withdrawal needs ink.
                notice_before_prompt("answer cancelled (^C)", out, state);
            }
        }
        Some(active.request_id)
    }

    /// Records a `#?` request handed to the daemon and the bounded-side decision made
    /// at fire time (whether the triggering command was still running). For a command
    /// that ended without a rendered prompt, PTY buffering starts here — before the
    /// first delta — so the returning prompt is already held while the daemon
    /// thinks; with a rendered prompt the response runs anchored instead.
    pub fn note_dispatch(
        &mut self,
        request_id: &str,
        still_running: bool,
        state: &SessionState,
        now: Instant,
    ) {
        self.dispatched
            .insert(request_id.to_string(), still_running);
        if self.active.is_none() {
            let mode = if still_running {
                Mode::Block
            } else {
                Mode::Stream
            };
            self.begin_response(request_id, mode, state, now);
        }
    }

    /// Creates the active response. In stream mode, when the cursor rests on an
    /// already-rendered prompt (the stabilization path fires only after the prompt
    /// settles, so it cannot be buffered like the shell-integrated path), the
    /// response runs anchored: deltas insert above the live input line, which stays
    /// fully usable in the meantime. Nothing is written at dispatch time.
    fn begin_response(&mut self, request_id: &str, mode: Mode, state: &SessionState, now: Instant) {
        let mut active = ActiveResponse::new(request_id.to_string(), mode, now);
        active.anchored = mode == Mode::Stream && state.resting_prompt();
        active.began_anchored = active.anchored;
        self.active = Some(active);
    }

    /// Routes PTY visible output: buffered while a stream-mode response is in flight,
    /// written through otherwise. When the held output reaches the fuse it is released
    /// as one labeled block and buffering continues (design 0010), so the answer and
    /// command output stay in separate blocks instead of interleaving line by line.
    pub fn pty_output<W: Write>(
        &mut self,
        visible: &[u8],
        out: &mut W,
        state: &mut SessionState,
        now: Instant,
    ) {
        if let Some(active) = self.active.as_mut()
            && active.holding_pty()
        {
            active.buffered_pty.extend_from_slice(visible);
            if active.buffered_pty.len() >= PTY_BUFFER_FUSE_BYTES {
                release_held_block(
                    active,
                    "held command output hit the size limit; releasing it (the answer resumes below)",
                    out,
                    state,
                    now,
                );
            } else if active.stall_deadline_passed(now) {
                // The stall deadline passed while nothing was held, so `poll` stayed
                // silent (design 0010 revision). Output arriving now is the first thing
                // actually held, and the invariant is that held output is never silent.
                stall_notice(active, out, state);
            }
            return;
        }
        let _ = out.write_all(visible);
        let _ = out.flush();
        state.record_output(visible, now);
    }

    /// Time until the next presentation deadline (waiting notice or stall notice),
    /// used to bound the processor's channel wait alongside the trigger deadlines.
    pub fn next_deadline(&self, now: Instant) -> Option<Duration> {
        let active = self.active.as_ref()?;
        let mut next: Option<Instant> = None;
        if !active.receipt_shown && active.nothing_rendered() {
            next = Some(active.dispatched_at + RECEIPT_NOTICE_DELAY);
        }
        // Do not schedule a progress deadline already in the past: `poll` handles it
        // immediately when output is held, and a later PTY write handles the nothing-held
        // case. A later AI delta or visible tool event moves `last_progress_at` forward
        // and re-arms the clock. Rescheduling the expired instant would spin the processor
        // on a zero-length wait.
        if active.holding_pty() && !active.stall_notice_shown {
            let stall = active.last_progress_at + STALL_NOTICE_DELAY;
            if stall > now {
                next = Some(next.map_or(stall, |n| n.min(stall)));
            }
        }
        next.map(|deadline| deadline.saturating_duration_since(now))
    }

    /// Fires due presentation deadlines: the delayed waiting notice, and the stall
    /// notice that reports held command output without force-flushing it.
    pub fn poll<W: Write>(&mut self, now: Instant, out: &mut W, state: &mut SessionState) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if !active.receipt_shown
            && active.nothing_rendered()
            && now >= active.dispatched_at + RECEIPT_NOTICE_DELAY
        {
            active.receipt_shown = true;
            if active.anchored {
                // The live input line stays the last line; the notice goes above it.
                notice_above_live("waiting for the AI answer…", out, state);
            } else {
                notice("waiting for the AI answer…", out, state);
            }
        }
        // Stall notice (design 0010): the answer has made no progress, but the held
        // output is not force-flushed — the user decides when to release it with Ctrl+C,
        // so the answer and command output never mix on their own. It fires only when
        // output is actually held; a quiet answer that holds nothing needs no notice,
        // because the waiting notice above already said the answer is pending and
        // Ctrl+C already stops it.
        if active.holding_pty()
            && active.stall_deadline_passed(now)
            && !active.buffered_pty.is_empty()
        {
            stall_notice(active, out, state);
        }
    }

    /// Applies one daemon message to the terminal.
    pub fn handle_server_message<W: Write>(
        &mut self,
        message: &ServerMessage,
        out: &mut W,
        state: &mut SessionState,
        now: Instant,
    ) {
        match message {
            // Receipt feedback is deadline-driven (see poll), not ack-driven.
            ServerMessage::Ack { .. } => {}
            ServerMessage::AiDelta { request_id, delta } => {
                self.on_delta(request_id, delta, out, state, now);
            }
            ServerMessage::AiResponseEnd { request_id } => {
                self.finish(request_id, None, out, state, now);
            }
            ServerMessage::AiError {
                request_id,
                message,
            } => {
                self.finish(request_id, Some(message), out, state, now);
            }
            ServerMessage::AiToolActivity {
                request_id,
                tool_name,
                phase,
                message,
            } => {
                self.on_tool_activity(
                    ToolActivity {
                        request_id,
                        tool_name,
                        phase,
                        message,
                    },
                    out,
                    state,
                    now,
                );
            }
            // The interactive session never sends a status_request or an auth
            // request, so their replies are not expected here; ignore them (and
            // any future additive message types) defensively.
            _ => {}
        }
    }

    /// Renders one tool-activity line, so the user can see what the AI is doing
    /// while it does it and interrupt if it is not what they wanted.
    ///
    /// The line is a seam in the answer, and the design 0010 rule applies: the answer
    /// and anything else stay in separate labeled blocks. Both stream paths therefore
    /// arrange for the resumed answer to be relabeled rather than continuing as if
    /// nothing had been printed between.
    ///
    /// This is also the terminal's only sighting of a daemon-served tool — `web_search`
    /// reaches it no other way — so every announcement is logged, including the ones
    /// that render nothing.
    fn on_tool_activity<W: Write>(
        &mut self,
        activity: ToolActivity<'_>,
        out: &mut W,
        state: &mut SessionState,
        now: Instant,
    ) {
        // A locally aborted request keeps rendering nothing, exactly like its deltas;
        // an unknown id is a stale reply. Both are still recorded: an announcement the
        // user never saw because they had just interrupted is precisely the case the
        // dogfooding question is about.
        let mut rendered = false;
        if !self.aborted.contains(activity.request_id)
            && let Some(active) = self.active.as_mut()
            && active.request_id == activity.request_id
        {
            // An unrecognized future phase counts as neither, so a later `progress`
            // phase cannot silently inflate the call count.
            match activity.phase {
                "started" => active.tool_calls += 1,
                "failed" => active.tool_failures += 1,
                _ => {}
            }
            // Showing the work *is* both receipt and progress: the delayed "waiting
            // for the AI answer" notice would only repeat what this line already says,
            // and a visible tool step proves the response is not stalled.
            active.receipt_shown = true;
            active.last_progress_at = now;

            if active.anchored {
                notice_above_live(activity.message, out, state);
                // The row above the live region is now this notice, not the AI tail, so
                // the resume point is gone. Dropping it makes the next delta reprint the
                // `[koshell ai]` header and start a fresh block below the notice, instead
                // of failing the tail check and degrading the rest of the answer to one
                // block at the end.
                active.ai_end = None;
            } else {
                notice(activity.message, out, state);
                if active.started {
                    active.resume_header_pending = true;
                }
            }
            rendered = true;
        }
        self.event_log.emit(Event::ToolActivity {
            request_id: activity.request_id.to_string(),
            tool_name: crate::event_log::identifier(activity.tool_name),
            phase: crate::event_log::identifier(activity.phase),
            rendered,
        });
    }

    fn on_delta<W: Write>(
        &mut self,
        request_id: &str,
        delta: &str,
        out: &mut W,
        state: &mut SessionState,
        now: Instant,
    ) {
        if self.aborted.contains(request_id) {
            return;
        }
        if self
            .active
            .as_ref()
            .is_none_or(|a| a.request_id != request_id)
        {
            // The daemon serializes responses, so a delta for a different request
            // means the previous one already finished. An unknown request id (a
            // stale reply) falls back to block mode: it cannot disturb PTY flow.
            let mode = match self.dispatched.get(request_id) {
                Some(false) => Mode::Stream,
                _ => Mode::Block,
            };
            self.begin_response(request_id, mode, state, now);
        }
        let active = self.active.as_mut().expect("active response just ensured");
        active.last_progress_at = now;
        active.delta_count += 1;
        if active.first_delta_at.is_none() {
            active.first_delta_at = Some(now);
            self.event_log.emit(Event::FirstDelta {
                request_id: active.request_id.clone(),
                dispatch_to_first_delta_ms: now
                    .saturating_duration_since(active.dispatched_at)
                    .as_millis() as u64,
                mode: match active.mode {
                    Mode::Stream => "stream",
                    Mode::Block => "block",
                },
                anchored: active.anchored,
            });
        }
        match active.mode {
            Mode::Stream if active.anchored => anchored_delta(active, delta, out, state),
            Mode::Stream => {
                // Reprint the header on the first delta, and again after a held block
                // was released mid-answer, so the resumed answer is relabeled.
                if !active.started || active.resume_header_pending {
                    active.started = true;
                    active.resume_header_pending = false;
                    let header = format!("{}{AI_HEADER}", line_prefix(state));
                    let _ = out.write_all(header.as_bytes());
                    state.record_presentation_output(header.as_bytes());
                }
                let bytes = normalize_newlines(delta, &mut active.last_was_cr);
                let _ = out.write_all(&bytes);
                let _ = out.flush();
                state.record_presentation_output(&bytes);
            }
            Mode::Block => active.accumulated.push_str(delta),
        }
        if let Some(reason) = active.degrade_reason
            && !active.degrade_emitted
        {
            active.degrade_emitted = true;
            self.event_log.emit(Event::DegradeToBlock {
                request_id: active.request_id.clone(),
                reason,
                ms_since_dispatch: now
                    .saturating_duration_since(active.dispatched_at)
                    .as_millis() as u64,
                deltas_so_far: active.delta_count,
            });
        }
    }

    fn finish<W: Write>(
        &mut self,
        request_id: &str,
        error: Option<&str>,
        out: &mut W,
        state: &mut SessionState,
        now: Instant,
    ) {
        // An aborted request's terminal marker is part of the suppressed tail;
        // dropping it here also completes the abort bookkeeping.
        if self.aborted.remove(request_id) {
            self.dispatched.remove(request_id);
            return;
        }
        self.dispatched.remove(request_id);
        let active = match self.active.take() {
            Some(active) if active.request_id == request_id => active,
            other => {
                self.active = other;
                if let Some(message) = error {
                    notice(&format!("AI error: {message}"), out, state);
                }
                return;
            }
        };
        let status = if error.is_some() { "error" } else { "ok" };
        self.event_log.emit(active.end_event(status, now));

        match active.mode {
            Mode::Stream if active.anchored => {
                // The screen is already in its final shape: AI text in the free
                // zone, the live input line last. Only a failure needs ink.
                if let Some(message) = error {
                    notice_above_live(&format!("AI error: {message}"), out, state);
                }
            }
            Mode::Stream => {
                if active.started {
                    let tail = b"\r\n";
                    let _ = out.write_all(tail);
                    state.record_presentation_output(tail);
                }
                if let Some(message) = error {
                    notice(&format!("AI error: {message}"), out, state);
                }
                if !active.buffered_pty.is_empty() {
                    let _ = out.write_all(&active.buffered_pty);
                    state.record_output(&active.buffered_pty, now);
                }
                let _ = out.flush();
            }
            Mode::Block => {
                if !active.accumulated.is_empty() {
                    let mut last_was_cr = false;
                    let mut block = AI_HEADER.as_bytes().to_vec();
                    block.extend(normalize_newlines(&active.accumulated, &mut last_was_cr));
                    block.extend_from_slice(b"\r\n");
                    // With a prompt-shaped live line resting under the cursor, the
                    // block is inserted above it instead of consuming it.
                    let inserted = state.resting_prompt() && insert_above_live(&block, out, state);
                    if !inserted {
                        let mut placed = line_prefix(state).as_bytes().to_vec();
                        placed.extend_from_slice(&block);
                        let _ = out.write_all(&placed);
                        let _ = out.flush();
                        state.record_presentation_output(&placed);
                    }
                }
                if let Some(message) = error {
                    notice_before_prompt(&format!("AI error: {message}"), out, state);
                }
            }
        }
    }
}

impl Default for Presentation {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mirror::TerminalMirror;
    use crate::shell_integration::{MarkerKind, ShellIntegrationMarker};

    fn state() -> SessionState {
        SessionState::new(80, 24, true)
    }

    /// Replays what the user's terminal received — the setup PTY bytes, then
    /// everything presentation wrote — and returns the resulting screen text plus
    /// the cursor column. The strongest assertion available: it checks the final
    /// picture, not the byte choreography.
    fn replay_screen(columns: u16, setup: &[u8], out: &[u8]) -> (String, u16) {
        let mut mirror = TerminalMirror::new(columns, 24);
        mirror.write(setup);
        mirror.write(out);
        let snapshot = mirror.snapshot();
        (snapshot.screen, snapshot.cursor_x)
    }

    fn delta(request_id: &str, text: &str) -> ServerMessage {
        ServerMessage::AiDelta {
            request_id: request_id.to_string(),
            delta: text.to_string(),
        }
    }

    fn tool_activity(request_id: &str, message: &str) -> ServerMessage {
        tool_activity_of(request_id, "web_search", "started", message)
    }

    fn tool_activity_of(
        request_id: &str,
        tool_name: &str,
        phase: &str,
        message: &str,
    ) -> ServerMessage {
        ServerMessage::AiToolActivity {
            request_id: request_id.to_string(),
            tool_name: tool_name.to_string(),
            phase: phase.to_string(),
            message: message.to_string(),
        }
    }

    fn end(request_id: &str) -> ServerMessage {
        ServerMessage::AiResponseEnd {
            request_id: request_id.to_string(),
        }
    }

    // Koshell's own chrome must stay safe for legacy/no-color terminals. A reprinted
    // prompt is exempt: it replays styling the program itself just rendered on this
    // terminal, so the terminal supports those sequences by construction.
    fn assert_no_rich_terminal_sequences(output: &[u8]) {
        let text = String::from_utf8_lossy(output);
        for sequence in [
            "\x1b[38;2",
            "\x1b[48;2",
            "\x1b[38:2",
            "\x1b[48:2",
            "\x1b[38;5",
            "\x1b[48;5",
            "\x1b[38:5",
            "\x1b[48:5",
            "\x1b]8;",
        ] {
            assert!(
                !text.contains(sequence),
                "presentation output must stay safe for legacy/no-color terminals; found \
                 unsupported sequence {sequence:?} in {text:?}"
            );
        }
    }

    #[test]
    fn stream_mode_buffers_pty_from_dispatch_until_response_end() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        // The returning prompt arrives before the first delta and must be held.
        presentation.note_dispatch("r1", false, &state, now);
        presentation.pty_output(b"$ ", &mut out, &mut state, now);
        assert!(out.is_empty(), "prompt must be held from dispatch time");

        presentation.handle_server_message(&delta("r1", "The answer."), &mut out, &mut state, now);
        let during = String::from_utf8_lossy(&out).to_string();
        assert!(during.contains("[koshell ai]"));
        assert!(during.contains("The answer."));
        assert!(
            !during.contains("$ "),
            "prompt must stay buffered while streaming"
        );

        presentation.handle_server_message(&end("r1"), &mut out, &mut state, now);
        let after = String::from_utf8_lossy(&out).to_string();
        assert!(
            after.ends_with("$ "),
            "buffered prompt flushes after the response"
        );

        // Subsequent PTY output writes through again.
        presentation.pty_output(b"ls\r\n", &mut out, &mut state, now);
        assert!(String::from_utf8_lossy(&out).ends_with("ls\r\n"));
    }

    #[test]
    fn block_mode_lets_pty_flow_and_inserts_one_block_at_end() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        presentation.note_dispatch("r1", true, &state, now);
        presentation.handle_server_message(&delta("r1", "Still "), &mut out, &mut state, now);
        presentation.pty_output(b"build output\r\n", &mut out, &mut state, now);
        let during = String::from_utf8_lossy(&out).to_string();
        assert!(during.contains("build output"));
        assert!(
            !during.contains("Still "),
            "deltas accumulate in block mode"
        );

        presentation.handle_server_message(&delta("r1", "compiling."), &mut out, &mut state, now);
        presentation.handle_server_message(&end("r1"), &mut out, &mut state, now);
        let after = String::from_utf8_lossy(&out).to_string();
        assert!(after.contains("[koshell ai]"));
        assert!(after.contains("Still compiling."));
    }

    #[test]
    fn newlines_are_normalized_for_raw_mode_across_deltas() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        presentation.note_dispatch("r1", false, &state, now);
        presentation.handle_server_message(&delta("r1", "line one\n"), &mut out, &mut state, now);
        presentation.handle_server_message(&delta("r1", "line two\r"), &mut out, &mut state, now);
        presentation.handle_server_message(&delta("r1", "\nline three"), &mut out, &mut state, now);
        presentation.handle_server_message(&end("r1"), &mut out, &mut state, now);

        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("line one\r\nline two\r\nline three"));
        assert!(
            !text.contains("\r\r\n"),
            "a split \\r\\n must not be doubled"
        );
    }

    #[test]
    fn pty_buffer_fuse_releases_a_block_and_keeps_buffering() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        presentation.note_dispatch("r1", false, &state, now);
        presentation.handle_server_message(&delta("r1", "answer"), &mut out, &mut state, now);

        // The held output reaches the fuse: it is released as one labeled block.
        let big = vec![b'x'; PTY_BUFFER_FUSE_BYTES];
        presentation.pty_output(&big, &mut out, &mut state, now);
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("held command output hit the size limit"));
        assert!(out.len() > PTY_BUFFER_FUSE_BYTES, "the block flushed");

        // Buffering stays on: output after the block is held again, not written through.
        let len_after_release = out.len();
        presentation.pty_output(b"more", &mut out, &mut state, now);
        assert_eq!(
            out.len(),
            len_after_release,
            "output after the released block is re-buffered, not interleaved"
        );

        // Response end flushes the re-buffered tail exactly once.
        presentation.handle_server_message(&end("r1"), &mut out, &mut state, now);
        assert!(String::from_utf8_lossy(&out).ends_with("more"));
    }

    #[test]
    fn fuse_flush_relabels_the_resumed_answer() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        presentation.note_dispatch("r1", false, &state, now);
        presentation.handle_server_message(&delta("r1", "before"), &mut out, &mut state, now);
        presentation.pty_output(
            &vec![b'x'; PTY_BUFFER_FUSE_BYTES],
            &mut out,
            &mut state,
            now,
        );
        // The answer resumes after the released block under a fresh header.
        presentation.handle_server_message(&delta("r1", "after"), &mut out, &mut state, now);

        let text = String::from_utf8_lossy(&out).to_string();
        let first_header = text.find("[koshell ai]").expect("initial header");
        let boundary = text.find("hit the size limit").expect("boundary notice");
        let second_header = text.rfind("[koshell ai]").expect("resumed header");
        let resumed = text.find("after").expect("resumed answer text");
        assert!(
            first_header < boundary && boundary < second_header && second_header < resumed,
            "answer, boundary block, then relabeled answer appear in order"
        );
    }

    #[test]
    fn waiting_notice_fires_once_when_nothing_rendered() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        presentation.note_dispatch("r1", false, &state, now);
        assert!(presentation.next_deadline(now).is_some());

        presentation.poll(now, &mut out, &mut state);
        assert!(out.is_empty(), "no notice before the delay");

        let later = now + RECEIPT_NOTICE_DELAY + Duration::from_millis(10);
        presentation.poll(later, &mut out, &mut state);
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("waiting for the AI answer"));

        let len = out.len();
        presentation.poll(later + Duration::from_secs(1), &mut out, &mut state);
        assert_eq!(out.len(), len, "the waiting notice prints only once");
    }

    #[test]
    fn no_waiting_notice_once_the_response_started() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        presentation.note_dispatch("r1", false, &state, now);
        presentation.handle_server_message(&delta("r1", "quick"), &mut out, &mut state, now);
        let len = out.len();
        let later = now + RECEIPT_NOTICE_DELAY + Duration::from_millis(10);
        presentation.poll(later, &mut out, &mut state);
        assert_eq!(out.len(), len, "streaming output is its own receipt");
    }

    #[test]
    fn streamed_delta_resets_the_stall_deadline() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        presentation.note_dispatch("r1", false, &state, now);
        presentation.pty_output(b"$ ", &mut out, &mut state, now);
        let progress = now + Duration::from_secs(20);
        presentation.handle_server_message(
            &delta("r1", "A long answer is still streaming."),
            &mut out,
            &mut state,
            progress,
        );

        // Crossing the original dispatch-based deadline is harmless because the
        // delta moved the inactivity deadline forward.
        presentation.poll(
            now + STALL_NOTICE_DELAY + Duration::from_millis(10),
            &mut out,
            &mut state,
        );
        assert!(
            !String::from_utf8_lossy(&out).contains("no AI progress"),
            "healthy streaming must not be labeled as stalled"
        );
        assert!(
            presentation
                .next_deadline(now + STALL_NOTICE_DELAY)
                .is_some(),
            "the progress-based deadline remains scheduled"
        );

        // The same response is genuinely stalled after 30 seconds without another
        // delta, so the held prompt and Ctrl+C recovery are reported then.
        presentation.poll(
            progress + STALL_NOTICE_DELAY + Duration::from_millis(10),
            &mut out,
            &mut state,
        );
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("no AI progress for 30 seconds"));
        assert!(text.contains("press Ctrl+C"));
        assert!(!text.contains("$ "), "the held prompt remains buffered");
    }

    #[test]
    fn visible_tool_activity_resets_the_stall_deadline() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        presentation.note_dispatch("r1", false, &state, now);
        presentation.pty_output(b"$ ", &mut out, &mut state, now);
        let progress = now + Duration::from_secs(20);
        presentation.handle_server_message(
            &tool_activity("r1", "searching the web"),
            &mut out,
            &mut state,
            progress,
        );

        presentation.poll(
            now + STALL_NOTICE_DELAY + Duration::from_millis(10),
            &mut out,
            &mut state,
        );
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("searching the web"));
        assert!(
            !text.contains("no AI progress"),
            "visible tool work is meaningful response progress"
        );

        presentation.poll(
            progress + STALL_NOTICE_DELAY + Duration::from_millis(10),
            &mut out,
            &mut state,
        );
        assert!(String::from_utf8_lossy(&out).contains("no AI progress for 30 seconds"));
    }

    #[test]
    fn max_hold_holds_and_prompts_ctrl_c() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        presentation.note_dispatch("r1", false, &state, now);
        presentation.pty_output(b"$ ", &mut out, &mut state, now);
        assert!(out.is_empty());

        // At the stall deadline the held output is reported, not force-flushed.
        let stalled = now + STALL_NOTICE_DELAY + Duration::from_millis(10);
        presentation.poll(stalled, &mut out, &mut state);
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(
            text.contains("no AI progress for 30 seconds") && text.contains("press Ctrl+C"),
            "the stall notice reports inactivity and offers Ctrl+C: {text:?}"
        );
        assert!(
            !text.contains("$ "),
            "the held prompt is not flushed on stall: {text:?}"
        );
        assert!(
            presentation.next_deadline(stalled).is_none(),
            "the hold no longer runs on a clock after the stall notice"
        );

        // Typed-ahead output stays held too — still no interleaving.
        presentation.pty_output(b"typed", &mut out, &mut state, stalled);
        assert!(
            !String::from_utf8_lossy(&out).contains("typed"),
            "output after the stall stays buffered"
        );

        // Ctrl+C is what releases the held output, behind a boundary notice.
        let aborted = presentation.user_interrupt(&mut out, &mut state, stalled);
        assert_eq!(aborted.as_deref(), Some("r1"));
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("releasing held command output"));
        assert!(
            text.ends_with("$ typed"),
            "the held output flushes on interrupt: {text:?}"
        );
    }

    // Tool work used to be invisible: web_search runs entirely inside the daemon, and
    // the command readers were intercepted before presentation. The user could not
    // tell a slow lookup from a hung daemon, which is exactly the moment they need to
    // decide whether to press Ctrl+C.
    #[test]
    fn tool_activity_is_shown_before_the_answer_arrives() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        presentation.note_dispatch("r1", false, &state, now);
        presentation.handle_server_message(
            &tool_activity("r1", "searching the web (exa): homebrew shallow clone"),
            &mut out,
            &mut state,
            now,
        );

        let text = String::from_utf8_lossy(&out).to_string();
        assert!(
            text.contains("searching the web (exa): homebrew shallow clone"),
            "the tool line names what the AI is doing: {text:?}"
        );
        assert!(
            text.contains("[koshell]"),
            "the tool line is self-identifying: {text:?}"
        );

        // Showing the work is the receipt; the waiting notice must not also fire.
        let waited = now + RECEIPT_NOTICE_DELAY + Duration::from_millis(10);
        let before = String::from_utf8_lossy(&out).to_string();
        presentation.poll(waited, &mut out, &mut state);
        assert_eq!(
            String::from_utf8_lossy(&out).to_string(),
            before,
            "the tool line already told the user the answer is pending"
        );
    }

    // Design 0010's rule applies to this seam like any other: the answer and
    // everything else stay in separate labeled blocks.
    #[test]
    fn a_mid_answer_tool_line_relabels_the_resumed_answer() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        presentation.note_dispatch("r1", false, &state, now);
        presentation.handle_server_message(&delta("r1", "Checking"), &mut out, &mut state, now);
        presentation.handle_server_message(
            &tool_activity("r1", "looking up your recent commands"),
            &mut out,
            &mut state,
            now,
        );
        presentation.handle_server_message(&delta("r1", "Found it."), &mut out, &mut state, now);

        let text = String::from_utf8_lossy(&out).to_string();
        let header_count = text.matches("[koshell ai]").count();
        assert_eq!(
            header_count, 2,
            "the answer resumes under a fresh header after the tool line: {text:?}"
        );
        let tool_at = text
            .find("looking up your recent commands")
            .expect("tool line");
        let resumed_at = text.find("Found it.").expect("resumed answer");
        assert!(
            tool_at < resumed_at,
            "the tool line precedes the resumption"
        );
    }

    // On the anchored path the row above the live region is the AI tail, and the
    // tail check degrades the response when something else takes that row. Dropping
    // the resume point instead re-anchors cleanly, so using a tool does not cost the
    // rest of the answer its streaming.
    #[test]
    fn an_anchored_tool_line_re_anchors_instead_of_degrading() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        let setup = b"2\r\n>>> ";
        state.record_output(setup, now);
        presentation.note_dispatch("r1", false, &state, now);
        presentation.handle_server_message(&delta("r1", "Looking"), &mut out, &mut state, now);
        presentation.handle_server_message(
            &tool_activity("r1", "searching the web (exa): brew error"),
            &mut out,
            &mut state,
            now,
        );
        presentation.handle_server_message(&delta("r1", "Answer."), &mut out, &mut state, now);
        presentation.handle_server_message(&end("r1"), &mut out, &mut state, now);

        let (screen, _) = replay_screen(80, setup, &out);
        assert!(
            screen.contains("searching the web (exa): brew error"),
            "the tool line is on screen: {screen:?}"
        );
        assert!(
            screen.contains("Answer."),
            "the answer continued after the tool line: {screen:?}"
        );
        // The live prompt is still the last line: the answer stayed anchored above it
        // rather than degrading into one block appended at the end.
        assert!(
            screen.trim_end().ends_with(">>>"),
            "the live prompt remains the last line: {screen:?}"
        );
    }

    #[test]
    fn tool_activity_for_an_interrupted_request_renders_nothing() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        presentation.note_dispatch("r1", false, &state, now);
        presentation.handle_server_message(&delta("r1", "Working"), &mut out, &mut state, now);
        presentation.user_interrupt(&mut out, &mut state, now);
        let before = String::from_utf8_lossy(&out).to_string();

        presentation.handle_server_message(
            &tool_activity("r1", "searching the web (exa): late"),
            &mut out,
            &mut state,
            now,
        );
        assert_eq!(
            String::from_utf8_lossy(&out).to_string(),
            before,
            "a withdrawn request renders no further tool lines"
        );
    }

    #[test]
    fn tool_activity_for_an_unknown_request_renders_nothing() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        presentation.note_dispatch("r1", false, &state, now);
        presentation.handle_server_message(
            &tool_activity("r2", "stale activity"),
            &mut out,
            &mut state,
            now,
        );
        assert!(out.is_empty(), "a stale request id renders nothing");
    }

    #[test]
    fn a_stalled_answer_holding_nothing_stays_silent() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        presentation.note_dispatch("r1", false, &state, now);

        // The one-second waiting notice still fires: the answer is pending.
        let waited = now + RECEIPT_NOTICE_DELAY + Duration::from_millis(10);
        presentation.poll(waited, &mut out, &mut state);
        let waiting = String::from_utf8_lossy(&out).to_string();
        assert!(waiting.contains("waiting for the AI answer"));

        // At the stall deadline nothing is held, so there is nothing to report.
        let stalled = now + STALL_NOTICE_DELAY + Duration::from_millis(10);
        presentation.poll(stalled, &mut out, &mut state);
        assert_eq!(
            String::from_utf8_lossy(&out).to_string(),
            waiting,
            "a slow answer that holds no command output prints no stall notice"
        );
        assert!(
            presentation.next_deadline(stalled).is_none(),
            "the passed deadline is not rescheduled onto a zero-length wait"
        );

        // Output arriving after the deadline is the first thing actually held, and
        // held output is never silent (design 0010).
        presentation.pty_output(b"$ ", &mut out, &mut state, stalled);
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(
            text.contains("press Ctrl+C"),
            "late held output still reports itself: {text:?}"
        );
        assert!(
            !text.contains("$ "),
            "reporting the hold does not flush it: {text:?}"
        );

        // And it fires at most once.
        let before = String::from_utf8_lossy(&out).to_string();
        presentation.pty_output(b"more", &mut out, &mut state, stalled);
        assert_eq!(String::from_utf8_lossy(&out).to_string(), before);
    }

    #[test]
    fn error_mid_stream_flushes_buffer_and_prints_notice() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        presentation.note_dispatch("r1", false, &state, now);
        presentation.handle_server_message(&delta("r1", "partial"), &mut out, &mut state, now);
        presentation.pty_output(b"$ ", &mut out, &mut state, now);
        presentation.handle_server_message(
            &ServerMessage::AiError {
                request_id: "r1".to_string(),
                message: "provider exploded".to_string(),
            },
            &mut out,
            &mut state,
            now,
        );

        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("partial"));
        assert!(text.contains("AI error: provider exploded"));
        assert!(
            text.ends_with("$ "),
            "buffered prompt still flushes after an error"
        );
    }

    #[test]
    fn error_before_any_delta_flushes_held_output() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        presentation.note_dispatch("r1", false, &state, now);
        presentation.pty_output(b"$ ", &mut out, &mut state, now);
        presentation.handle_server_message(
            &ServerMessage::AiError {
                request_id: "r1".to_string(),
                message: "no provider configured".to_string(),
            },
            &mut out,
            &mut state,
            now,
        );
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("AI error: no provider configured"));
        assert!(text.ends_with("$ "), "held prompt flushes after the error");
    }

    #[test]
    fn koshell_presentation_output_avoids_rich_color_sequences() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        presentation.note_dispatch("stream", false, &state, now);
        presentation.handle_server_message(
            &delta("stream", "streamed answer"),
            &mut out,
            &mut state,
            now,
        );
        presentation.handle_server_message(
            &ServerMessage::AiError {
                request_id: "stream".to_string(),
                message: "provider failed".to_string(),
            },
            &mut out,
            &mut state,
            now,
        );

        presentation.note_dispatch("wait", false, &state, now);
        presentation.poll(
            now + RECEIPT_NOTICE_DELAY + Duration::from_millis(10),
            &mut out,
            &mut state,
        );
        presentation.poll(
            now + STALL_NOTICE_DELAY + Duration::from_millis(10),
            &mut out,
            &mut state,
        );
        presentation.handle_server_message(&end("wait"), &mut out, &mut state, now);

        presentation.note_dispatch("block", true, &state, now);
        presentation.handle_server_message(
            &delta("block", "block answer"),
            &mut out,
            &mut state,
            now,
        );
        presentation.handle_server_message(&end("block"), &mut out, &mut state, now);

        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("[koshell ai]"));
        assert!(text.contains("streamed answer"));
        assert!(text.contains("waiting for the AI answer"));
        assert!(text.contains("block answer"));
        assert_no_rich_terminal_sequences(&out);
    }

    #[test]
    fn presentation_lines_skip_the_leading_blank_at_line_start() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        // The Enter echo left the cursor at the start of an empty line (the returning
        // prompt is buffered), so the notice starts right there — no blank line.
        state.record_output(b"% #? question\r\n", now);
        presentation.note_dispatch("r1", false, &state, now);
        presentation.poll(
            now + RECEIPT_NOTICE_DELAY + Duration::from_millis(10),
            &mut out,
            &mut state,
        );
        assert!(
            out.starts_with(b"\x1b[2m[koshell] waiting"),
            "no leading blank line at line start: {:?}",
            String::from_utf8_lossy(&out)
        );

        // The header follows the notice directly, again without a blank line.
        presentation.handle_server_message(&delta("r1", "answer"), &mut out, &mut state, now);
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(!text.contains("\r\n\r\n"), "no blank lines: {text:?}");
    }

    #[test]
    fn multi_line_notice_does_not_staircase_in_raw_mode() {
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();

        // A body with embedded newlines, like the missing-config error block whose
        // lone `\n`s used to drop a row without returning to column 0.
        notice(
            "AI error: no config, e.g.:\nmodel = \"anthropic/claude-sonnet-4-5\"\n    api_key = \"sk-ant-...\"",
            &mut out,
            &mut state,
        );

        // Every embedded newline is CR-normalized: no lone `\n` survives.
        for i in 0..out.len() {
            if out[i] == b'\n' {
                assert!(
                    i > 0 && out[i - 1] == b'\r',
                    "lone \\n at {i}: {:?}",
                    String::from_utf8_lossy(&out)
                );
            }
        }

        // The replayed screen shows each line flush at column 0, no rightward drift.
        let (screen, _) = replay_screen(80, b"", &out);
        let lines: Vec<&str> = screen.lines().collect();
        assert_eq!(lines[0], "[koshell] AI error: no config, e.g.:");
        assert_eq!(lines[1], "model = \"anthropic/claude-sonnet-4-5\"");
        assert_eq!(lines[2], "    api_key = \"sk-ant-...\"");
    }

    #[test]
    fn presentation_lines_keep_the_newline_when_mid_line() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        // The cursor rests mid-line on non-prompt output; the notice must move to a
        // fresh line first.
        state.record_output(b"downloading", now);
        presentation.note_dispatch("r1", false, &state, now);
        presentation.poll(
            now + RECEIPT_NOTICE_DELAY + Duration::from_millis(10),
            &mut out,
            &mut state,
        );
        assert!(
            out.starts_with(b"\r\n\x1b[2m[koshell] waiting"),
            "mid-line output needs the leading newline: {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    #[test]
    fn anchored_stream_inserts_deltas_above_the_live_prompt() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        // The REPL case: stabilization fired only after the prompt rendered.
        let setup = b"2\r\n>>> ";
        state.record_output(setup, now);
        presentation.note_dispatch("r1", false, &state, now);
        assert!(
            out.is_empty(),
            "dispatch writes nothing; the prompt stays live"
        );

        presentation.handle_server_message(
            &delta("r1", "The answer is"),
            &mut out,
            &mut state,
            now,
        );
        let (mid, col) = replay_screen(80, setup, &out);
        assert_eq!(mid, "2\n[koshell ai]\nThe answer is\n>>>");
        assert_eq!(col, 4, "cursor parked at the live prompt column");

        presentation.handle_server_message(
            &delta("r1", " two.\nSecond"),
            &mut out,
            &mut state,
            now,
        );
        presentation.handle_server_message(&end("r1"), &mut out, &mut state, now);
        let (screen, col) = replay_screen(80, setup, &out);
        assert_eq!(screen, "2\n[koshell ai]\nThe answer is two.\nSecond\n>>>");
        assert_eq!(col, 4);
    }

    #[test]
    fn anchored_stream_keeps_typed_input_live_across_redraws() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        let setup = b">>> ";
        state.record_output(setup, now);
        presentation.note_dispatch("r1", false, &state, now);
        presentation.handle_server_message(&delta("r1", "Thinking"), &mut out, &mut state, now);

        // The user types mid-stream: the echo shows immediately (never buffered).
        presentation.pty_output(b"1+1", &mut out, &mut state, now);
        let (mid, col) = replay_screen(80, setup, &out);
        assert_eq!(mid, "[koshell ai]\nThinking\n>>> 1+1");
        assert_eq!(col, 7);

        // The next delta redraw preserves the typed input and cursor column.
        presentation.handle_server_message(&delta("r1", " done"), &mut out, &mut state, now);
        presentation.handle_server_message(&end("r1"), &mut out, &mut state, now);
        let (screen, col) = replay_screen(80, setup, &out);
        assert_eq!(screen, "[koshell ai]\nThinking done\n>>> 1+1");
        assert_eq!(col, 7);
    }

    #[test]
    fn anchored_stream_resumes_an_exactly_full_ai_line_without_loss() {
        let mut presentation = Presentation::new();
        let mut state = SessionState::new(20, 24, true);
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        let setup = b">>> ";
        state.record_output(setup, now);
        presentation.note_dispatch("r1", false, &state, now);
        // Exactly the terminal width: the AI line ends in the pending-wrap state.
        presentation.handle_server_message(
            &delta("r1", &"a".repeat(20)),
            &mut out,
            &mut state,
            now,
        );
        presentation.handle_server_message(&delta("r1", "bbb"), &mut out, &mut state, now);
        presentation.handle_server_message(&end("r1"), &mut out, &mut state, now);

        let (screen, _) = replay_screen(20, setup, &out);
        assert_eq!(
            screen,
            format!("[koshell ai]\n{}\nbbb\n>>>", "a".repeat(20)),
            "the wrap continuation resumes without losing or shifting characters"
        );
    }

    #[test]
    fn anchored_stream_restores_a_soft_wrapped_live_line() {
        let mut presentation = Presentation::new();
        let mut state = SessionState::new(20, 24, true);
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        let setup = b">>> ";
        state.record_output(setup, now);
        presentation.note_dispatch("r1", false, &state, now);
        presentation.handle_server_message(&delta("r1", "Answer"), &mut out, &mut state, now);

        // Typed input long enough to wrap the live line onto a second row.
        presentation.pty_output(
            &[b"x".repeat(20).as_slice()].concat(),
            &mut out,
            &mut state,
            now,
        );
        presentation.handle_server_message(&delta("r1", " more"), &mut out, &mut state, now);
        presentation.handle_server_message(&end("r1"), &mut out, &mut state, now);

        let (screen, col) = replay_screen(20, setup, &out);
        assert_eq!(
            screen,
            format!(
                "[koshell ai]\nAnswer more\n>>> {}\n{}",
                "x".repeat(16),
                "x".repeat(4)
            ),
            "both rows of the wrapped live line survive the redraw"
        );
        assert_eq!(col, 4);
    }

    #[test]
    fn intervening_program_output_degrades_to_one_block() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        let setup = b">>> ";
        state.record_output(setup, now);
        presentation.note_dispatch("r1", false, &state, now);
        presentation.handle_server_message(&delta("r1", "Part one."), &mut out, &mut state, now);

        // A mid-stream command executed: its output breaks the free-zone adjacency.
        presentation.pty_output(b"\r\n2\r\n>>> ", &mut out, &mut state, now);
        presentation.handle_server_message(&delta("r1", "Part two."), &mut out, &mut state, now);
        presentation.handle_server_message(&end("r1"), &mut out, &mut state, now);

        let (screen, col) = replay_screen(80, setup, &out);
        assert_eq!(
            screen, "[koshell ai]\nPart one.\n>>>\n2\n[koshell ai]\nPart two.\n>>>",
            "the remainder arrives as one block above the new prompt, never interleaved"
        );
        assert_eq!(col, 4);
    }

    #[test]
    fn anchored_error_keeps_the_live_line_last() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        let setup = b">>> ";
        state.record_output(setup, now);
        presentation.note_dispatch("r1", false, &state, now);
        // Typed-ahead input echoes live while the response is in flight.
        presentation.pty_output(b"x", &mut out, &mut state, now);
        presentation.handle_server_message(
            &ServerMessage::AiError {
                request_id: "r1".to_string(),
                message: "provider exploded".to_string(),
            },
            &mut out,
            &mut state,
            now,
        );
        let (screen, col) = replay_screen(80, setup, &out);
        assert_eq!(screen, "[koshell] AI error: provider exploded\n>>> x");
        assert_eq!(col, 5);
    }

    #[test]
    fn anchored_waiting_notice_inserts_above_the_live_prompt() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        let setup = b">>> ";
        state.record_output(setup, now);
        presentation.note_dispatch("r1", false, &state, now);
        presentation.poll(
            now + RECEIPT_NOTICE_DELAY + Duration::from_millis(10),
            &mut out,
            &mut state,
        );
        let (screen, col) = replay_screen(80, setup, &out);
        assert_eq!(screen, "[koshell] waiting for the AI answer…\n>>>");
        assert_eq!(col, 4, "the prompt stays the last line, cursor on it");
    }

    #[test]
    fn interjected_question_mid_stream_is_captured_and_queued() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        // Inside a REPL command span, as in real use.
        state.handle_marker(
            ShellIntegrationMarker {
                kind: MarkerKind::CommandStart,
                command: Some("python3".to_string()),
                exit_code: None,
                cwd: None,
                executed: true,
            },
            now,
        );
        state.record_output(b">>> ", now);
        presentation.note_dispatch("r1", false, &state, now);
        presentation.handle_server_message(&delta("r1", "Thinking"), &mut out, &mut state, now);

        // The user interjects: the echo reaches the mirror live, so the ordinary
        // Enter capture sees the line and queues the question.
        presentation.pty_output(b"1+1 #? why", &mut out, &mut state, now);
        state.record_input(b"\r", now);
        assert!(state.has_pending(), "the interjection is pending");

        let actions = state.poll(now + Duration::from_millis(600));
        let questions: Vec<&str> = actions
            .iter()
            .filter_map(|action| match action {
                crate::trigger::Action::Fire(trigger) => Some(trigger.question.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(questions, vec!["why"], "queued question fires normally");
    }

    #[test]
    fn notice_before_prompt_keeps_the_prompt_as_the_last_line() {
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        state.record_output(b">>> ", now);
        notice_before_prompt("#? cancelled: why", &mut out, &mut state);
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(
            text.starts_with("\r\x1b[K"),
            "prompt line erased first: {text:?}"
        );
        assert!(text.contains("[koshell] #? cancelled: why"));
        assert!(text.ends_with(">>> "), "prompt reprinted below: {text:?}");
    }

    #[test]
    fn unknown_request_id_falls_back_to_block_mode() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        presentation.handle_server_message(&delta("stale", "ghost"), &mut out, &mut state, now);
        presentation.pty_output(b"live output", &mut out, &mut state, now);
        assert!(
            String::from_utf8_lossy(&out).contains("live output"),
            "unknown responses must not buffer PTY output"
        );
    }

    #[test]
    fn interrupt_stops_an_anchored_stream_and_drops_the_late_tail() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        let setup = b">>> ";
        state.record_output(setup, now);
        presentation.note_dispatch("r1", false, &state, now);
        presentation.handle_server_message(&delta("r1", "The answer"), &mut out, &mut state, now);
        assert!(presentation.owns_interrupt(), "stream in flight claims ^C");

        let aborted = presentation.user_interrupt(&mut out, &mut state, now);
        assert_eq!(aborted.as_deref(), Some("r1"));
        assert!(!presentation.owns_interrupt(), "window disarms after abort");

        // Everything the daemon still sends for r1 is dropped silently: the
        // local stop is authoritative, the daemon cancel only best-effort.
        let len = out.len();
        presentation.handle_server_message(&delta("r1", " is 42."), &mut out, &mut state, now);
        presentation.handle_server_message(&end("r1"), &mut out, &mut state, now);
        assert_eq!(out.len(), len, "late deltas and the end marker are silent");

        let (screen, col) = replay_screen(80, setup, &out);
        assert_eq!(
            screen,
            "[koshell ai]\nThe answer\n[koshell] answer interrupted (^C)\n>>>"
        );
        assert_eq!(col, 4, "the live prompt stays last and usable");
        assert_no_rich_terminal_sequences(&out);
    }

    #[test]
    fn interrupt_before_the_first_delta_leaves_the_prompt_live() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        let setup = b">>> ";
        state.record_output(setup, now);
        presentation.note_dispatch("r1", false, &state, now);
        assert!(presentation.owns_interrupt());

        let aborted = presentation.user_interrupt(&mut out, &mut state, now);
        assert_eq!(aborted.as_deref(), Some("r1"));

        let (screen, col) = replay_screen(80, setup, &out);
        assert_eq!(screen, "[koshell] answer interrupted (^C)\n>>>");
        assert_eq!(col, 4);
    }

    #[test]
    fn interrupt_flushes_the_held_prompt_in_buffered_stream_mode() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        // Shell-integrated path: the returning prompt is buffered at dispatch.
        presentation.note_dispatch("r1", false, &state, now);
        presentation.pty_output(b"$ ", &mut out, &mut state, now);
        presentation.handle_server_message(&delta("r1", "Partial"), &mut out, &mut state, now);
        assert!(presentation.owns_interrupt());

        let aborted = presentation.user_interrupt(&mut out, &mut state, now);
        assert_eq!(aborted.as_deref(), Some("r1"));
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("answer interrupted (^C)"));
        assert!(
            text.contains("releasing held command output"),
            "the interrupt notice labels the released output: {text:?}"
        );
        assert!(
            text.ends_with("$ "),
            "the held prompt flushes on interrupt: {text:?}"
        );
    }

    #[test]
    fn interrupt_withdraws_a_block_mode_answer() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        presentation.note_dispatch("r1", true, &state, now);
        presentation.handle_server_message(&delta("r1", "Never shown"), &mut out, &mut state, now);
        assert!(
            !presentation.owns_interrupt(),
            "block mode must never claim ^C from the running program"
        );

        let aborted = presentation.user_interrupt(&mut out, &mut state, now);
        assert_eq!(aborted.as_deref(), Some("r1"));
        presentation.handle_server_message(&delta("r1", " either"), &mut out, &mut state, now);
        presentation.handle_server_message(&end("r1"), &mut out, &mut state, now);

        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("answer cancelled (^C)"));
        assert!(
            !text.contains("Never shown"),
            "the withdrawn block must not be inserted at response end: {text:?}"
        );
    }

    #[test]
    fn interrupt_with_nothing_active_is_a_no_op() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        assert!(!presentation.owns_interrupt());
        assert_eq!(presentation.user_interrupt(&mut out, &mut state, now), None);
        assert!(out.is_empty());
    }

    #[test]
    fn a_new_question_streams_normally_after_an_interrupt() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        let setup = b">>> ";
        state.record_output(setup, now);
        presentation.note_dispatch("r1", false, &state, now);
        presentation.handle_server_message(&delta("r1", "Old"), &mut out, &mut state, now);
        presentation.user_interrupt(&mut out, &mut state, now);

        // The daemon (cancelled or not) eventually terminates r1, then serves r2.
        presentation.handle_server_message(&end("r1"), &mut out, &mut state, now);
        presentation.note_dispatch("r2", false, &state, now);
        presentation.handle_server_message(&delta("r2", "Fresh answer"), &mut out, &mut state, now);
        presentation.handle_server_message(&end("r2"), &mut out, &mut state, now);

        let (screen, _) = replay_screen(80, setup, &out);
        assert!(
            screen.contains("Fresh answer"),
            "the next response must render normally: {screen}"
        );
    }

    /// Drains every event line captured so far and parses each as JSON.
    fn drain_events(rx: &std::sync::mpsc::Receiver<String>) -> Vec<serde_json::Value> {
        rx.try_iter()
            .map(|line| serde_json::from_str(&line).expect("event lines are valid JSON"))
            .collect()
    }

    #[test]
    fn anchored_flow_emits_first_delta_and_response_end() {
        let (log, rx) = EventLog::capture();
        let mut presentation = Presentation::new();
        presentation.set_event_log(log);
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        state.record_output(b">>> ", now);
        presentation.note_dispatch("r1", false, &state, now);
        let first = now + Duration::from_millis(300);
        presentation.handle_server_message(&delta("r1", "One"), &mut out, &mut state, first);
        presentation.handle_server_message(&delta("r1", " two"), &mut out, &mut state, first);
        let done = now + Duration::from_millis(1200);
        presentation.handle_server_message(&end("r1"), &mut out, &mut state, done);

        let events = drain_events(&rx);
        assert_eq!(events.len(), 2, "first_delta then response_end: {events:?}");
        assert_eq!(events[0]["event"], "first_delta");
        assert_eq!(events[0]["request_id"], "r1");
        assert_eq!(events[0]["dispatch_to_first_delta_ms"], 300);
        assert_eq!(events[0]["mode"], "stream");
        assert_eq!(events[0]["anchored"], true);
        assert_eq!(events[1]["event"], "response_end");
        assert_eq!(events[1]["status"], "ok");
        assert_eq!(events[1]["total_ms"], 1200);
        assert_eq!(events[1]["first_delta_ms"], 300);
        assert_eq!(events[1]["delta_count"], 2);
        assert_eq!(events[1]["began_anchored"], true);
        assert_eq!(events[1]["degraded_to_block"], false);
        assert_eq!(events[1]["mid_stream_input_chunks"], 0);
    }

    #[test]
    fn degrade_emits_its_reason_and_marks_the_response_end() {
        let (log, rx) = EventLog::capture();
        let mut presentation = Presentation::new();
        presentation.set_event_log(log);
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        state.record_output(b">>> ", now);
        presentation.note_dispatch("r1", false, &state, now);
        presentation.handle_server_message(&delta("r1", "Part one."), &mut out, &mut state, now);
        // A mid-stream command's output breaks the free-zone adjacency.
        presentation.pty_output(b"\r\n2\r\n>>> ", &mut out, &mut state, now);
        presentation.handle_server_message(&delta("r1", "Part two."), &mut out, &mut state, now);
        presentation.handle_server_message(&end("r1"), &mut out, &mut state, now);

        let events = drain_events(&rx);
        let kinds: Vec<&str> = events
            .iter()
            .map(|event| event["event"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["first_delta", "degrade_to_block", "response_end"]);
        assert_eq!(events[1]["reason"], "tail_mismatch");
        assert_eq!(events[1]["deltas_so_far"], 2);
        assert_eq!(events[2]["began_anchored"], true);
        assert_eq!(events[2]["degraded_to_block"], true);
    }

    #[test]
    fn interrupt_emits_response_end_and_the_late_finish_emits_nothing() {
        let (log, rx) = EventLog::capture();
        let mut presentation = Presentation::new();
        presentation.set_event_log(log);
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        state.record_output(b">>> ", now);
        presentation.note_dispatch("r1", false, &state, now);
        presentation.handle_server_message(&delta("r1", "Old"), &mut out, &mut state, now);
        presentation.user_interrupt(&mut out, &mut state, now);
        presentation.handle_server_message(&delta("r1", "late"), &mut out, &mut state, now);
        presentation.handle_server_message(&end("r1"), &mut out, &mut state, now);

        let events = drain_events(&rx);
        let kinds: Vec<&str> = events
            .iter()
            .map(|event| event["event"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            ["first_delta", "response_end"],
            "the aborted tail must not emit anything: {events:?}"
        );
        assert_eq!(events[1]["status"], "interrupted");
    }

    #[test]
    fn mid_stream_input_chunks_land_on_response_end() {
        let (log, rx) = EventLog::capture();
        let mut presentation = Presentation::new();
        presentation.set_event_log(log);
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        state.record_output(b">>> ", now);
        presentation.note_dispatch("r1", false, &state, now);
        presentation.handle_server_message(&delta("r1", "Thinking"), &mut out, &mut state, now);
        presentation.note_mid_stream_input();
        presentation.note_mid_stream_input();
        presentation.handle_server_message(&end("r1"), &mut out, &mut state, now);

        let events = drain_events(&rx);
        assert_eq!(events.last().unwrap()["event"], "response_end");
        assert_eq!(events.last().unwrap()["mid_stream_input_chunks"], 2);
    }

    #[test]
    fn block_mode_never_counts_mid_stream_input() {
        let (log, rx) = EventLog::capture();
        let mut presentation = Presentation::new();
        presentation.set_event_log(log);
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        presentation.note_dispatch("r1", true, &state, now);
        presentation.handle_server_message(&delta("r1", "block"), &mut out, &mut state, now);
        // Typing while a command runs is ordinary shell use, not the metric.
        presentation.note_mid_stream_input();
        presentation.handle_server_message(&end("r1"), &mut out, &mut state, now);

        let events = drain_events(&rx);
        assert_eq!(events.last().unwrap()["event"], "response_end");
        assert_eq!(events.last().unwrap()["mid_stream_input_chunks"], 0);
    }

    // "How often does the agent pull, and does the user interrupt when it does"
    // is only answerable if every announcement is recorded — including the ones
    // for tools the daemon serves itself, which reach the terminal no other way.
    #[test]
    fn tool_activity_is_logged_and_counted_on_the_response_end() {
        let (log, rx) = EventLog::capture();
        let mut presentation = Presentation::new();
        presentation.set_event_log(log);
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        presentation.note_dispatch("r1", false, &state, now);
        presentation.handle_server_message(
            &tool_activity_of(
                "r1",
                "web_search",
                "started",
                "searching the web (exa): brew",
            ),
            &mut out,
            &mut state,
            now,
        );
        presentation.handle_server_message(
            &tool_activity_of("r1", "web_search", "failed", "web search failed (timeout)"),
            &mut out,
            &mut state,
            now,
        );
        presentation.handle_server_message(
            &tool_activity_of(
                "r1",
                "list_recent_commands",
                "started",
                "looking up your recent commands",
            ),
            &mut out,
            &mut state,
            now,
        );
        presentation.handle_server_message(&delta("r1", "Answer."), &mut out, &mut state, now);
        presentation.handle_server_message(&end("r1"), &mut out, &mut state, now);

        let events = drain_events(&rx);
        let activity: Vec<&serde_json::Value> = events
            .iter()
            .filter(|event| event["event"] == "tool_activity")
            .collect();
        assert_eq!(
            activity.len(),
            3,
            "every announcement is logged: {events:?}"
        );
        assert_eq!(activity[0]["tool_name"], "web_search");
        assert_eq!(activity[0]["phase"], "started");
        assert_eq!(activity[0]["rendered"], true);
        assert_eq!(activity[1]["phase"], "failed");
        assert_eq!(activity[2]["tool_name"], "list_recent_commands");

        let end_event = events.last().unwrap();
        assert_eq!(end_event["event"], "response_end");
        // Failures are counted apart from calls, so a retried search is not read
        // as two separate pulls.
        assert_eq!(end_event["tool_calls"], 2);
        assert_eq!(end_event["tool_failures"], 1);
    }

    #[test]
    fn a_response_without_tools_reports_zero() {
        let (log, rx) = EventLog::capture();
        let mut presentation = Presentation::new();
        presentation.set_event_log(log);
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        presentation.note_dispatch("r1", false, &state, now);
        presentation.handle_server_message(
            &delta("r1", "Straight answer."),
            &mut out,
            &mut state,
            now,
        );
        presentation.handle_server_message(&end("r1"), &mut out, &mut state, now);

        let end_event = drain_events(&rx).pop().expect("response_end");
        assert_eq!(end_event["tool_calls"], 0);
        assert_eq!(end_event["tool_failures"], 0);
    }

    // The interrupted-mid-call case is the whole point of the metric, so it must be
    // recorded even though nothing was drawn for it.
    #[test]
    fn a_tool_call_the_user_never_saw_is_still_logged() {
        let (log, rx) = EventLog::capture();
        let mut presentation = Presentation::new();
        presentation.set_event_log(log);
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        presentation.note_dispatch("r1", false, &state, now);
        presentation.handle_server_message(&delta("r1", "Working"), &mut out, &mut state, now);
        presentation.user_interrupt(&mut out, &mut state, now);
        let after_interrupt = out.len();
        presentation.handle_server_message(
            &tool_activity("r1", "searching the web (exa): late"),
            &mut out,
            &mut state,
            now,
        );

        assert_eq!(
            out.len(),
            after_interrupt,
            "a withdrawn request draws nothing"
        );
        let logged = drain_events(&rx)
            .pop()
            .expect("the activity is still recorded");
        assert_eq!(logged["event"], "tool_activity");
        assert_eq!(logged["rendered"], false);
    }

    // The tool name and phase arrive over the socket; the log is not a channel for
    // whatever they happen to contain.
    #[test]
    fn a_tool_name_that_is_not_an_identifier_is_bounded_before_it_is_logged() {
        let (log, rx) = EventLog::capture();
        let mut presentation = Presentation::new();
        presentation.set_event_log(log);
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        presentation.note_dispatch("r1", false, &state, now);
        presentation.handle_server_message(
            &tool_activity_of("r1", "cat /home/me/.ssh/id_rsa", "Started!", "running"),
            &mut out,
            &mut state,
            now,
        );

        let logged = drain_events(&rx).pop().expect("one activity event");
        assert_eq!(logged["tool_name"], "other");
        assert_eq!(logged["phase"], "other");
    }
}

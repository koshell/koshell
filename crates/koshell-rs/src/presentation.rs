//! AI response presentation: renders streamed daemon replies into the user's terminal
//! while preserving the mirror-feed invariant (the mirror consumes exactly the bytes
//! written to the terminal, PTY plus presentation output, in the same order).
//!
//! Buffering follows design 0002's "buffer the bounded side" rule, in its prototype
//! simplification:
//! - **Command ended** at fire time: PTY visible output (the returning prompt) is held
//!   back from the moment the request is dispatched, the AI response streams first,
//!   and the held output flushes after it — so `#?` reads like a command that prints
//!   its answer before the next prompt. Two bounds keep the hold safe: a size fuse
//!   (a new command's output cannot grow the buffer without bound) and a max-hold
//!   deadline (a hung daemon cannot freeze the prompt); past either, the buffer
//!   flushes and output interleaves. If nothing has been rendered shortly after
//!   dispatch, one dim waiting notice tells the user the answer is coming.
//! - **Command still running**: program output keeps flowing in real time; deltas
//!   accumulate and the whole response is inserted as one block when it completes.
//!   Quiescence-gap insertion and its max-wait are deferred to dogfooding.
//!
//! The AI output style (a dim `[koshell ai]` header) is a placeholder; design 0002
//! leaves the final prefix/style open.

use std::collections::HashMap;
use std::io::Write;
use std::time::{Duration, Instant};

use koshell_proto::ServerMessage;

use crate::trigger::SessionState;

/// Buffered-PTY fuse: past this, buffering gives up and output interleaves, so a
/// user launching a new command mid-response cannot grow the buffer without bound.
const PTY_BUFFER_FUSE_BYTES: usize = 256 * 1024;

/// If nothing of the response has rendered this long after dispatch, print one dim
/// waiting notice (same philosophy as the pending-trigger receipt in design 0001).
/// Tunable during dogfooding.
const RECEIPT_NOTICE_DELAY: Duration = Duration::from_secs(1);

/// How long stream-mode buffering may hold PTY output while waiting for the response
/// to finish. Past this the buffer flushes and output interleaves, so a hung daemon
/// can never freeze the terminal. Tunable during dogfooding.
const RESPONSE_MAX_HOLD: Duration = Duration::from_secs(30);

const AI_HEADER: &str = "\r\n\x1b[2m[koshell ai]\x1b[0m\r\n";

/// How an in-flight response is rendered, decided by the trigger's completion state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Command ended: stream deltas, buffer PTY output until the response ends.
    Stream,
    /// Command still running: PTY flows, accumulate deltas, insert one block at end.
    Block,
}

#[derive(Debug)]
struct ActiveResponse {
    request_id: String,
    mode: Mode,
    dispatched_at: Instant,
    /// Whether the delayed waiting notice was printed.
    receipt_shown: bool,
    /// Stream mode: whether the header was written (deferred to the first delta).
    started: bool,
    /// Stream mode: whether the last written delta byte was a carriage return,
    /// so `\r\n` split across deltas is not doubled by normalization.
    last_was_cr: bool,
    /// Block mode: accumulated response text.
    accumulated: String,
    /// Stream mode: PTY visible bytes held back while the response is in flight.
    buffered_pty: Vec<u8>,
    /// Stream mode: the fuse blew or the max hold expired; stop buffering.
    interleaved: bool,
}

impl ActiveResponse {
    fn new(request_id: String, mode: Mode, dispatched_at: Instant) -> Self {
        Self {
            request_id,
            mode,
            dispatched_at,
            receipt_shown: false,
            started: false,
            last_was_cr: false,
            accumulated: String::new(),
            buffered_pty: Vec::new(),
            interleaved: false,
        }
    }

    /// True while nothing of the response has been rendered to the terminal.
    fn nothing_rendered(&self) -> bool {
        !self.started && self.accumulated.is_empty()
    }

    /// True while stream-mode buffering is holding PTY output.
    fn holding_pty(&self) -> bool {
        self.mode == Mode::Stream && !self.interleaved
    }
}

/// Renders daemon server messages into the terminal and decides when PTY output is
/// written through versus buffered. One instance per session, driven by the
/// processor thread.
pub struct Presentation {
    /// Requests dispatched to the daemon that have not finished: request id ->
    /// still-running at fire time.
    dispatched: HashMap<String, bool>,
    active: Option<ActiveResponse>,
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

/// Prints a dim one-line presentation notice, mirror-fed like all output.
fn notice<W: Write>(text: &str, out: &mut W, state: &mut SessionState) {
    let bytes = format!("\r\n\x1b[2m[koshell] {text}\x1b[0m\r\n");
    let _ = out.write_all(bytes.as_bytes());
    let _ = out.flush();
    state.record_presentation_output(bytes.as_bytes());
}

impl Presentation {
    pub fn new() -> Self {
        Self {
            dispatched: HashMap::new(),
            active: None,
        }
    }

    /// Records a `#?` request handed to the daemon and the bounded-side decision made
    /// at fire time (whether the triggering command was still running). For a command
    /// that ended, PTY buffering starts here — before the first delta — so the
    /// returning prompt is already held while the daemon thinks.
    pub fn note_dispatch(&mut self, request_id: &str, still_running: bool, now: Instant) {
        self.dispatched
            .insert(request_id.to_string(), still_running);
        if self.active.is_none() {
            let mode = if still_running {
                Mode::Block
            } else {
                Mode::Stream
            };
            self.active = Some(ActiveResponse::new(request_id.to_string(), mode, now));
        }
    }

    /// Routes PTY visible output: buffered while a stream-mode response is in flight
    /// (until the fuse blows or the hold expires), written through otherwise.
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
            if active.buffered_pty.len() > PTY_BUFFER_FUSE_BYTES {
                active.interleaved = true;
                let buffered = std::mem::take(&mut active.buffered_pty);
                let _ = out.write_all(&buffered);
                let _ = out.flush();
                state.record_output(&buffered, now);
            }
            return;
        }
        let _ = out.write_all(visible);
        let _ = out.flush();
        state.record_output(visible, now);
    }

    /// Time until the next presentation deadline (waiting notice or max hold), used
    /// to bound the processor's channel wait alongside the trigger deadlines.
    pub fn next_deadline(&self, now: Instant) -> Option<Duration> {
        let active = self.active.as_ref()?;
        let mut next: Option<Instant> = None;
        if !active.receipt_shown && active.nothing_rendered() {
            next = Some(active.dispatched_at + RECEIPT_NOTICE_DELAY);
        }
        if active.holding_pty() {
            let hold = active.dispatched_at + RESPONSE_MAX_HOLD;
            next = Some(next.map_or(hold, |n| n.min(hold)));
        }
        next.map(|deadline| deadline.saturating_duration_since(now))
    }

    /// Fires due presentation deadlines: the delayed waiting notice, and the
    /// max-hold release of buffered PTY output.
    pub fn poll<W: Write>(&mut self, now: Instant, out: &mut W, state: &mut SessionState) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if !active.receipt_shown
            && active.nothing_rendered()
            && now >= active.dispatched_at + RECEIPT_NOTICE_DELAY
        {
            active.receipt_shown = true;
            notice("waiting for the AI answer…", out, state);
        }
        if active.holding_pty() && now >= active.dispatched_at + RESPONSE_MAX_HOLD {
            active.interleaved = true;
            active.receipt_shown = true;
            let buffered = std::mem::take(&mut active.buffered_pty);
            notice(
                "still waiting for the AI answer; releasing command output",
                out,
                state,
            );
            if !buffered.is_empty() {
                let _ = out.write_all(&buffered);
                let _ = out.flush();
                state.record_output(&buffered, now);
            }
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
        }
    }

    fn on_delta<W: Write>(
        &mut self,
        request_id: &str,
        delta: &str,
        out: &mut W,
        state: &mut SessionState,
        now: Instant,
    ) {
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
            self.active = Some(ActiveResponse::new(request_id.to_string(), mode, now));
        }
        let active = self.active.as_mut().expect("active response just ensured");
        match active.mode {
            Mode::Stream => {
                if !active.started {
                    active.started = true;
                    let _ = out.write_all(AI_HEADER.as_bytes());
                    state.record_presentation_output(AI_HEADER.as_bytes());
                }
                let bytes = normalize_newlines(delta, &mut active.last_was_cr);
                let _ = out.write_all(&bytes);
                let _ = out.flush();
                state.record_presentation_output(&bytes);
            }
            Mode::Block => active.accumulated.push_str(delta),
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

        match active.mode {
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
                    let _ = out.write_all(&block);
                    let _ = out.flush();
                    state.record_presentation_output(&block);
                }
                if let Some(message) = error {
                    notice(&format!("AI error: {message}"), out, state);
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

    fn state() -> SessionState {
        SessionState::new(80, 24, true)
    }

    fn delta(request_id: &str, text: &str) -> ServerMessage {
        ServerMessage::AiDelta {
            request_id: request_id.to_string(),
            delta: text.to_string(),
        }
    }

    fn end(request_id: &str) -> ServerMessage {
        ServerMessage::AiResponseEnd {
            request_id: request_id.to_string(),
        }
    }

    #[test]
    fn stream_mode_buffers_pty_from_dispatch_until_response_end() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        // The returning prompt arrives before the first delta and must be held.
        presentation.note_dispatch("r1", false, now);
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

        presentation.note_dispatch("r1", true, now);
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

        presentation.note_dispatch("r1", false, now);
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
    fn pty_buffer_fuse_flushes_and_interleaves() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        presentation.note_dispatch("r1", false, now);
        presentation.handle_server_message(&delta("r1", "answer"), &mut out, &mut state, now);

        let big = vec![b'x'; PTY_BUFFER_FUSE_BYTES + 1];
        presentation.pty_output(&big, &mut out, &mut state, now);
        assert!(out.len() > PTY_BUFFER_FUSE_BYTES, "fuse flushed the buffer");

        // Past the fuse, PTY output writes through immediately.
        presentation.pty_output(b"more", &mut out, &mut state, now);
        assert!(String::from_utf8_lossy(&out).ends_with("more"));

        // Response end must not replay already-flushed bytes.
        let len_before = out.len();
        presentation.handle_server_message(&end("r1"), &mut out, &mut state, now);
        assert!(
            out.len() - len_before <= 2,
            "only the closing newline is written"
        );
    }

    #[test]
    fn waiting_notice_fires_once_when_nothing_rendered() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        presentation.note_dispatch("r1", false, now);
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

        presentation.note_dispatch("r1", false, now);
        presentation.handle_server_message(&delta("r1", "quick"), &mut out, &mut state, now);
        let len = out.len();
        let later = now + RECEIPT_NOTICE_DELAY + Duration::from_millis(10);
        presentation.poll(later, &mut out, &mut state);
        assert_eq!(out.len(), len, "streaming output is its own receipt");
    }

    #[test]
    fn max_hold_releases_buffered_output() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        presentation.note_dispatch("r1", false, now);
        presentation.pty_output(b"$ ", &mut out, &mut state, now);
        assert!(out.is_empty());

        let expired = now + RESPONSE_MAX_HOLD + Duration::from_millis(10);
        presentation.poll(expired, &mut out, &mut state);
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("releasing command output"));
        assert!(text.ends_with("$ "), "held prompt flushes on max hold");

        // Buffering stays off for the rest of this response.
        presentation.pty_output(b"typed", &mut out, &mut state, expired);
        assert!(String::from_utf8_lossy(&out).ends_with("typed"));
        assert!(presentation.next_deadline(expired).is_none());
    }

    #[test]
    fn error_mid_stream_flushes_buffer_and_prints_notice() {
        let mut presentation = Presentation::new();
        let mut state = state();
        let mut out: Vec<u8> = Vec::new();
        let now = Instant::now();

        presentation.note_dispatch("r1", false, now);
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

        presentation.note_dispatch("r1", false, now);
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
}

//! Bounded, in-memory index of completed shell commands and their output.
//!
//! This is the terminal-side store behind the read-only pull tools
//! (`list_recent_commands`, `read_command_output`). It exists because the pushed
//! context package is bounded by the screen: a dogfooding case on 2026-07-10 produced
//! an answer grounded only in the currently visible portion of a command whose output
//! ran well past one screen. The store gives the agent bounded random access by command
//! instead of a larger push.
//!
//! It is deliberately separate from [`crate::timeline`]. The timeline serves *recent*
//! contextual facts under a session-wide character budget; this store serves *one
//! selected command* under per-command and per-session byte caps. They share a command
//! id and nothing else.
//!
//! What it retains is `pty_text`: the marker-clean bytes observed between a real
//! `command_start` and its `command_end`, including carriage returns and control
//! sequences. It does not claim to reconstruct a rendered scrollback buffer, and it
//! does not identify the producing process — a background job writing to the same
//! terminal lands inside the foreground span, because a byte stream carries no such
//! attribution.
//!
//! Nothing here is written to disk.

use std::collections::VecDeque;

/// Completed commands whose metadata is retained, newest first.
pub const MAX_COMPLETED_COMMANDS: usize = 10;

/// Retained output for a single command, in UTF-8 bytes of the decoded text.
pub const MAX_COMMAND_OUTPUT_BYTES: usize = 1024 * 1024;

/// Retained output across the active capture plus all completed commands.
pub const MAX_TOTAL_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

/// Stored command text. An abnormal shell line (a pasted blob, a generated one-liner)
/// must not defeat the store's bounds through the metadata path.
pub const MAX_COMMAND_TEXT_BYTES: usize = 4 * 1024;

/// Command text returned per row by the list tool, in Unicode scalar values.
pub const MAX_COMMAND_PREVIEW_CHARACTERS: usize = 512;

/// Default page size for a read, in Unicode scalar values.
pub const DEFAULT_READ_LIMIT: usize = 8_000;

/// Hard cap on one read page, in Unicode scalar values.
pub const MAX_READ_LIMIT: usize = 16_000;

/// Truthful accounting for one command's output. Every field is reported on every
/// result so absence is never presented as complete output: `total_*` is what the
/// command produced, `retained_*` is what survives the caps, and the offset/dropped
/// fields say exactly where the gap is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutputInfo {
    /// Always `pty_text`. Named so a later rendered-text format is additive.
    pub format: &'static str,
    pub total_bytes: usize,
    pub total_characters: usize,
    pub retained_bytes: usize,
    pub retained_characters: usize,
    /// Absolute character offset of the first retained scalar in the original output.
    pub retained_start_offset: usize,
    pub dropped_prefix_bytes: usize,
    /// Whether any output was dropped, by the per-command cap or by reclamation.
    pub source_truncated: bool,
    /// Whether the retained text can serve everything the store knows about. False
    /// once output existed but was fully reclaimed.
    pub available: bool,
}

/// One completed command's metadata. Output lives beside it and may be reclaimed
/// independently, which is why the metadata row survives its own output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedCommand {
    pub command_id: String,
    /// Full stored command text, capped at [`MAX_COMMAND_TEXT_BYTES`].
    pub command: String,
    pub command_truncated: bool,
    pub cwd: Option<String>,
    pub started_at: i64,
    pub ended_at: i64,
    pub duration_ms: u64,
    pub exit_code: Option<i32>,
    output: OutputBuffer,
}

impl CompletedCommand {
    pub fn output_info(&self) -> CommandOutputInfo {
        self.output.info()
    }

    /// The list tool's bounded command text.
    pub fn command_preview(&self) -> (String, bool) {
        preview(&self.command, self.command_truncated)
    }
}

/// Why a read could not be served. These map onto the tool-error codes the agent sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// No completed command carries this id.
    CommandNotFound,
    /// The requested offset predates the retained window. Carries the earliest offset
    /// still available, so the agent can re-ask precisely instead of guessing.
    OutputEvicted { earliest_offset: usize },
}

/// One page of a command's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadPage {
    pub command_id: String,
    pub info: CommandOutputInfo,
    /// Absolute character offset this page starts at.
    pub offset: usize,
    pub next_offset: usize,
    pub has_more: bool,
    pub content: String,
}

/// Bounded retained text plus the accounting needed to describe what was dropped.
///
/// Byte counters refer to the UTF-8 encoding of the *decoded* text, not to the original
/// wire bytes: invalid input became U+FFFD before it got here, so counting wire bytes
/// would report sizes the store cannot serve. Character offsets count Unicode scalar
/// values in that same decoded text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct OutputBuffer {
    retained: String,
    total_bytes: usize,
    total_characters: usize,
    retained_start_offset: usize,
    dropped_prefix_bytes: usize,
    source_truncated: bool,
}

impl OutputBuffer {
    fn append(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.total_bytes += text.len();
        self.total_characters += text.chars().count();
        self.retained.push_str(text);
        self.trim_to(MAX_COMMAND_OUTPUT_BYTES);
    }

    /// Drops whole scalars from the front until the retained text fits `max_bytes`.
    /// Keeping the recent tail is the deliberate choice: the end of a command's output
    /// is where the error usually is.
    fn trim_to(&mut self, max_bytes: usize) {
        if self.retained.len() <= max_bytes {
            return;
        }
        let target_drop = self.retained.len() - max_bytes;
        let mut dropped_bytes = 0;
        let mut dropped_chars = 0;
        for character in self.retained.chars() {
            if dropped_bytes >= target_drop {
                break;
            }
            dropped_bytes += character.len_utf8();
            dropped_chars += 1;
        }
        self.retained.drain(..dropped_bytes);
        self.retained_start_offset += dropped_chars;
        self.dropped_prefix_bytes += dropped_bytes;
        self.source_truncated = true;
    }

    /// Releases all retained output, keeping the accounting. Used when the session-wide
    /// cap reclaims an old command: the metadata row stays, its output does not.
    fn reclaim_all(&mut self) -> usize {
        let freed = self.retained.len();
        if freed == 0 {
            return 0;
        }
        self.retained_start_offset += self.retained.chars().count();
        self.dropped_prefix_bytes += freed;
        self.source_truncated = true;
        self.retained.clear();
        freed
    }

    /// Releases at least `bytes` from the front, returning how much was freed.
    fn reclaim_prefix(&mut self, bytes: usize) -> usize {
        if bytes >= self.retained.len() {
            return self.reclaim_all();
        }
        let before = self.retained.len();
        self.trim_to(before - bytes);
        before - self.retained.len()
    }

    fn info(&self) -> CommandOutputInfo {
        CommandOutputInfo {
            format: "pty_text",
            total_bytes: self.total_bytes,
            total_characters: self.total_characters,
            retained_bytes: self.retained.len(),
            retained_characters: self.retained.chars().count(),
            retained_start_offset: self.retained_start_offset,
            dropped_prefix_bytes: self.dropped_prefix_bytes,
            source_truncated: self.source_truncated,
            available: self.total_characters == 0 || !self.retained.is_empty(),
        }
    }
}

/// The span currently being captured, between a real `command_start` and its
/// `command_end`.
#[derive(Debug)]
struct ActiveSpan {
    command_id: String,
    command: String,
    command_truncated: bool,
    cwd: Option<String>,
    started_at: i64,
    output: OutputBuffer,
    /// Bytes of an incomplete UTF-8 sequence split across PTY chunks.
    partial: Vec<u8>,
}

/// Newest-first index of completed commands, with one span captured at a time.
pub struct CommandHistory {
    completed: VecDeque<CompletedCommand>,
    active: Option<ActiveSpan>,
    next_id: u64,
    now: Box<dyn Fn() -> i64 + Send + Sync>,
    /// Set when a span was abandoned (a new start arrived while one was active, or the
    /// shell exited mid-span). Drained by the session loop into the log.
    warnings: Vec<String>,
}

fn default_now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Truncates on a scalar boundary at or below `max_bytes`, reporting whether it cut.
fn truncate_command(command: &str) -> (String, bool) {
    if command.len() <= MAX_COMMAND_TEXT_BYTES {
        return (command.to_string(), false);
    }
    let mut end = 0;
    for (index, _) in command.char_indices() {
        if index > MAX_COMMAND_TEXT_BYTES {
            break;
        }
        end = index;
    }
    (command[..end].to_string(), true)
}

fn preview(command: &str, already_truncated: bool) -> (String, bool) {
    let preview: String = command
        .chars()
        .take(MAX_COMMAND_PREVIEW_CHARACTERS)
        .collect();
    // Truncated here, or already truncated in storage: either way the row's text is
    // not the whole command line, and the flag has to say so.
    let cut_here = preview.chars().count() < command.chars().count();
    (preview, cut_here || already_truncated)
}

impl Default for CommandHistory {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandHistory {
    pub fn new() -> Self {
        Self::with_clock(default_now)
    }

    pub fn with_clock<F: Fn() -> i64 + Send + Sync + 'static>(now: F) -> Self {
        Self {
            completed: VecDeque::new(),
            active: None,
            next_id: 1,
            now: Box::new(now),
            warnings: Vec::new(),
        }
    }

    /// Takes the warnings accumulated since the last call, for the caller to log.
    pub fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    /// The id of the span currently being captured, if any.
    pub fn active_command_id(&self) -> Option<&str> {
        self.active.as_ref().map(|span| span.command_id.as_str())
    }

    /// Completed commands, newest first.
    pub fn recent(&self) -> Vec<&CompletedCommand> {
        self.completed.iter().rev().collect()
    }

    pub fn completed_count(&self) -> usize {
        self.completed.len()
    }

    /// The newest completed command's id, advertised in the pushed inventory.
    pub fn latest_command_id(&self) -> Option<&str> {
        self.completed.back().map(|c| c.command_id.as_str())
    }

    /// Opens a span for a real (executed) `command_start`.
    ///
    /// A start arriving while another span is active means the previous span's
    /// `command_end` never came — the shell died, the marker was lost, or a nested
    /// emitter interleaved. The incomplete capture is discarded rather than closed with
    /// invented boundaries: a wrong span attached to a stable id is worse than no span.
    pub fn begin(&mut self, command: &str, cwd: Option<String>) -> String {
        if let Some(previous) = self.active.take() {
            self.warnings.push(format!(
                "discarded the incomplete capture for {} (a new command started before it ended)",
                previous.command_id
            ));
        }
        let command_id = format!("command-{}", self.next_id);
        self.next_id += 1;
        let (text, truncated) = truncate_command(command);
        self.active = Some(ActiveSpan {
            command_id: command_id.clone(),
            command: text,
            command_truncated: truncated,
            cwd,
            started_at: (self.now)(),
            output: OutputBuffer::default(),
            partial: Vec::new(),
        });
        command_id
    }

    /// Appends marker-clean PTY bytes to the active span. A no-op with no active span,
    /// so the returning prompt and anything typed at an idle prompt stay out.
    ///
    /// Decoding is incremental: a scalar split across two PTY reads is held until its
    /// remaining bytes arrive, and a genuinely malformed sequence becomes U+FFFD rather
    /// than dropping the surrounding text.
    pub fn record_output(&mut self, bytes: &[u8]) {
        let Some(span) = self.active.as_mut() else {
            return;
        };
        if bytes.is_empty() {
            return;
        }

        let mut buffer = std::mem::take(&mut span.partial);
        buffer.extend_from_slice(bytes);

        let mut decoded = String::new();
        let mut cursor = 0;
        loop {
            match std::str::from_utf8(&buffer[cursor..]) {
                Ok(text) => {
                    decoded.push_str(text);
                    cursor = buffer.len();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    // SAFETY-equivalent: `valid_up_to` guarantees this prefix is UTF-8.
                    decoded.push_str(
                        std::str::from_utf8(&buffer[cursor..cursor + valid]).unwrap_or_default(),
                    );
                    cursor += valid;
                    match error.error_len() {
                        // Malformed: substitute and skip exactly the bad bytes.
                        Some(bad) => {
                            decoded.push('\u{FFFD}');
                            cursor += bad;
                        }
                        // Truncated at the end: hold it for the next chunk.
                        None => break,
                    }
                }
            }
        }

        span.partial = buffer[cursor..].to_vec();
        // A held tail that can never complete is malformed, not pending. Four bytes is
        // the longest valid sequence, so anything longer is stuck.
        if span.partial.len() >= 4 {
            decoded.push('\u{FFFD}');
            span.partial.clear();
        }
        span.output.append(&decoded);
        self.enforce_total_cap();
    }

    /// Closes the active span into a completed row. Returns its id, or `None` when no
    /// span was open — a `command_end` without a start never invents a command.
    pub fn end(&mut self, exit_code: Option<i32>) -> Option<String> {
        let mut span = self.active.take()?;
        // Any held partial bytes will never complete now.
        if !span.partial.is_empty() {
            span.output.append("\u{FFFD}");
        }
        let ended_at = (self.now)();
        let command_id = span.command_id.clone();
        self.completed.push_back(CompletedCommand {
            command_id: command_id.clone(),
            command: span.command,
            command_truncated: span.command_truncated,
            cwd: span.cwd,
            started_at: span.started_at,
            ended_at,
            duration_ms: ended_at.saturating_sub(span.started_at).max(0) as u64,
            exit_code,
            output: span.output,
        });
        while self.completed.len() > MAX_COMPLETED_COMMANDS {
            self.completed.pop_front();
        }
        self.enforce_total_cap();
        Some(command_id)
    }

    /// Discards an unfinished capture (shell exit). The command never completed, so it
    /// never becomes a history row.
    pub fn abandon_active(&mut self) {
        if let Some(span) = self.active.take() {
            self.warnings.push(format!(
                "discarded the incomplete capture for {} (the shell exited before it ended)",
                span.command_id
            ));
        }
    }

    /// Total retained output across the active capture and every completed row.
    pub fn retained_bytes(&self) -> usize {
        self.active
            .as_ref()
            .map_or(0, |span| span.output.retained.len())
            + self
                .completed
                .iter()
                .map(|command| command.output.retained.len())
                .sum::<usize>()
    }

    /// Reclaims output from the oldest completed commands until the session fits its
    /// cap. Metadata rows survive; only their output is released, and the released
    /// amount is recorded so a later read reports the gap instead of a short answer.
    ///
    /// The active span is never reclaimed here: it is already bounded per-command, and
    /// it is the one the user is most likely asking about.
    fn enforce_total_cap(&mut self) {
        let mut total = self.retained_bytes();
        if total <= MAX_TOTAL_OUTPUT_BYTES {
            return;
        }
        for command in self.completed.iter_mut() {
            if total <= MAX_TOTAL_OUTPUT_BYTES {
                break;
            }
            let excess = total - MAX_TOTAL_OUTPUT_BYTES;
            total -= command.output.reclaim_prefix(excess);
        }
    }

    /// Reads one page of a completed command's output.
    ///
    /// `offset` is an absolute character offset in the command's *original* output, so
    /// a page reference stays meaningful after prefix reclamation. Requesting an offset
    /// older than the retained window is an explicit `OutputEvicted` naming the earliest
    /// available offset, never a silently shifted page.
    pub fn read(
        &self,
        command_id: &str,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> Result<ReadPage, ReadError> {
        let command = self
            .completed
            .iter()
            .find(|candidate| candidate.command_id == command_id)
            .ok_or(ReadError::CommandNotFound)?;

        let info = command.output.info();
        let start = offset.unwrap_or(info.retained_start_offset);
        if start < info.retained_start_offset {
            return Err(ReadError::OutputEvicted {
                earliest_offset: info.retained_start_offset,
            });
        }

        let limit = limit.unwrap_or(DEFAULT_READ_LIMIT).clamp(1, MAX_READ_LIMIT);
        let skip = start - info.retained_start_offset;
        let content: String = command
            .output
            .retained
            .chars()
            .skip(skip)
            .take(limit)
            .collect();
        let taken = content.chars().count();
        let next_offset = start + taken;
        let end_of_retained = info.retained_start_offset + info.retained_characters;

        Ok(ReadPage {
            command_id: command.command_id.clone(),
            offset: start,
            next_offset,
            has_more: next_offset < end_of_retained,
            content,
            info,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    fn clocked() -> (CommandHistory, Arc<AtomicI64>) {
        let clock = Arc::new(AtomicI64::new(0));
        let handle = clock.clone();
        (
            CommandHistory::with_clock(move || handle.load(Ordering::SeqCst)),
            clock,
        )
    }

    fn history() -> CommandHistory {
        clocked().0
    }

    #[test]
    fn one_id_spans_start_through_end() {
        let mut store = history();
        let id = store.begin("ls -la", Some("/tmp".to_string()));
        store.record_output(b"file-a\r\nfile-b\r\n");
        assert_eq!(store.active_command_id(), Some(id.as_str()));
        assert_eq!(store.end(Some(0)).as_deref(), Some(id.as_str()));

        let recent = store.recent();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].command_id, id);
        assert_eq!(recent[0].command, "ls -la");
        assert_eq!(recent[0].cwd.as_deref(), Some("/tmp"));
        assert_eq!(recent[0].exit_code, Some(0));
        assert_eq!(
            store.read(&id, None, None).unwrap().content,
            "file-a\r\nfile-b\r\n"
        );
    }

    #[test]
    fn duration_comes_from_the_span_boundaries() {
        let (mut store, clock) = clocked();
        clock.store(1_000, Ordering::SeqCst);
        let id = store.begin("sleep 2", None);
        clock.store(3_500, Ordering::SeqCst);
        store.end(Some(0));

        let recent = store.recent();
        assert_eq!(recent[0].started_at, 1_000);
        assert_eq!(recent[0].ended_at, 3_500);
        assert_eq!(recent[0].duration_ms, 2_500);
        assert_eq!(
            store.read(&id, None, None).unwrap().info.total_characters,
            0
        );
    }

    #[test]
    fn output_outside_a_span_is_not_captured() {
        let mut store = history();
        // The returning prompt, and anything typed at an idle prompt.
        store.record_output(b"$ ");
        let id = store.begin("echo hi", None);
        store.record_output(b"hi\r\n");
        store.end(Some(0));
        store.record_output(b"$ ");

        assert_eq!(store.read(&id, None, None).unwrap().content, "hi\r\n");
    }

    #[test]
    fn an_end_without_a_start_creates_no_row() {
        let mut store = history();
        assert_eq!(store.end(Some(0)), None);
        assert_eq!(store.completed_count(), 0);
    }

    #[test]
    fn a_new_start_discards_the_incomplete_capture_with_a_warning() {
        let mut store = history();
        store.begin("first", None);
        store.record_output(b"partial");
        let second = store.begin("second", None);

        let warnings = store.take_warnings();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("command-1"));
        assert!(warnings[0].contains("a new command started"));

        store.record_output(b"done");
        store.end(Some(0));
        assert_eq!(store.completed_count(), 1);
        assert_eq!(store.recent()[0].command_id, second);
        assert_eq!(store.read(&second, None, None).unwrap().content, "done");
    }

    #[test]
    fn shell_exit_discards_the_unfinished_span() {
        let mut store = history();
        store.begin("tail -f log", None);
        store.record_output(b"line");
        store.abandon_active();

        assert_eq!(store.completed_count(), 0);
        assert!(store.take_warnings()[0].contains("the shell exited"));
    }

    #[test]
    fn a_failed_command_is_still_a_completed_span() {
        let mut store = history();
        let id = store.begin("false", None);
        store.end(Some(1));
        assert_eq!(store.recent()[0].exit_code, Some(1));
        assert!(store.read(&id, None, None).unwrap().info.available);
    }

    #[test]
    fn only_the_ten_most_recent_commands_are_indexed() {
        let mut store = history();
        for index in 0..13 {
            store.begin(&format!("cmd-{index}"), None);
            store.end(Some(0));
        }
        let recent = store.recent();
        assert_eq!(recent.len(), MAX_COMPLETED_COMMANDS);
        // Newest first.
        assert_eq!(recent[0].command, "cmd-12");
        assert_eq!(recent[9].command, "cmd-3");
        assert_eq!(store.latest_command_id(), Some("command-13"));
    }

    #[test]
    fn utf8_split_across_chunks_is_decoded_whole() {
        let mut store = history();
        let id = store.begin("cat unicode.txt", None);
        let bytes = "日本語".as_bytes();
        // Split mid-scalar in two places.
        store.record_output(&bytes[..2]);
        store.record_output(&bytes[2..7]);
        store.record_output(&bytes[7..]);
        store.end(Some(0));

        assert_eq!(store.read(&id, None, None).unwrap().content, "日本語");
    }

    #[test]
    fn malformed_bytes_become_replacement_characters() {
        let mut store = history();
        let id = store.begin("cat binary", None);
        store.record_output(&[b'a', 0xFF, 0xFE, b'b']);
        store.end(Some(0));

        assert_eq!(
            store.read(&id, None, None).unwrap().content,
            "a\u{FFFD}\u{FFFD}b"
        );
    }

    #[test]
    fn a_dangling_partial_sequence_is_replaced_at_span_end() {
        let mut store = history();
        let id = store.begin("cat truncated", None);
        store.record_output(&"日".as_bytes()[..2]);
        store.end(Some(0));

        assert_eq!(store.read(&id, None, None).unwrap().content, "\u{FFFD}");
    }

    #[test]
    fn a_single_command_is_capped_and_keeps_its_tail() {
        let mut store = history();
        let id = store.begin("yes", None);
        // Two full caps' worth, in chunks.
        let chunk = vec![b'a'; 64 * 1024];
        for _ in 0..32 {
            store.record_output(&chunk);
        }
        store.record_output(b"TAIL");
        store.end(Some(0));

        let info = store.read(&id, None, None).unwrap().info;
        assert!(info.retained_bytes <= MAX_COMMAND_OUTPUT_BYTES);
        assert_eq!(info.total_bytes, 32 * 64 * 1024 + 4);
        assert!(info.source_truncated);
        assert!(info.dropped_prefix_bytes > 0);
        assert_eq!(
            info.retained_start_offset + info.retained_characters,
            info.total_characters
        );

        // The tail survives: the end of a command's output is where the error is.
        let page = store
            .read(&id, Some(info.total_characters - 4), Some(16))
            .unwrap();
        assert_eq!(page.content, "TAIL");
    }

    #[test]
    fn the_session_cap_reclaims_the_oldest_output_but_keeps_its_metadata() {
        let mut store = history();
        let chunk = vec![b'x'; 256 * 1024];
        let mut ids = Vec::new();
        for index in 0..8 {
            ids.push(store.begin(&format!("big-{index}"), None));
            for _ in 0..3 {
                store.record_output(&chunk);
            }
            store.end(Some(0));
        }

        assert!(
            store.retained_bytes() <= MAX_TOTAL_OUTPUT_BYTES,
            "retained {} exceeds the session cap",
            store.retained_bytes()
        );
        // Every command still has a metadata row.
        assert_eq!(store.completed_count(), 8);

        // The oldest lost its output and says so, rather than reporting a short read.
        let oldest = store.read(&ids[0], None, None).unwrap();
        assert_eq!(oldest.info.retained_bytes, 0);
        assert!(oldest.info.source_truncated);
        assert!(!oldest.info.available);
        assert!(oldest.info.total_bytes > 0);
    }

    #[test]
    fn paging_covers_retained_text_without_gaps_or_overlap() {
        let mut store = history();
        let id = store.begin("seq", None);
        let text: String = (0..5_000)
            .map(|n| char::from(b'a' + (n % 26) as u8))
            .collect();
        store.record_output(text.as_bytes());
        store.end(Some(0));

        let mut assembled = String::new();
        let mut offset = 0;
        loop {
            let page = store.read(&id, Some(offset), Some(700)).unwrap();
            assembled.push_str(&page.content);
            offset = page.next_offset;
            if !page.has_more {
                break;
            }
        }
        assert_eq!(assembled, text);
    }

    #[test]
    fn paging_is_scalar_aligned_across_multibyte_text() {
        let mut store = history();
        let id = store.begin("cat cjk", None);
        let text: String = "日本語テキスト".repeat(500);
        store.record_output(text.as_bytes());
        store.end(Some(0));

        let mut assembled = String::new();
        let mut offset = 0;
        loop {
            let page = store.read(&id, Some(offset), Some(333)).unwrap();
            assembled.push_str(&page.content);
            offset = page.next_offset;
            if !page.has_more {
                break;
            }
        }
        assert_eq!(assembled, text);
    }

    #[test]
    fn the_read_limit_is_clamped() {
        let mut store = history();
        let id = store.begin("seq", None);
        store.record_output(&vec![b'z'; MAX_READ_LIMIT * 2]);
        store.end(Some(0));

        let page = store.read(&id, None, Some(MAX_READ_LIMIT * 2)).unwrap();
        assert_eq!(page.content.chars().count(), MAX_READ_LIMIT);
        assert!(page.has_more);
    }

    #[test]
    fn reading_an_evicted_offset_names_the_earliest_available_one() {
        let mut store = history();
        let id = store.begin("yes", None);
        let chunk = vec![b'a'; 64 * 1024];
        for _ in 0..24 {
            store.record_output(&chunk);
        }
        store.end(Some(0));

        let earliest = store
            .read(&id, None, None)
            .unwrap()
            .info
            .retained_start_offset;
        assert!(earliest > 0);
        assert_eq!(
            store.read(&id, Some(0), None),
            Err(ReadError::OutputEvicted {
                earliest_offset: earliest
            })
        );
        // The named offset works.
        assert!(store.read(&id, Some(earliest), None).is_ok());
    }

    #[test]
    fn reading_an_unknown_id_is_command_not_found() {
        let mut store = history();
        store.begin("ls", None);
        store.end(Some(0));
        assert_eq!(
            store.read("command-99", None, None),
            Err(ReadError::CommandNotFound)
        );
        // The active span is not readable either: it has no completed row yet.
        store.begin("tail -f", None);
        assert_eq!(
            store.read("command-2", None, None),
            Err(ReadError::CommandNotFound)
        );
    }

    #[test]
    fn an_abnormal_command_line_is_capped_in_storage_and_in_the_preview() {
        let mut store = history();
        let long = "x".repeat(MAX_COMMAND_TEXT_BYTES * 2);
        store.begin(&long, None);
        store.end(Some(0));

        let command = &store.recent()[0];
        assert!(command.command.len() <= MAX_COMMAND_TEXT_BYTES);
        assert!(command.command_truncated);

        let (preview, truncated) = command.command_preview();
        assert_eq!(preview.chars().count(), MAX_COMMAND_PREVIEW_CHARACTERS);
        assert!(truncated);
    }

    #[test]
    fn a_short_command_previews_whole_and_untruncated() {
        let mut store = history();
        store.begin("git status", None);
        store.end(Some(0));
        assert_eq!(
            store.recent()[0].command_preview(),
            ("git status".to_string(), false)
        );
    }

    #[test]
    fn a_command_with_no_output_is_available_and_empty() {
        let mut store = history();
        let id = store.begin("true", None);
        store.end(Some(0));

        let page = store.read(&id, None, None).unwrap();
        assert_eq!(page.content, "");
        assert!(!page.has_more);
        assert!(page.info.available);
        assert!(!page.info.source_truncated);
        assert_eq!(page.info.total_bytes, 0);
    }
}

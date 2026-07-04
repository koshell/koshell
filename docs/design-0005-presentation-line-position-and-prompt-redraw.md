# Design 0005 — presentation line position and anchored streaming

Date: 2026-07-03 11:36:14 CST (original) / 2026-07-03 12:10 CST (styled reprint) /
2026-07-03 12:54 CST (anchored streaming replaces erase-and-reprint)

## Why

Dogfooding surfaced two presentation details:

1. `[koshell] waiting for the AI answer…` (and the `[koshell ai]` header) printed a
   blank line between the `#?` line and itself. The cause: every presentation line
   hard-coded a leading `\r\n`, but after the user presses Enter the cursor is already
   resting at the start of an empty line (the Enter echo has printed; the returning
   prompt is buffered), so the defensive newline became a visible blank line.
2. In a REPL (the stabilization path — "S3" in design 0001's original signal naming),
   the next `>>> ` prompt appears immediately after Enter, and the koshell output only
   lands below it. This is structural: stabilization _by definition_ fires only after
   the prompt has settled on screen, so the prompt cannot be held back the way the
   shell-integrated path buffers it (there, `command_end` arrives before the prompt is
   printed). The user is left with an orphaned prompt above the answer and no prompt
   below it.

## What changed (`crates/koshell-rs/src/presentation.rs`, `trigger.rs`, `session.rs`)

### Line-position rule

Presentation lines (notices, the `[koshell ai]` header, block insertions) now compute
their leading newline from the mirror instead of hard-coding it:
`SessionState::at_line_start()` reports whether the cursor rests at column 0 of an
empty row; if so, the leading `\r\n` is dropped. The mirror-feed invariant makes this
reliable — the mirror cursor is the real cursor, including while PTY output is being
buffered (held bytes are not fed to the mirror until they are flushed).

### Anchored streaming (revised 2026-07-03 12:54 CST; replaces erase-and-reprint)

The first fix here was a coarse erase-and-reprint: erase the rendered prompt at
dispatch, stream the answer in its place, reprint the prompt at the end. Dogfooding
rejected it — the prompt visibly vanished for the whole response, and typing was
held (blind) until the response finished. The replacement is **anchored streaming**,
the `patch_stdout` model: the cursor's live input line stays fully usable, and every
delta is inserted into the free zone directly above it.

Screen model while a response streams anchored:

- **Free zone** — the AI text so far; its last (possibly partial) line is the AI
  tail.
- **Live region** — the cursor's logical line (prompt plus echoed input, spanning
  soft-wrapped rows). PTY output writes through in real time; anchored mode never
  buffers it.

Invariant after every redraw: exactly one line break separates the AI tail from the
live region's top, and the real cursor rests at its live position — so program
echo between deltas lands exactly where the program believes the cursor is.

Per delta (`presentation.rs::anchored_delta`), all derived from the mirror (the
mirror-feed invariant makes the mirror cursor the real cursor — no local width
bookkeeping, CJK safe):

1. Sample the live region (`TerminalMirror::live_region`): styled rows, cursor
   column, terminal pending-wrap state, and the plain text of the row above.
2. Invariant check: the row above the live region must still equal the recorded AI
   tail. A mismatch (a mid-stream command printed output, the screen was cleared)
   degrades the rest of the response to block mode — one seam at the end, never
   line-level interleaving of program output and AI text.
3. Erase the live region bottom-up; resume the AI text (first delta writes the
   `[koshell ai]` header; a pending-wrap AI line resumes in place, since cursor
   movement would clear the pending wrap and lose the boundary column; otherwise
   cursor-up plus a column move to the recorded resume point).
4. Write the delta, record it, and re-sample the AI resume point from the mirror.
5. Rewrite the live region below — styling, soft wrapping, and cursor column intact.

Interjections come for free: because echo reaches the mirror live, the ordinary
Enter capture sees a mid-stream `#? question` and queues it as a normal pending
question (FIFO through the one conversation, as design 0001 already specifies). If
the interjected command runs immediately, its output triggers the degrade path
above, and the next response starts with a fresh anchor.

Standalone notices (`notice_before_prompt`: trigger notices, the daemon-unavailable
message, the waiting notice while anchored) use the same insert-above-live
primitive when the resting line is prompt-shaped, so the live line always stays the
last line on screen. Block-mode responses also insert their final block above a
prompt-shaped live line instead of consuming it.

The shell-integrated path is untouched: there `command_end` arrives before the
prompt prints, so the prompt is buffered and the answer streams first (design
0002's "buffer the bounded side"), with the size fuse and max-hold bounds — those
now apply only to that non-anchored path.

### Styled rewrite from grid cells (added 2026-07-03 12:10 CST)

Rewriting the live region needs its bytes, but the original byte stream is not
retained per row — so `TerminalMirror` re-synthesizes SGR styling from the grid
cells: runs of identical cell attributes become SGR parameter groups covering the
common attribute subset (bold, dim, italic, underline, inverse, strikeout) and all
three color forms (named 16-color, indexed 256-color, truecolor), with resets at
run boundaries. Underline colors and OSC 8 hyperlinks are accepted misses.

This replays sequences the program itself just rendered on this terminal, so it does
not violate the legacy-terminal rule that koshell's own chrome avoids rich color
sequences — the terminal supports them by construction.

The alternative considered — holding raw PTY bytes from submit until fire, which
would preserve the exact original bytes — was rejected as strictly more complex: it
breaks the mirror-feed invariant (stabilization reads the mirror, so the mirror
would have to run ahead of the real display, making snapshots and the
`at_line_start` check untruthful), needs its own hold state machine, relies on
splitting a raw escape-sequence stream heuristically, and freezes live output —
against design 0002's rule that a living producer owns the terminal in real time.

## Tests

- `crates/koshell-rs/src/mirror.rs` — `live_region` reconstructs styled SGR runs
  (named/indexed/truecolor), spans soft-wrapped input, reports the pending-wrap
  state, and is unavailable on the alternate screen.
- `crates/koshell-rs/src/trigger.rs` — `at_line_start` tracks the mirror cursor;
  `resting_prompt` matches prompt shapes only; `cursor_probe` reports the AI-tail
  facts.
- `crates/koshell-rs/src/presentation.rs` — replay-based assertions (the emitted
  byte stream is fed through a fresh terminal emulator and the final screen is
  checked): deltas insert above the live prompt; typed input stays live across
  redraws; an exactly-full AI line resumes without character loss; a soft-wrapped
  live line is restored; intervening program output degrades the remainder to one
  block; errors and the waiting notice keep the live line last; a mid-stream `#?`
  is captured and queued.
- `crates/koshell-rs/tests/anchored_stream_pty.rs` — end-to-end: a fake AI daemon
  (Unix socket, ack + two spaced deltas + end) drives a real python REPL session;
  typing lands mid-stream; the replayed final screen shows the answer directly
  above the live prompt carrying the typed input, which then executes normally.

## Open issues

- When the degrade path fires mid-response, the answer renders as two contiguous
  chunks (the streamed part in the free zone, the remainder as a block below the
  intervening output), each under its own `[koshell ai]` header. Bounded and
  self-identifying; revisit if dogfooding finds it confusing.
- The invariant check compares the row above the live region against the AI tail's
  plain text; program output that exactly reproduces that text passes the check
  falsely. Harmless (the redraw still lands adjacent to the live region).
- The prompt-shape heuristic can misclassify a real output line ending in a prompt
  tail character (for example `Compiling foo:`); the anchored gate then treats it
  as a live line and inserts above it rather than losing it. Bounded, and the same
  heuristic risk design 0001 already accepts for debounce tier selection.
- Cursor-column padding uses the char count, not display width; a live line whose
  trimmed text contains wide characters followed by trailing spaces would be
  over-padded. Prompt tails are all narrow, so this stays theoretical.
- Esc aborting a streaming response (design 0001's pending-trigger interaction)
  is superseded: interrupting a streaming response landed on Ctrl+C, and the
  bare-Esc cancel path was removed entirely (design 0006). Interjection still
  queues rather than cancels.

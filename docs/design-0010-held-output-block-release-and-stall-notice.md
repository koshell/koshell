# Design 0010 — held-output block release and the stall notice

Date: 2026-07-08 12:19:29 CST

Status: implemented.

## Why

Design 0002's buffered-stream prototype held the bounded side (the returning prompt)
while the answer streamed, guarded by two bounds that both **gave up by interleaving**:

- a 30s max-hold deadline that force-flushed the held output and then let subsequent
  `ai_delta`s and PTY output write through in real time;
- a 256 KiB size fuse that did the same the instant the held buffer grew too large.

Once either fired, answer text and command output landed on the terminal line by line
with no separator. Dogfooding showed the user could no longer tell which line was the
AI and which was the shell — the exact confusion the `[koshell ai]` header exists to
prevent, now reintroduced by the safety valves themselves. The triggering symptom was
the `still waiting for the AI answer; releasing command output` notice, after which
everything blurred together.

## Semantics

**The answer and command output are always kept in separate, labeled blocks — never
line-interleaved.** The two bounds are reshaped around that invariant:

- **Stall deadline (30s, `STALL_NOTICE_DELAY`).** If the answer is still absent, print
  one dim notice that the command output is held and that Ctrl+C releases it — and
  **do not flush**. The held output stays buffered until the answer finishes, the fuse
  fires, or the user presses Ctrl+C. The user decides when the answer and command
  output meet; koshell never mixes them on its own. Wording adapts to whether anything
  is actually held.
- **Size fuse (256 KiB, `PTY_BUFFER_FUSE_BYTES`).** When the held output reaches the
  fuse, release it as **one labeled block** behind a dim boundary notice, then keep
  buffering. If the block was released mid-answer, the next delta reprints the
  `[koshell ai]` header so the resumed answer is relabeled. The result is alternating,
  self-identifying blocks — answer, boundary + command-output block, relabeled answer —
  bounded in memory by the fuse per cycle.
- **Every release seam carries a boundary.** The fuse release, the Ctrl+C interrupt
  (`answer interrupted (^C); releasing held command output`), and the stall notice are
  all dim `[koshell]` lines that separate answer text from resumed command output. The
  normal, fast completion path is unchanged: the small returning prompt flushes after
  the answer with no extra boundary, so the common case stays quiet.

## Invariant change

Design 0002 stated: _a hung daemon can never freeze the terminal_, enforced by the 30s
force-flush. That is revised to: **a hung daemon can never freeze the terminal
_silently_.** The held output is no longer force-flushed on a timer; instead the stall
notice always tells the user the output is held and that Ctrl+C releases it, and memory
is bounded by the fuse's block release rather than by giving up buffering. The terminal
may sit on the stall notice until the answer arrives, the user presses Ctrl+C, or the
held output reaches the fuse — always with a visible, actionable recovery path.

## Mechanics (`crates/koshell-rs/src/presentation.rs`)

- `ActiveResponse` drops `interleaved` (buffering is never permanently abandoned) and
  gains `stall_notice_shown` (stall notice fires once) and `resume_header_pending` (the
  next delta reprints the header after a mid-answer block release). `holding_pty()` is
  therefore true for the whole stream response.
- `release_held_block()` takes the held bytes, prints the boundary notice, writes the
  block, records it to the mirror, and arms `resume_header_pending` when mid-answer.
  `pty_output` calls it at the fuse and keeps buffering; `poll` calls only the stall
  notice at `STALL_NOTICE_DELAY` (no flush); `next_deadline` stops scheduling the hold
  once the stall notice has fired (the fuse is event-driven).
- `user_interrupt` folds the boundary into the interrupt notice when output is held.
  `finish` is unchanged.

The single processor thread (`session.rs`) serializes deltas against PTY output, so a
block release is atomic with respect to `ai_delta`s — no lock is needed to "pause" the
answer during a release.

## Open issues

- Under a sustained heavy producer (e.g. a typed-ahead `yes`), option-A alternation
  degrades to 256 KiB command-output blocks interleaved with short answer bursts. It is
  bounded and self-labeled, but coarse; a live-producer hand-off (design 0002's
  command-still-running block mode) would read better and is deferred.
- If the answer never arrives and no new command output accumulates, the terminal rests
  on the stall notice until the user presses Ctrl+C. Accepted: the notice states the
  recovery path, and this is the deliberate trade for never mixing the two streams.

## Verification

- `presentation.rs` unit tests: `pty_buffer_fuse_releases_a_block_and_keeps_buffering`
  (block release then re-buffering, no interleave), `fuse_flush_relabels_the_resumed_answer`
  (answer → boundary block → relabeled answer, in order), `max_hold_holds_and_prompts_ctrl_c`
  (stall notice holds the output, `next_deadline` goes quiet, Ctrl+C releases behind a
  boundary), and the updated `interrupt_flushes_the_held_prompt_in_buffered_stream_mode`.
- `cargo test`, `cargo clippy --all-targets`, `cargo fmt --check` pass.
- Manual real-PTY smoke: a stalled answer holds the prompt and offers Ctrl+C; flooding
  over 256 KiB of typed-ahead output yields an emergency notice, one command-output
  block, then a re-headered answer — never line-interleaved.

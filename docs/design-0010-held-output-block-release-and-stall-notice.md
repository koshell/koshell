# Design 0010 — held-output block release and the stall notice

Date: 2026-07-08 12:19:29 CST

Status: implemented.

Revised 2026-07-28: the stall notice is now conditional on output actually being held.
Its no-output wording (`still no answer — press Ctrl+C to stop waiting for the AI`) was
noise — it repeated what the one-second waiting notice already said, and Ctrl+C already
stops a streaming answer, so it announced nothing new. It is removed. The held-output
wording is unchanged and still fires, because it is what carries the invariant below.
See "Conditional stall notice".

Revised 2026-08-01: the 30-second deadline is now measured from the most recent
meaningful AI progress — dispatch, an `ai_delta`, or visible tool activity — rather than
from dispatch unconditionally. The fixed dispatch-age deadline mislabeled every
non-anchored response longer than 30 seconds as `still no answer`, even while text was
visibly streaming. See "Progress-based stall deadline".

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

- **Stall deadline (30s, `STALL_NOTICE_DELAY`).** After 30 seconds without meaningful
  AI progress, print one dim notice that the command output is held and that Ctrl+C
  releases it — and **do not flush**. Dispatch, every `ai_delta`, and visible tool
  activity count as progress. The held output stays buffered until the answer finishes,
  the fuse fires, or the user presses Ctrl+C. The user decides when the answer and
  command output meet; koshell never mixes them on its own. The notice is conditional
  on something actually being held.
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
  notice after `STALL_NOTICE_DELAY` without progress (no flush). `last_progress_at`
  starts at dispatch and is refreshed by `on_delta` and visible `on_tool_activity`
  events; `next_deadline` schedules from it and stops scheduling an already-expired
  instant to avoid a zero-length receive spin.
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

## Conditional stall notice

Revised 2026-07-28.

The stall notice originally fired on a pure timer, with wording that adapted to whether
anything was held. The no-output branch was removed: it fired 30 s into any slow answer
and told the user only that the answer was slow (already covered by the one-second
waiting notice) and that Ctrl+C stops it (already true and already documented by the
interrupt path). It made a working-but-slow provider look like a fault.

The invariant is unchanged — **a hung daemon can never hold command output silently** —
because it was only ever carried by the held-output branch. That branch fires from two
places:

- `poll` at the current progress deadline, when output is already held (the common case:
  the returning prompt arrives right after `command_end`);
- `pty_output`, when the current progress deadline has already passed and this write is
  the first thing actually held. `ActiveResponse::stall_deadline_passed` is the shared
  predicate.

`next_deadline` stops scheduling the current stall deadline once it is in the past rather
than once the notice has fired. Without that guard the notice's held-output precondition
would leave `stall_notice_shown` false past the deadline, and
`saturating_duration_since` would hand the processor a zero-length channel wait — a
spin. A later AI delta or visible tool event moves the progress deadline into the future
and re-arms the clock; otherwise the fuse and the first late held output remain
event-driven through `pty_output`.

## Progress-based stall deadline

Revised 2026-08-01 12:24:10 CST (+0800).

The dispatch-age deadline confused response duration with response inactivity. On the
integrated-shell path, `command_end` dispatches before the returning prompt, so that
prompt is normally held for the complete answer. Any answer that lasted more than 30
seconds therefore met the old predicate, regardless of whether deltas were arriving.
The message said `still no answer` while the answer was visibly streaming.

`ActiveResponse::last_progress_at` now starts at dispatch and advances on every delta
for the active request and every visible tool-activity event. The stall notice fires
only when held output exists and 30 seconds have passed without either kind of progress.
Its wording is correspondingly factual:

```text
no AI progress for 30 seconds — press Ctrl+C to stop the AI and release the held command output
```

The held-output invariant and recovery behavior are unchanged. A genuinely silent
provider still produces the notice after 30 seconds; an answer that pauses for 30 seconds
mid-stream does too. A long answer that keeps producing deltas does not.

## Verification

- `presentation.rs` unit tests: `pty_buffer_fuse_releases_a_block_and_keeps_buffering`
  (block release then re-buffering, no interleave), `fuse_flush_relabels_the_resumed_answer`
  (answer → boundary block → relabeled answer, in order), `max_hold_holds_and_prompts_ctrl_c`
  (stall notice holds the output, `next_deadline` goes quiet, Ctrl+C releases behind a
  boundary), and the updated `interrupt_flushes_the_held_prompt_in_buffered_stream_mode`.
- 2026-07-28 revision: `a_stalled_answer_holding_nothing_stays_silent` (the waiting
  notice fires, the stall deadline passes silently with nothing held, `next_deadline`
  goes quiet rather than returning zero, then the first late PTY write reports the hold
  without flushing it, once).
- 2026-08-01 revision: `streamed_delta_resets_the_stall_deadline` and
  `visible_tool_activity_resets_the_stall_deadline` cross the original dispatch-based
  threshold without a notice, then verify that 30 seconds of genuine inactivity still
  reports the hold. `max_hold_holds_and_prompts_ctrl_c` covers silence from dispatch and
  asserts the new progress-based wording.
- `cargo test`, `cargo clippy --all-targets`, `cargo fmt --check` pass.
- Manual real-PTY smoke: a stalled answer holds the prompt and offers Ctrl+C; flooding
  over 256 KiB of typed-ahead output yields an emergency notice, one command-output
  block, then a re-headered answer — never line-interleaved.

# Fix 0003 — thinking pause before Enter fired `#?` before the program's echo

Date: 2026-07-04 18:44:13 CST

## Why

Dogfooding in a Python REPL: after typing `#? question` and pressing Enter, the
newline did not appear. The cursor stayed at the end of the question line until the
`[koshell] waiting for the AI answer…` notice showed up ~1 s later — and that line
break came from the notice's `line_prefix`, not from python.

## Root cause

Stabilization quiescence was measured from `last_output_at` alone. At the Enter
instant, the last PTY output is the echo of the user's own final keystroke — so any
thinking pause before pressing Enter (over the 150/500 ms in-program tiers) already
satisfied the quiescence condition. The chain:

1. `record_input` captures the pending question, and `poll()` in the same processor
   iteration finds it "stabilized" — the fire happens before python's response to
   the Enter (the `\r\n` echo and the next `>>> `) has arrived.
2. `begin_response` samples the cursor mid-line on `>>> #? question` — not a
   resting prompt — so anchored streaming does not engage and the response runs in
   buffered stream mode.
3. Python's `\r\n>>> ` echo arrives milliseconds later and is buffered behind the
   response. The screen looks frozen at the end of the question line.
4. At +1 s the waiting notice prints via `notice()`, whose `line_prefix` supplies
   the `\r\n` the user finally sees.

The e2e never caught it because the test driver pauses only 300 ms between typing
the question and pressing Enter — below the 500 ms tier, so quiescence was not yet
satisfied at the submit instant.

## The fix

Quiescence is measured from the later of the last PTY output and the question's
submission (`quiet_from` in `trigger.rs`, used by both `poll` and `next_deadline`):
a question can only stabilize after `tier` of post-submit silence, giving the
program a chance to respond to the Enter first.

- Normal REPL: the echo arrives within milliseconds of the submit, resets the
  clock, and leaves the cursor on a genuine `>>> ` prompt — the question fires
  ~150 ms later with anchored streaming engaged, so the newline appears instantly
  (from the program itself) and the waiting notice inserts above the live prompt.
- Silent/broken program: nothing follows the Enter, the question fires at
  `submitted_at + tier` with the cursor genuinely mid-line, and koshell creates
  the line break itself — the intended last resort only.
- Prompt-line origin (non-integrated shells) gets the same clamp; it only delays
  firing to at least `submit + tier`, which the conservative prompt-line tiers
  already intended (the authoritative `command_end` marker should win the race).

When no output was ever recorded, quiescence falls back to the submission instant
(previously such questions could only fire via max-wait; the case is practically
unreachable because capturing a question requires its echo).

## Verification

- New unit tests: `thinking_pause_before_enter_does_not_fire_before_the_echo`
  (2 s pause, no fire at submit, fires after the echo settles) and
  `silent_program_still_fires_a_tier_after_submission` (no response to Enter,
  fires at `submit + tier`).
- Existing trigger/presentation/PTY suites unchanged and green: `cargo fmt
--check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `pnpm check`.

## Residuals

- Firing now trails the submit by at least one tier even when the pre-submit
  screen was already quiet; in exchange the context package always includes the
  program's response to the submitted line. The tiers remain dogfooding-tunable.

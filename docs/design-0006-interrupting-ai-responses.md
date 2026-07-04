# Design 0006 — interrupting AI responses with Ctrl+C

Date: 2026-07-04 18:06:06 CST

Status: implemented.

## Why

Dogfooding surfaced the problem on the anchored-streaming path (a `#?` asked at a
REPL prompt, design 0005): once a response started streaming, nothing could stop
it. Ctrl+C only cancelled _pending_ questions (`trigger.rs`); a dispatched request
had no abort path anywhere:

- The presentation layer had no way to stop an `ActiveResponse` — and worse, a
  forwarded Ctrl+C made the REPL print `KeyboardInterrupt` plus a fresh prompt,
  which tripped the anchored invariant check and degraded the rest of the answer
  to a block dumped at the end. The interrupt made the experience worse, not
  better.
- The IPC protocol had no cancel message, so the daemon kept generating (wasted
  tokens) and its FIFO queue stayed blocked — an interjected follow-up `#?` had
  to wait behind the doomed answer.

## Semantics

One sentence: **Ctrl+C stops whatever currently occupies the terminal; while an
AI answer is streaming onto an idle prompt, the answer is that thing.**

| State                                                        | Ctrl+C routing                                                 | AI request                                                         |
| ------------------------------------------------------------ | -------------------------------------------------------------- | ------------------------------------------------------------------ |
| Stream-mode response in flight (anchored or buffered-prompt) | Swallowed by koshell (forwarded after all if the race is lost) | Rendering stops immediately + best-effort `ai_cancel`              |
| Command still running (block mode in flight)                 | Forwarded to the program, as always                            | Withdrawn (same semantics as the existing pending-question cancel) |
| Alternate screen                                             | Forwarded untouched                                            | Unaffected                                                         |
| No AI activity                                               | Forwarded untouched                                            | —                                                                  |

Rationale for the two contested rows:

- **Stream mode swallows.** The triggering command has ended, so no foreground
  program is waiting for the interrupt — the user's intent is unambiguous.
  Forwarding would reach the shell/REPL line editor and discard whatever the user
  typed ahead on the live input line, betraying exactly what anchored streaming
  exists to protect, and would add `KeyboardInterrupt` noise. After the response
  stops, Ctrl+C passes through untouched again.
- **Block mode forwards and also withdraws.** Killing the foreground program is
  inviolable, so the byte always reaches the child. The in-flight answer is
  withdrawn as well, for consistency: Ctrl+C already withdraws _pending_
  questions ("a user-typed interrupt withdraws the line's future output");
  whether the stabilization race happened to dispatch the request milliseconds
  earlier must not change what the interrupt means. Recovery is cheap — re-ask
  after the kill; the killed command's output is in the context.

## Local stop is authoritative; the daemon cancel is best-effort

The terminal stops rendering the instant the processor sees the interrupt; the
`ai_cancel` sent to the daemon only stops generation (tokens) and unblocks the
FIFO queue (interjected questions). This split is structural, not an
optimization:

- Deltas already in flight (socket buffers, the IPC reader thread, the processor
  channel) cannot be recalled by any daemon-side action. The only
  press-equals-stop implementation is dropping messages locally from the
  interrupt onward. Suppression runs on the single processor thread, so it is
  naturally ordered against input: deltas queued before the Ctrl+C still render,
  everything after is dropped.
- The moment the user most wants Ctrl+C is when the daemon is hung — precisely
  when a confirmation round-trip would never arrive. The interrupt's reliability
  depends only on `koshell-rs`, matching the graceful-degradation principle.

Mechanics (`presentation.rs`): `user_interrupt()` takes the active response,
moves its id into an `aborted` set, and renders the closing ink (anchored: a dim
`answer interrupted (^C)` above the live line; buffered stream: tail + notice +
flush of the held prompt; block: a `answer cancelled (^C)` notice, accumulated
text dropped). Late `ai_delta`s for an aborted id are dropped silently and the
daemon's terminal marker completes the bookkeeping, keeping the
one-terminal-marker contract intact.

## Input routing (`session.rs`)

The swallow reuses the bare-Esc window machinery: the processor maintains an
`interrupt_window: AtomicBool` (`presentation.owns_interrupt() && !alt_screen`),
the stdin thread extracts the first `0x03` from a chunk while the window is
armed and sends `Msg::Interrupt` instead of forwarding it. If the processor then
finds no active response (the answer finished in the race window), it restores
transparency: the swallowed byte is written to the PTY and treated as ordinary
input. A `0x03` that arrives as normal input (command running, or a stale
window) also withdraws the in-flight response via the same `user_interrupt`
path.

`owns_interrupt()` is true only for stream-mode responses; block mode never
claims the key.

## Protocol (`koshell-proto`, daemon)

New terminal → daemon message: `ai_cancel { request_id }`. Additive evolution —
no version bump; a daemon that predates it ignores the line, which degrades to
"generation runs out server-side, output suppressed locally anyway".

Daemon handling (`server.ts`): a cancelled id is remembered in a set; if it is
the currently running request, `agent.abort()` (pi's `session.abort()`) cuts the
in-flight prompt short and `run()` terminates it with its normal
`ai_response_end`. A request cancelled while still queued (or while the agent
was being created) is skipped without prompting, and still gets its
`ai_response_end` so the per-request contract holds. The set entry is cleared
when the request ends, so a cancel racing past its request's completion cannot
linger.

## Accepted residuals

- If Ctrl+C lands in the instant between the response completing and the
  processor noticing, the "answer interrupted" marker prints under an actually
  complete answer. Harmless mislabel; not worth extra state.
- A cancel can hit the microtask window between the daemon's post-create
  cancelled-check and pi actually starting the prompt; the abort is then a no-op
  and the answer generates (suppressed locally). Vanishingly narrow, bounded
  cost.
- The interrupt aborts only the currently rendering response. Questions still
  pending terminal-side are cancelled by the existing Ctrl+C path when the byte
  is forwarded, or left alone when it is swallowed. With the bare-Esc cancel
  removed (see below), there is no way to withdraw a queued question without
  Ctrl+C's side effects — accepted; the cost is one unwanted, ignorable answer.
- The partial assistant turn stays in the daemon-side pi conversation; the next
  answer may reference it. Acceptable for the prototype; revisit if it confuses
  real use.

## Bare-Esc cancel removed (2026-07-04)

Once Ctrl+C owned interruption, the bare-Esc pending-cancel path (design 0001,
2026-07-02) lost its reason to exist and was removed in the same dogfooding wave:

- Ctrl+C covers every cancel scenario but one — withdrawing a question about a
  still-running command without killing the command. That niche is not worth a
  dedicated key: the regretted question just fires and yields one ignorable
  answer, the same bounded worst case accepted for quote-parity misses.
- The Esc swallow was the product's most intrusive input interception: it armed
  whenever a question was pending — exactly the moment after submitting `#?`,
  when vi-mode line editor users habitually press Esc. Undiscoverable benefit,
  real mental-model conflict.
- Removal deletes the 40 ms disambiguation timeout, the `esc_window` state, and
  the lost-race Esc re-forward from the stdin hot path, and frees Esc as a
  candidate binding for the future non-echoing-program entry hotkey.

ESC is now forwarded unconditionally, in every state. Revisit with a
non-conflicting binding only if dogfooding shows real demand for
question-withdrawal-without-interrupt.

## Verification

- `presentation.rs` unit tests: anchored abort keeps the live prompt last and
  drops the late tail; pre-first-delta abort; buffered-stream abort flushes the
  held prompt; block withdrawal drops the accumulated text; no-op without an
  active response; the next question streams normally after an abort.
- Real-PTY e2e (`tests/anchored_stream_pty.rs`): Ctrl+C mid-stream in a python
  REPL — the fake daemon receives `ai_cancel`, the tail chunks never render, no
  `KeyboardInterrupt` reaches python, and the REPL line stays usable.
- Daemon tests: `ai_cancel` aborts the running ask and still ends the request;
  a queued cancelled request is skipped without prompting; protocol parse tests
  both sides.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo
test`, `pnpm check` all pass.

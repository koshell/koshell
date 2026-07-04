# Fix 0004 — flaky CI assertion on the interrupt test's chunk count

Date: 2026-07-04 20:48:16 CST

## Why

`ctrl_c_mid_stream_stops_the_answer_and_cancels_daemon_side` (the real-PTY e2e for
design 0006) failed on GitHub CI with `chunk-12` on screen, while passing locally.
The failing screen showed correct behavior: chunks 0–12, then the
`answer interrupted (^C)` notice, then a working REPL line.

## Root cause

The test asserted `!screen.contains("chunk-12")` — an absolute chunk index. That
encodes a wall-clock assumption: Ctrl+C is written 2000 ms after Enter, the fake
daemon emits a chunk every 150 ms starting immediately after `ai_request`, so the
index of the last rendered chunk depends on how quickly the stabilization fired
and the IPC round trip completed. On a fast CI runner that pipeline finished in
under ~200 ms, `chunk-12` (sent at stream start + 1800 ms) landed just before the
Ctrl+C, and the assertion tripped on a perfectly correct run.

The interrupt semantics themselves cannot produce a late tail: the stdin thread's
`Msg::Interrupt` and the daemon's `ai_delta` messages flow through the same
single-consumer processor channel, and `user_interrupt` moves the request to the
aborted set so later deltas are dropped.

## The fix

Test-only. The assertion now checks the property design 0006 actually guarantees —
order, not count: once the `answer interrupted (^C)` notice is on screen, no
`chunk-` text may appear after it. The chunk-0 presence, daemon-side `ai_cancel`
receipt, swallowed-Ctrl+C (no `KeyboardInterrupt`), and REPL-usability assertions
are unchanged.

## Verification

- `cargo test -p koshell-rs --test anchored_stream_pty` green locally; `cargo fmt
--check` and `cargo clippy --all-targets -- -D warnings` clean.

## Residuals

- The test still assumes chunk-0 renders before the 2000 ms Ctrl+C (stabilization
  plus IPC under ~1.85 s). That margin is wide; if it ever flakes the other way,
  the fix is to gate the Ctrl+C step on observed output rather than a fixed delay.

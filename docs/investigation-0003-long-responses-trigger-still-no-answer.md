# Investigation 0003 — long responses trigger `still no answer`

Date: 2026-08-01 11:39:28 CST (+0800)

Status: fixed on 2026-08-01 12:24:10 CST (+0800).

## Requirement

Explain why Koshell now prints `still no answer` for almost every long AI response, including responses that are visibly streaming normally.

## Finding

The notice is controlled by total response age, not by absence of answer progress.

For a non-anchored stream response, `ActiveResponse::stall_deadline_passed` becomes true exactly 30 seconds after dispatch. Receiving and rendering `ai_delta` messages does not move or cancel that deadline. If any PTY output is buffered at the deadline, `Presentation::poll` prints:

```text
still no answer — press Ctrl+C to stop the AI and release the held command output
```

On the integrated-shell `command_end` path, the buffered PTY output is normally the returning shell prompt. Therefore the conditions reduce in practice to:

1. the request uses non-anchored stream mode;
2. the shell prompt has arrived and is being held behind the answer; and
3. `ai_response_end` has not arrived within 30 seconds of dispatch.

A long response commonly satisfies all three. The notice can consequently appear in the middle of healthy, visible streaming. Its wording describes “no answer”, but the predicate actually means “the response has not completed and some PTY output is held”.

The direct cause is in `crates/koshell-rs/src/presentation.rs`:

- `STALL_NOTICE_DELAY` is a fixed 30 seconds.
- `stall_deadline_passed` compares only `now` with `dispatched_at + STALL_NOTICE_DELAY`.
- `on_delta` records `first_delta_at` and `delta_count`, but does not update the stall deadline.
- `poll` requires `holding_pty`, the expired deadline, and non-empty `buffered_pty`; it does not require `nothing_rendered()` or a period without deltas.
- `finish` is the operation that ends the response and flushes the held prompt. Until it receives `ai_response_end` or `ai_error`, the original dispatch deadline remains active.

The daemon is not synthesizing this notice. `packages/ai-daemon/src/server.ts` forwards each pi `text_delta` as `ai_delta`, awaits the complete `agent.ask`, and then sends `ai_response_end`. The 30-second classification is entirely terminal-side.

## Implemented resolution

`ActiveResponse` now stores `last_progress_at`, initialized to the dispatch time. Every
`ai_delta` for the active request and every visible tool-activity event updates it.
`stall_deadline_passed` and `next_deadline` now measure 30 seconds from that timestamp.

Crossing 30 seconds of total response age therefore does nothing while progress
continues. If the response later makes no progress for a full 30 seconds and PTY output
is held, the same Ctrl+C recovery path is shown with accurate wording:

```text
no AI progress for 30 seconds — press Ctrl+C to stop the AI and release the held command output
```

No IPC or daemon change was required. The held-output block separation, size fuse,
one-notice limit, and Ctrl+C release semantics remain unchanged.

The tool-activity handler also gained a borrowed `ToolActivity` payload that groups the
four protocol fields. This keeps progress tracking, active-request matching, rendering,
and event logging inside one lifecycle handler without adding an eighth parallel
argument or suppressing Clippy's interface warning.

## Why response length correlated with the notice

The actual variable is wall-clock time from dispatch to response end, not answer length. Longer answers usually take longer to generate and are therefore more likely to cross 30 seconds. A short answer preceded by more than 30 seconds of thinking can also trigger it, while a long answer generated in under 30 seconds will not.

The integrated-shell presentation path strengthens the correlation: `command_end` dispatches the request before the returning prompt is shown, so the prompt gives `buffered_pty` the non-empty value required by the notice. The prompt is ordinary expected output, not evidence that the provider has stalled.

Anchored streaming is exempt because it does not hold PTY output. This is why a similarly long response dispatched onto an already-rendered REPL prompt need not show the notice.

## Local evidence

A metadata-only analysis of `~/.local/share/koshell/events.jsonl` was performed without printing question or answer text.

- 26 completed responses were present; 23 had status `ok`.
- 21 successful responses were non-anchored stream responses.
- Five successful responses lasted at least 30 seconds; all five were non-anchored stream responses.
- One of those five did not receive its first delta until 55.299 seconds, so the 30-second wording was accurate for that case.
- The other four received their first delta at 17.645–19.987 seconds and continued successfully. By the 30-second deadline they had already been visibly streaming for 10.013–12.355 seconds, yet the same `still no answer` predicate applied.

The event log does not record `buffered_pty` or the stall notice itself, so it cannot independently prove which historical responses displayed the notice. The source predicate, the integrated-shell prompt behavior, the user's observation, and the focused characterization test below jointly establish the result.

## Validation

`cargo test -p koshell-rs presentation::tests` passed all 45 presentation unit tests.

A temporary focused characterization test then reproduced the exact behavior:

1. dispatch a non-anchored stream response;
2. buffer the returning prompt;
3. render an `ai_delta` at 2 seconds;
4. call `poll` just after 30 seconds; and
5. assert that output contains both the streamed answer text and `still no answer`.

The test passed. It was removed after characterization because permanently asserting the undesirable behavior would lock in the defect. The source tree was restored before this document was written.

The existing `max_hold_holds_and_prompts_ctrl_c` test verifies the intended 30-second held-output notice, but it does not send a delta before the deadline. The test suite therefore validated the hold invariant without detecting that active streaming was mislabeled as absence of an answer.

Post-fix tests add `streamed_delta_resets_the_stall_deadline` and
`visible_tool_activity_resets_the_stall_deadline`, and strengthen the genuine-inactivity
case with the new wording. All 47 presentation tests pass. Full repository validation
also passes: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo
test`, and `bun run check`.

## Options considered

### Progress-based stall deadline — selected

Track the last meaningful AI progress time, initialized at dispatch and updated by every `ai_delta` and visible tool-activity event. Emit the held-output notice only after 30 seconds without progress.

This makes “stall” describe inactivity rather than total answer duration while preserving design 0010's invariant that held output cannot remain silent behind a genuinely unresponsive answer. `next_deadline` is re-armed from the updated progress timestamp and retains its no-zero-spin behavior after expiry.

A single factual message covers both no first delta and a mid-response pause while retaining the Ctrl+C recovery instruction.

### Completion-age notice with accurate wording — not selected

Keep the current fixed dispatch deadline but change the message to say that the AI response is still running and shell output remains held.

This is the smallest behavior change and makes the statement true, but every response longer than 30 seconds will remain interrupted by a notice. It addresses misleading wording rather than the reported noise.

### Increase the fixed delay — not selected

Raise `STALL_NOTICE_DELAY`.

This reduces frequency but preserves the same classification error at a later threshold. It also delays recovery guidance for a genuinely hung response.

### Classify the held prompt separately — not selected

Suppress the notice when the only held bytes appear to be a returning prompt, and retain it for additional command output.

This targets the common false-positive precondition, but prompt recognition from raw terminal bytes is heuristic and could hide typed-ahead command output. It is materially more complex than measuring AI progress directly.

## Open issues and resolution conditions

- No known correctness issue remains within this fix's scope. Unit coverage verifies both sides of the boundary: deltas and visible tool activity reset the deadline, while 30 seconds without progress still reports non-empty held output and preserves Ctrl+C release behavior.
- The dogfooding event schema still cannot count stall notices directly. This is not required for correctness. If future tuning requires measured notice frequency, add a metadata-only presentation event recording whether a first delta had rendered; do not log answer or PTY content.

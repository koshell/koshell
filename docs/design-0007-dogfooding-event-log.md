# Design 0007 — the dogfooding event log

Date: 2026-07-04 21:10:31 CST

Status: implemented.

## Why

The dogfooding experiment (two to four weeks of daily personal use, judged by
whether the owner keeps reaching for `#?`) needs numbers, not impressions.
Three metrics decide what gets scheduled next:

- **Degrade frequency** — how often anchored streaming falls back to a block.
  This gates whether quiescence-gap insertion and re-anchor-after-degrade are
  worth building.
- **Mid-stream typing usage** — whether the user actually types on the live
  line while an answer streams. This is the behavior anchored streaming exists
  to protect; if it never happens, the invariant machinery is over-engineered.
- **Latency distributions** — submit → dispatch → first delta → response end.
  No latency budget exists yet; these distributions are how one gets set.

None of these can be reconstructed after the fact from the human-oriented
debug log, and asking the owner to remember is how dogfooding produces
fiction. So `koshell-rs` writes a local, append-only JSONL event log.

This is a dogfooding instrument, **not product telemetry**: nothing is ever
uploaded, and turning it into telemetry would be a separate, opt-in decision.

## Semantics

### The privacy invariant

**The event structs have no field that could carry screen content or PTY
bytes.** The log records metadata plus the question text — nothing else. The
guarantee is structural, not procedural: the only free-text fields in the
whole schema are `question` (text the user typed after `#?`), `request_id`,
and `shell` (the program name). Screen snapshots, PTY output, input bytes,
AI answer text, daemon error text, the command text left of an inline `#?`,
and the context package have no place to go, so no future call site can leak
them by accident.

The tool events (added with design 0021) are the first fields the terminal
does not author itself: `tool_name`, `phase`, and `code` arrive over the
socket. They pass through `identifier()`, which writes a value through only
when it is a non-empty lowercase `[a-z0-9_]` name of at most 32 characters
and records `other` otherwise. A daemon that grows a new tool still logs its
name; a malformed or oversized value can carry nothing. This keeps the
invariant structural rather than a promise about how the daemon behaves.

### Envelope

One JSON object per line, appended to
`$XDG_DATA_HOME/koshell/events.jsonl` (fallback
`~/.local/share/koshell/events.jsonl`):

```json
{"ts": 1751634631000, "session": "koshell-12345", "event": "...", ...}
```

`ts` is wall-clock epoch milliseconds stamped at emit time; `session` is the
same `koshell-<pid>` id the IPC hello uses, pairing log lines with daemon
logs and making `request_id` (a per-session counter) globally unique.

### Events

| Event                | Fields                                                                                                                                                                                                  |
| -------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `session_start`      | `shell`, `integrated`, `cols`, `rows`, `version`                                                                                                                                                        |
| `session_end`        | `exit_code`, `duration_ms`                                                                                                                                                                              |
| `question_submitted` | `question`, `origin` (`prompt_line` \| `in_program`), `form` (`standalone` \| `inline`)                                                                                                                 |
| `question_cancelled` | `question`, `pending_for_ms` — Ctrl+C before the question fired                                                                                                                                         |
| `dispatched`         | `request_id`, `question`, `fire_reason` (`command_end` \| `stabilized` \| `max_wait`), `still_running`, `submit_to_dispatch_ms`                                                                         |
| `dispatch_failed`    | `request_id`, `question`, `fire_reason`, `reason` (`daemon_unavailable`)                                                                                                                                |
| `first_delta`        | `request_id`, `dispatch_to_first_delta_ms`, `mode` (`stream` \| `block`), `anchored`                                                                                                                    |
| `degrade_to_block`   | `request_id`, `reason` (`live_region_unavailable` \| `tail_mismatch`), `ms_since_dispatch`, `deltas_so_far`                                                                                             |
| `response_end`       | `request_id`, `status` (`ok` \| `error` \| `interrupted`), `total_ms`, `first_delta_ms`, `delta_count`, `began_anchored`, `degraded_to_block`, `mid_stream_input_chunks`, `tool_calls`, `tool_failures` |
| `tool_activity`      | `request_id`, `tool_name`, `phase` (`started` \| `failed`), `rendered` — one announcement from the daemon (design 0021)                                                                                 |
| `tool_call`          | `request_id`, `tool_name`, `ok`, `code` (present only on failure), `duration_us` — one call the terminal served itself (design 0020)                                                                    |

`first_delta` and `degrade_to_block` are standalone events rather than
`response_end` fields so they survive a hung daemon whose `response_end`
never arrives.

The two tool events answer different questions and neither subsumes the
other. `tool_activity` is the only record of a tool the daemon serves itself
— `web_search` reaches the terminal no other way — so it is what makes the
pull rate measurable at all; it is emitted even when the line rendered
nothing (`rendered: false`), because an announcement suppressed by a Ctrl+C
the user had just pressed is exactly the case being counted. `tool_call`
covers only the two command readers, and adds what the announcement cannot
know: whether the read succeeded and how long it took. Durations are
microseconds — this is an in-memory read on the processor thread, and a
millisecond field would record zero for every healthy call.

Metric mapping:

- Degrade frequency = `degrade_to_block` count over responses with
  `began_anchored` (or `first_delta.anchored` when the end never came).
- Mid-stream typing usage = share of anchored responses with
  `mid_stream_input_chunks > 0`, plus the chunk-count distribution.
- Latency = the `submit_to_dispatch_ms`, `dispatch_to_first_delta_ms`,
  `total_ms`, and `first_delta_ms` distributions.
- Pull rate = share of `response_end` with `tool_calls > 0`, and the
  `tool_calls` distribution (does the agent list-then-read once, or page
  ten times?).
- Interruption while working = `response_end` with `status: "interrupted"`
  and `tool_calls > 0`, plus `tool_activity` with `rendered: false` for
  calls announced after the withdrawal.
- Tool health = `tool_failures` per response, the `tool_call.code`
  distribution (is `output_evicted` routine? are the retention bounds too
  tight?), and the `duration_us` distribution against the bridge's
  5-second timeout.

Deliberate exclusions:

- **Daemon error text.** An `ai_error` becomes `response_end` with
  `status: "error"`. Error prose is daemon-generated free text; a status flag
  serves the metrics and keeps the free-text surface at exactly one field.
- **Per-keystroke or per-burst mid-stream events.** The analysis needs "did
  they type, roughly how much" — a counter of stdin read chunks on the active
  response, attached to `response_end`. Chunk count approximates bursts; the
  bytes themselves are never inspected beyond the existing Ctrl+C check.
- **`cwd`, hostnames, rotation.** Dogfooding volume is a handful of lines per
  question; a month is kilobytes.
- **Tool arguments and results.** A `read_command_output` call carries a
  command id and an offset, and its result is command output — the exact
  content the invariant exists to keep out. The code and the outcome answer
  the questions; the payload is never touched.
- **`tool_call_id`.** It joins a call to a daemon log line, which the daemon
  log already does by request id and tool name. Recording it would add a
  wire-sourced string for no metric.

## Decision

- New module `event_log.rs` in `koshell-rs`. `EventLog` is a cheap `Clone`
  handle over an `Option<mpsc::Sender>`; a dedicated writer thread owns the
  file and appends one line per event, flushing each (volume is per-question,
  not per-delta). Serialization happens in `emit()` on the emitting thread;
  the writer only writes.
- **Fail-silent.** If the directory cannot be created, the file cannot be
  opened, or a write fails, the log degrades to inert: one warning in the
  debug log, the shell is never disturbed, no render-path work is added.
  `emit()` on an inert handle is a no-op.
- **Clean shutdown.** `run_interactive_shell` drops its handle and joins the
  writer after the child is reaped, so `session_end` is drained before
  `main` exits the process.
- **Kill switch.** `KOSHELL_NO_EVENT_LOG=1` disables the log entirely,
  following the `KOSHELL_NO_AUTO` escape-hatch convention.
- Emit points live where the facts are known: `trigger.rs` owns
  `question_submitted` / `question_cancelled`, `presentation.rs` owns
  `first_delta` / `degrade_to_block` / `response_end` / `tool_activity` (the
  per-response bookkeeping already lives on `ActiveResponse`, and the
  announcement is a presentation decision), `session.rs` owns
  `session_start` / `session_end` / `dispatched` / `dispatch_failed` /
  `tool_call` (it is the thread that serves the call). The
  handle is injected with setters; a default-constructed `SessionState` or
  `Presentation` logs nothing, which keeps the existing unit tests untouched.
- Concurrent koshell sessions append to the same file; single-`write`
  appended lines of this size are effectively atomic on the local
  filesystems that matter, and `session` disambiguates the readers.

## Open issues

- `mid_stream_input_chunks` counts stdin read chunks, which approximates but
  does not equal typing bursts. Good enough to answer "does this happen at
  all"; refine only if the answer is "yes, a lot".
- A response whose request id was never dispatched in this process (stale
  daemon replies) can emit `first_delta` / `response_end` without a matching
  `dispatched` line. Readers should join on `request_id` leniently.
- No rotation or size cap. Revisit only if the log ever becomes telemetry —
  which is a separate opt-in decision, not an evolution of this file.

## Verification

- `event_log.rs` unit tests: XDG path resolution, envelope shape (tag,
  snake_case fields), emit round-trip through a captured channel, a
  successful `tool_call` omitting `code`, and `identifier()` reducing an
  empty, mixed-case, over-long, non-ASCII, or content-bearing value to
  `other`.
- `trigger.rs` unit tests: submission emits `question_submitted` carrying
  the question and **not** the command text left of an inline `#?`; Ctrl+C
  on a pending question emits `question_cancelled`.
- `presentation.rs` unit tests: anchored first delta, forced tail-mismatch
  degrade, interrupt, and mid-stream counting all land in the right events;
  a `finish` for an aborted or unknown id emits nothing; announcements are
  logged with their name and phase and counted onto `response_end` (starts
  and failures separately, so a retried search is not read as two pulls); a
  toolless response reports zero; an announcement for a withdrawn request is
  logged with `rendered: false`; and a tool name that is not an identifier
  is bounded before it is written.
- `command_output_tools_pty.rs`: the real-PTY acceptance run's own event log
  (`XDG_DATA_HOME` on a temp dir) carries both announcements with
  `rendered: true`, at least two successful `tool_call` lines with
  durations, a `response_end` counting them, and neither the sentinel nor
  any filler line anywhere in the file. The refusal case logs the failed
  call with `code: "unsupported_tool"` and no announcement.
- Real-PTY e2e (`tests/event_log_pty.rs`): the python-REPL anchored-stream
  flow with `XDG_DATA_HOME` pointed at a temp dir produces the ordered event
  subsequence `session_start → question_submitted → dispatched(stabilized) →
first_delta(anchored) → response_end(ok)` with sane latency fields, and —
  the privacy assertion — the raw file contains no prompt text, no command
  output, and no AI answer text. `KOSHELL_NO_EVENT_LOG=1` produces no file.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test`.

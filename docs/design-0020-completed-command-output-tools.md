# Design 0020 — read-only completed-command output tools

Date: 2026-07-28 16:40 CST +0800

Status: implemented.

## Why

Design 0002 split terminal context into "push the anchor, pull the exploratory tail",
but only the push arm existed. The consequence was reproduced in dogfooding on
2026-07-10: a command whose output ran past one screen produced an answer grounded only
in the currently visible portion, because there was no way for the agent to reach the
rest. The user's own summary of the gap is the requirement — the AI should be able to
see _the commands that ran and the complete output of each_, not just the current
screen.

Enlarging the pushed window is not the fix. The push is paid on every question, most of
which need none of it, and the screen is not the real bound — a `pnpm build` can emit
megabytes. What is needed is bounded _random access_: a cheap index the agent can
consult, and a pageable read for the one command that matters.

## What ships

Two read-only tools, and nothing else:

```text
list_recent_commands()                            → the 10 most recent completed
                                                    commands, newest first, no bodies
read_command_output(commandId, offset?, limit?)   → one page of that command's output
```

The whole reachable surface is those two functions (`command_tools.rs`). A tool name
outside the catalog is a structured error, not a dynamic dispatch. Nothing writes to the
PTY, runs a process, reads a file, or reaches another terminal session.

## Command identity

The store keys on one stable `command-N` id that spans a real `command_start` through
its matching `command_end`. That was not previously true: `handle_marker` allocated a
fresh id at _each_ marker, so a command's start and end carried different ids and no
span existed at all.

Three cases the state machine handles explicitly, because inventing a span is worse than
having none:

- a `command_end` with no open span creates no row;
- a new start while a span is open discards the incomplete capture with a logged
  warning;
- shell exit discards an unfinished span.

**Synthetic markers.** The precmd fallback emits a start/end pair for a comment-only
`#?` line so the trigger fires from an authoritative boundary even though the shell ran
nothing. Those pairs now carry `executed: false` and are trigger-only — otherwise asking
a question would itself appear in the command index, and the next prompt's bytes would
be attached to it. The field is absent-means-true, so a long-lived session whose rc
predates it keeps working.

## What is captured

Marker-clean PTY segments observed between the two boundaries — the same `Segment::Visible`
bytes the mirror consumes, so a koshell marker can never enter command output. The echoed
command line precedes `command_start` and the returning prompt follows `command_end`, so
neither is captured. Presentation (AI) output takes a separate mirror path entirely.

The retained form is `pty_text`: carriage returns and control sequences included. The
store does not claim to reconstruct rendered scrollback, and it does not attribute output
to a process — a background job writing to the same terminal lands inside the foreground
span, because a byte stream carries no such attribution. The tool descriptions and the
prompt both say so, so the agent does not over-claim.

Decoding is incremental across PTY chunk boundaries: a scalar split across two reads is
held until it completes, and a genuinely malformed sequence becomes U+FFFD rather than
discarding the surrounding text.

## Bounds

Memory-only, no disk, per terminal session:

- 10 completed commands' metadata;
- 1 MiB of retained text per command;
- 4 MiB across the active capture and all completed commands.

When a command exceeds its cap the **recent tail is kept** — the end of a command's
output is where the error usually is. When the session cap is crossed, output is
reclaimed from the oldest completed commands first while their **metadata rows survive**,
so a later list still shows that the command ran and that its output is gone.

Every result reports `totalBytes`/`totalCharacters`, `retainedBytes`/`retainedCharacters`,
`retainedStartOffset`, `droppedPrefixBytes`, `sourceTruncated`, and `available`. Absence
is never presented as complete output. Byte counters refer to the decoded text's UTF-8
encoding, not the original wire bytes; offsets count Unicode scalar values in that same
decoded text.

`offset` is **absolute** in the command's original output, so a page reference stays
meaningful after prefix reclamation. Requesting an offset older than the retained window
returns `output_evicted` naming the earliest available offset — a retry instruction, not
a dead end. `limit` defaults to 8,000 characters and clamps to 16,000.

This store is deliberately separate from the timeline. The timeline serves recent
contextual facts under a session-wide character budget; this serves one selected command
under per-command byte caps. They share a command id and nothing else.

## The round trip

```text
daemon → terminal:  ai_tool_call  { request_id, tool_call_id, tool_name, arguments }
terminal → daemon:  tool_response { request_id, tool_call_id, ok, result? | error? }
```

Additive on both sides; no protocol version bump. `arguments`, `result`, and
`error.details` cross as untrusted JSON and are validated at each runtime boundary.

The terminal's processor thread intercepts `ai_tool_call` before presentation — the
message itself is a data request, not display content — executes it synchronously
against the `SessionState` it already owns (that thread is the only writer, so the read
needs no lock and cannot race the PTY), and replies only on the connection the call
arrived on. What the _user_ sees comes from a separate announcement message; see
`design-0021-visible-tool-activity.md`.

Exactly one response settles a call. Unknown, duplicate, mismatched-request, and late
responses are dropped with a log line and no terminal presentation.

## Capability negotiation

The frozen `hello` gains an optional `capabilities` list. A terminal advertises
`command_output_tools_v1`; the daemon registers the two tools only for connections that
did.

This is what keeps a mixed-version fleet honest in both directions. A new daemon with an
old terminal stays push-only rather than issuing calls that can never be answered; a new
terminal with an old daemon has its extra field ignored. Unknown or malformed entries in
the list are dropped rather than rejecting the handshake — the hello shape is frozen, so
a capability the daemon cannot read must never cost the connection.

`noTools` flips from `"all"` to `"builtin"` only when a tool is registered, keeping pi's
own file, shell, edit, and write tools disabled either way. The observe-only boundary is
what `tools.ts` contains, not a pi default.

## Request bounds

Per `#?` request: at most 32 tool calls and 256,000 characters of serialized results,
with a 5-second timeout per call. These are request safety bounds, not retention bounds —
the API stays pageable across later questions, while one runaway agent turn cannot page
the whole retained megabyte into context by accident.

Every failure settles the call, because an unsettled call stalls the agent turn and
therefore the answer: timeout, caller abort (Ctrl+C), request end, and connection
disposal all resolve outstanding calls with a structured code. Codes are
`unsupported_tool`, `invalid_arguments`, `command_not_found`, `output_evicted`,
`request_inactive`, `budget_exceeded`, `timeout`, and `terminal_disconnected`. Each
becomes a normal pi tool result, so the agent can answer from the evidence it still has
rather than losing the turn.

## Making the agent actually pull

An agent skips a tool mostly because it does not know the material exists. Two mechanisms
address that, both from design 0002:

- **The inventory.** The pushed package advances to `koshell_ai_context_v2` and carries
  `pullContext.commandOutput` — availability, count, and the newest id — rendered into
  the prompt as a "Retrievable evidence" section. It is rendered only when there is
  something to fetch, so an empty index never invites a pointless round trip.
- **`primaryTextTruncated`.** The push trims from the start, so "this is all there was"
  and "the beginning is missing" previously looked identical. The flag is now computed
  per primary source and stated explicitly in the prompt, and the static rules key the
  pull decision on it: _when primaryTextTruncated is true, or the visible evidence does
  not contain the error being asked about, list and read before answering._ That turns
  pulling from a curiosity problem into an instruction-following one.

## Trust

Command text and command output are untrusted evidence in both the tool descriptions and
the system prompt: to be quoted and reasoned about, never followed as instructions.

Calling a tool sends the returned terminal content to the configured model provider.
Hidden (non-echoed) input is excluded by construction, but a secret typed visibly or
printed by a program cannot be reliably identified in this slice. Conditional tool use,
the response caps, no persistence, and metadata-only telemetry reduce exposure;
automatic redaction remains an open problem rather than a false guarantee.

Tool telemetry is metadata only. Each served call emits one `tool_call` event —
request id, tool name, outcome, failure code, and how long the read took in
microseconds — and nothing else: not the command id, not the offset, not a byte of
content. The wire-sourced name and code are bounded to short lowercase identifiers
before they are written, so the event log's privacy invariant stays structural rather
than a promise about the daemon (design 0007). Announcements are logged separately by
design 0021.

## Coverage limits

Reliable boundaries are promised only for the integrated outer bash or zsh shell. REPL
statements, commands typed over SSH, non-integrated shells, direct programs, and
non-terminating commands without a `command_end` are not indexed. The prompt states this,
so an empty list is not misreported as "no such command".

## Verification

- `command_history.rs` unit tests (21): span identity, output outside a span, the
  three state-machine cases, the 10/1 MiB/4 MiB bounds with tail retention and
  metadata survival, incremental UTF-8 across chunk boundaries, malformed bytes,
  scalar-aligned paging with no gaps or overlap, limit clamping, absolute-offset
  eviction, and command-text/preview caps.
- `command_tools.rs` unit tests (11): result shapes, unknown tool refusal, argument
  validation (including mistyped offsets rejected rather than coerced), synthetic
  markers creating no row, the returning prompt staying out of the span, and inventory
  advertising only what can be read.
- `tool-bridge.test.ts` (14): settle/duplicate/stale/unknown handling, timeout, abort,
  request end, disposal, and both per-request budgets.
- `protocol.test.ts`: capability list parsing (including malformed entries not costing
  the handshake), `tool_response` parsing, and a failure without a well-formed error
  rejected at the wire boundary.
- `prompt.test.ts`: the inventory section, `primaryTextTruncated` rendering, and the
  system prompt matching the registered tool set in both directions.
- `command_output_tools_pty.rs` (3): the acceptance case for bash and zsh — a sentinel
  outside both the pushed tail and the current screen, recovered by list-then-page —
  plus an unserviceable tool call refused over the wire without hanging the turn. Each
  run also asserts on its own event log: successful `tool_call` lines with durations
  for the acceptance case, a failed one carrying `unsupported_tool` for the refusal,
  and no command output in the file either way.
- Full `cargo fmt --check`, `cargo clippy --all-targets -D warnings`, `cargo test`, and
  `bun run check`.

## Open issues

- Only completed integrated-shell commands are covered. A still-running command's span
  cannot be fetched, which is exactly the case a "what is this build doing" question
  wants; the prompt states the limit rather than papering over it.
- Snapshots, timeline ranges, raw PTY queries, and previous-question anchors are still
  not pullable. The inventory deliberately advertises only what exists.
- Redaction is unspecified. A secret printed by a command can reach the model provider
  through a read.
- Conversation persistence is unchanged and still absent: the index dies with the
  terminal session alongside the conversation.
- The per-command 1 MiB and per-session 4 MiB bounds are dogfooding starting values.

## Resolution conditions

- Dogfood this slice before scheduling snapshot, REPL, SSH, watcher, or TUI retrieval;
  the narrow catalog exists so the next slice is chosen from evidence.
- Design redaction before any persistent storage of command output.
- Revisit the retention bounds if the `tool_call.code` distribution shows real sessions
  routinely hitting `output_evicted`.
- Reconsider still-running span access if dogfooding shows the completed-only limit is
  what questions keep hitting.

# Design 0021 — visible tool activity

Date: 2026-07-28 17:20 CST +0800

Status: implemented.

## Why

Designs 0019 and 0020 gave the agent tools, and both made the work invisible:

- `web_search` runs entirely inside the daemon. The terminal never sees a message for
  it, so a search was undetectable from the terminal's side by construction.
- `list_recent_commands` / `read_command_output` do cross the socket, but design 0020
  deliberately intercepted `ai_tool_call` _before presentation_ on the grounds that a
  data request is not display content.

The result was a `#?` that could sit silent for several seconds and several round trips.
From the user's chair a slow search, a paging loop, and a hung daemon look identical, and
the only recovery gesture — Ctrl+C — has to be decided blind. The stall notice would
eventually fire at 30 seconds, but that is a fault report, not an account of what is
happening.

The product owner's correction: this boundary should not exist. Tool calls should print
as dim text so the user knows what is happening and can decide whether to interrupt.

That is also the honest position for a tool that sends data off the machine. A web search
ships a model-authored query — possibly quoting the user's terminal — to a third-party
vendor. Printing the query is the difference between a capability the user has and one
that merely runs on their behalf.

## The message

```text
daemon → terminal:
  ai_tool_activity { request_id, tool_name, phase, message }
```

Additive; no protocol version bump.

Three decisions inside that shape:

- **The daemon announces every call, including the ones it serves itself.** It is the
  only participant that sees the whole tool loop — the terminal sees command reads and
  nothing else. One announcer means one code path and no gap where a tool family is
  silent.
- **`message` is ready-to-render display text, not a code the terminal formats.** A
  daemon that gains a tool then needs no matching terminal release, and an older
  terminal renders a newer tool's line correctly. The alternative (terminal-side
  wording keyed on `tool_name`) would make every new tool a two-sided change and would
  print "running some_new_tool" on any terminal built before it.
- **`phase` is a free-form string.** An unknown phase renders as an ordinary line
  rather than failing to parse, so a later `progress` phase costs nothing.

`tool_name` stays on the wire alongside the display text because it is the structured
handle telemetry and log filtering want — see "Measurement" below.

## What is announced

- `web_search` start carries the provider and the query, abbreviated to 96 characters:
  `searching the web (exa): brew shallow clone error`. The query is the point of the
  line — it is what tells the user whether the AI understood the question well enough
  to be worth waiting for, and it is what is leaving the machine.
- `read_command_output` start names the command id, and the offset when paging deeper:
  `reading the output of command-3 from character 8000`. That distinguishes progress
  from a loop.
- `list_recent_commands` start: `looking up your recent commands`.
- Failures are announced with their stable code. This includes failures the bridge
  synthesizes rather than receives — timeout, disconnect, budget — because silence
  after a start line would read as success.

Success is not announced. The answer that follows is the success signal, and a
start/finish pair per call would double the noise for no decision the user can act on.

## Presentation

The line renders as a dim, self-identifying `[koshell]` notice through the same helpers
as the existing receipt and stall notices. It is a seam in the answer, so design 0010's
rule applies unchanged — the answer and everything else stay in separate labeled blocks:

- **Non-anchored stream.** Print the notice; if the answer had already started, set
  `resume_header_pending` so the continuation gets a fresh `[koshell ai]` header. Same
  mechanism the fuse's block release uses.
- **Anchored stream.** Print the notice above the live input line, then **drop
  `ai_end`**. The row above the live region is now the notice rather than the AI tail,
  so the resume point is genuinely gone; clearing it makes the next delta reprint the
  header and start a fresh block below the notice. Without this the tail check in
  `anchored_delta` would see a mismatch and degrade the rest of the answer to one block
  at the end — meaning every tool-using answer on the REPL path would lose its
  streaming, which is a real regression once search is enabled.
- **Block mode** (command still running): print the notice, matching the existing
  receipt notice's behavior in that mode.

Showing the work counts as the receipt: the line sets `receipt_shown`, so the delayed
"waiting for the AI answer…" notice does not also fire and repeat what the tool line
already said.

A withdrawn request (local Ctrl+C) renders no further tool lines, and a line naming an
unknown or stale request id renders nothing — the same suppression deltas get.

## Interruption

No new gesture. Ctrl+C already withdraws the in-flight request and, on the daemon side,
aborts generation and rejects outstanding tool calls with `request_inactive`. What
changes is that the decision is now informed: the user can see it is a search on the
wrong query, or a fourth page of a command they did not mean, and stop it.

## Measurement

Whether this is worth its screen space is a dogfooding question, and it is unanswerable
from impressions: the interesting cases are a search on a wrong query the user killed,
and a paging loop they sat through. So the announcement is also the event log's only
sighting of a daemon-served tool, and each one emits `tool_activity`
(`tool_name`, `phase`, `rendered`) into the design 0007 log. `response_end` carries
`tool_calls` and `tool_failures`, counted separately so a retried search is not read as
two pulls — that makes "did this answer use tools, and how did it end" one line rather
than a join.

Two deliberate choices:

- **A suppressed line is still logged**, with `rendered: false`. An announcement that
  arrived just after a Ctrl+C is the interrupted-mid-call case, which is the thing being
  measured; dropping it would bias the count toward calls the user tolerated.
- **The wire fields are bounded before they are written.** `tool_name` and `phase` are
  the first event-log fields the terminal does not author, so they pass through
  `identifier()` and become `other` unless they are short lowercase identifiers. A new
  daemon tool still logs its own name; nothing else can ride in.

Failures are counted from the announcement, so a `web_search` timeout is measurable even
though that call never touches the terminal. The terminal-served readers additionally
emit `tool_call` with their outcome and duration; see design 0020.

## Privacy

The announcement is local presentation output. It carries no more than the tool call
already carried, and for `web_search` it makes an outbound transfer visible that was
previously silent. Like all presentation output it feeds the mirror (the mirror-feed
invariant), so screen snapshots stay truthful; it is not PTY output and does not enter
terminal text context.

The event log records the tool name, the phase, and whether the line rendered. The
message — which carries the search query and the command id — is not recorded.

## Verification

- `presentation.rs` unit tests: the line renders before any answer arrives and
  suppresses the waiting notice; a mid-answer line relabels the resumed answer (two
  `[koshell ai]` headers, in order); an anchored line re-anchors instead of degrading
  (the live prompt stays the last line and the answer continues); a withdrawn request
  and a stale request id both render nothing.
- `tools.test.ts`: the search query is announced with its provider before the request
  goes out, a long query is bounded, a failed search announces its code, command reads
  announce id and offset, bridge-synthesized failures are announced, and the
  no-announcer path does not throw.
- `presentation.rs` event tests: announcements are logged with their name and phase and
  counted onto `response_end` (starts and failures separately); a toolless response
  reports zero; an announcement for a withdrawn request is logged with
  `rendered: false` while drawing nothing; a tool name that is not an identifier is
  written as `other`.
- `command_output_tools_pty.rs`: the bash and zsh acceptance runs assert both tool lines
  actually appear in the terminal's byte stream, and that the run's own event log
  carries both announcements as rendered, the served calls with their durations, and a
  `response_end` counting them — with no command output anywhere in the file.
- Full `cargo fmt --check`, `cargo clippy --all-targets -D warnings`, `cargo test`, and
  `bun run check`.

## Open issues

- No duration is recorded for a daemon-served tool. `tool_call` times the terminal's own
  reads, but a `web_search` start is only bracketed by its failure or by the answer;
  a slow-but-working search is still indistinguishable from a stuck one on screen. A
  `progress` phase or a `finished` announcement would fix both at once — deferred until
  the log says long single calls actually happen.
- The lines are not rate-limited beyond the per-request call budget (32). An agent that
  pages 20 times prints 20 lines.
- The dim `[koshell]` style is shared with notices that report faults; a tool line is
  ordinary progress. Distinguishing them is part of the still-open AI-output style
  question from design 0002.

## Resolution conditions

- Revisit a `progress` phase or per-call timing if the `tool_calls` distribution shows
  responses making many calls, or if answers are being interrupted mid-tool.
- Settle the notice-versus-progress styling with the design 0002 output-style question.

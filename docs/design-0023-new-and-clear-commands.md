# Design 0023 — `koshell new` and `koshell clear`

Date: 2026-07-30 12:33:17 CST (+0800)

Status: implemented.

## Why

A conversation beside the shell accumulates two independent kinds of history, and until now
neither could be dropped on purpose:

- **What the AI remembers.** One pi-backed conversation is memoized per live terminal
  connection and persists across every `#?`. The only thing that ever reset it was
  `koshell reload`, whose real job is re-reading `koshell.toml` — resetting the transcript
  is its side effect. Asking a fresh, unrelated question therefore meant either dragging
  the whole earlier transcript along or reloading configuration that had not changed.
- **What the AI can see.** Every `#?` is grounded in terminal evidence koshell keeps on the
  user's behalf: the timeline (recent text, PTY output, screen snapshots) and the
  completed-command index behind the read-only pull tools (design 0020). Plain `clear`
  wipes the user's screen but none of that, so output the user believed they had erased
  stayed available to the next answer. That is a privacy surprise, not only a context one:
  the natural gesture for "forget what just happened here" silently did not.

The two are separate axes, so they get separate commands rather than one flag.

## The commands

```bash
koshell new      # discard the AI conversation; screen and terminal evidence untouched
koshell clear    # clear the screen, discard the terminal evidence, AND the conversation
```

`new` is "let's start over talking". `clear` is "forget this whole stretch of terminal" —
the `clear` gesture users already have, extended to cover everything koshell retained
about what was on that screen. `clear` deliberately subsumes `new`: evidence the AI can no
longer read but still remembers discussing is the worst of both worlds, and a transcript
citing output nobody can re-read invites confident answers about erased material.

Both are instance-scoped and must run inside a koshell-wrapped shell. Outside one they
fail with guidance rather than doing nothing quietly.

### Why CLI subcommands

They join `status`, `reload`, and `model` as ordinary subcommands. The mechanism already
exists and is well understood on both sides, the commands are typed at a prompt like their
neighbours, and completion, history, and `--help` come for free.

The alternative was the `#? /` session-command namespace design 0001 had reserved. Writing
these two commands is what settled that it should not exist, and design 0001 withdrew the
reservation the same day: pi's `/` is backed by a completion menu in a TUI pi owns, while
koshell's `#?` is typed into the user's own shell line editor — koshell forwards stdin
byte-for-byte and reads the rendered line only at the submit instant, so it can never
complete anything. A `/` namespace here would have been these same commands minus
completion, minus listing, and minus any error before Enter, bought by declaring questions
that start with a path unsupported. There is now one surface, not two to keep in sync.

The cost is name shadowing, the accepted residual every koshell subcommand carries: `clear`
is the first reserved name with a real namesake on every system, so `koshell clear` is
koshell's own clear and running the binary in a PTY needs a path form (`koshell ./clear`,
`koshell /usr/bin/clear`). Plain `clear` is untouched — koshell only reserves names after
its own.

## How a child process reaches the wrapper

`koshell clear` runs as an ordinary child of the inner shell, one level below the process
that owns everything it needs to change: the mirror, the timeline, the command index, and
the live daemon connection. It has no handle on any of them, and koshell has no per-session
control socket for children.

It uses the channel that is already there: the **OSC 777 control marker**. `koshell new`
and `koshell clear` write one `\x1b]777;koshell;<base64>\x07` sequence carrying
`{"type":"new_conversation"}` or `{"type":"clear_context"}`, the wrapper's `MarkerScanner`
splits it out of the PTY byte stream exactly as it does the shell hooks' command-boundary
markers, and `session::apply_control_marker` applies it from the one thread that owns the
state. Because the scanner strips control markers before anything else sees them, the
request never renders as garbage and never reaches the mirror.

Reusing the marker channel rather than adding a socket keeps the number of ways to talk to
the wrapper at one, and inherits its ordering guarantee for free: the request lands in the
output stream at exactly the point the command ran, so it cannot be applied before output
that preceded it.

**Written to `/dev/tty`, not stdout.** The marker is a request addressed to the terminal,
not output. On stdout, `koshell clear > log` would drop the request into a file (with a
stray escape sequence in it) and silently do nothing; `/dev/tty` reaches the wrapper under
any redirection. Nothing goes to stdout at all, so both commands stay quiet in pipelines.

**Delivery is one-way.** The exit code reports that the request was _sent_; the wrapper's
dim notice is the outcome. Two-way would mean inventing a reply channel from the wrapper
back to a child process that is about to exit, to carry a result the user cannot act on
anyway — they asked for the reset.

### Trust boundary

Any program that can write to the terminal can emit a control marker, so any program can
clear koshell's context. This is the existing trust model of the marker channel, not a new
hole: the same program could already emit `command_start`/`command_end` markers and forge
command boundaries. It is bounded in the safe direction — a forged marker destroys context
rather than exfiltrating it, and nothing on the channel can make koshell run anything.

## Ordering inside `clear`

1. `SessionState::clear_context()` — drop the timeline and the completed-command index, and
   reset the screen-diff base (a retained one would make the next snapshot cite a
   `previous_snapshot_id` the reset removed).
2. Write `\x1b[H\x1b[2J\x1b[3J` to the terminal and feed those same bytes back through
   `record_presentation_output`, per the mirror-feed invariant (design 0002).
3. Ask the daemon to discard the conversation, and print the notice.

Step 1 before step 2 is what leaves exactly one honest snapshot — the now-empty screen — as
the only surviving evidence, instead of sweeping it away with the rest. The scrollback erase
(`3J`) is the point of the exercise: leaving it would keep what the user asked to forget one
scroll away, and would contradict a timeline that no longer holds it.

The **active** command span survives `clear_completed()` on purpose: `koshell clear` is
itself a running command, and discarding its span would leave the `command_end` marker
closing nothing and warning about it. Command ids keep advancing, so a stale id the model
still holds reads as "not found" rather than resolving to a different command.

## The conversation half

A new fire-and-forget `conversation_reset` client message, additive with no version bump.
It carries no session id: unlike the daemon-global `reload_request`, it arrives on the
terminal's own live connection, so the connection _is_ the address. The daemon runs the
same `resetAgent()` teardown `koshell reload` uses, minus the config re-read — deferred
onto the per-connection FIFO queue, so a streaming answer finishes on the old conversation
before it is dropped and the next `#?` rebuilds from the config already in hand.

With no live connection there is nothing to reset, and the notice says so
(`no AI conversation to reset`) rather than claiming a discard that did not happen. This is
also the normal state before the first `#?` of a session: the terminal half of `clear` runs
regardless of daemon availability, preserving the no-daemon invariant.

## What the user sees

One dim line, after the screen is wiped and before the shell's next prompt:

```
[koshell] screen, terminal context, and AI conversation cleared
[koshell] new AI conversation; the previous one is discarded
[koshell] no AI conversation to reset
```

`clear` prints a line rather than leaving a pristine screen because the invisible half of
what it did — dropping evidence and a transcript — is the half worth confirming.

## Accepted consequences and residuals

- **`clear` cannot be undone.** There is no transcript store and no evidence archive to
  restore from, which is the point; conversation persistence remains open (see the internal
  workspace's conversation-scope owner) and would need to state how a deliberate discard
  interacts with it.
- **Pending `#?` questions are left alone.** A question already submitted still fires, with
  whatever evidence exists after the clear — very little. Asking and clearing in the same
  breath is contradictory, so this stays an accepted corner rather than a cancel path.
- **Clearing during a streaming answer** is not specially ordered: the clear bytes go
  straight to the terminal while presentation may be holding output. Reaching that state
  requires typing at a prompt that a block-mode response is holding, so it is left as a
  dogfooding item rather than pre-solved.
- **Other conversations are untouched.** Both commands address one instance, like
  `koshell status` and a default `koshell reload`.

## Tests

- `shell_integration.rs` `round_trips_the_control_markers_without_span_fields` — both kinds
  survive the wire distinctly, carry no span payload, and answer `is_control`.
- `control_cli.rs` — the out-of-session refusal (including the tmux-pane shape, where
  another pane's koshell is the one that would answer), the coarse-brand fallback, and each
  control's marker being parseable by the wrapper.
- `command_history.rs` `clear_completed_drops_rows_but_keeps_the_running_span` — completed
  rows go, the clear command's own span still closes cleanly with no warning, and ids keep
  advancing.
- `trigger.rs` `clear_context_drops_the_timeline_and_the_command_index` — all evidence
  dropped, and the caller's screen-clear write roots a fresh diff chain with no dangling
  base.
- `tests/control_cli_pty.rs` — the whole loop against the real binary in a real session:
  `new` prints its notice and leaves the screen alone, `clear` erases screen _and_
  scrollback and reports what it dropped, the marker never renders on the terminal, and both
  refuse outside a session without writing to stdout.
- `packages/ai-daemon`: `protocol.test.ts` parses the message (and ignores a stray
  `session_id`); `server.test.ts` asserts the reset discards the conversation without
  replying, that the next question rebuilds, and that a reset before any question is a
  harmless no-op.

## Open issues

- Whether `clear` should also drop the shell's own history (it does not, and should not
  without an explicit ask) has not been raised by dogfooding either way.
- A `koshell new` that _keeps_ the terminal evidence but starts a conversation seeded with a
  summary of it (rather than nothing) is the obvious next question if dogfooding shows
  people reaching for `new` mainly to escape a long transcript. Out of scope here.

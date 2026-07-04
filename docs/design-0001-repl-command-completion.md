# Design 0001 — `#?` semantics and output-stabilization detection

Date: 2026-07-01 16:55:37 CST (original) / 2026-07-02 11:04 CST (revised) /
2026-07-02 12:28 CST (pending-trigger interaction and line-semantics corners added) /
2026-07-02 12:48 CST (Esc cancel added) / 2026-07-02 13:18 CST (implemented) /
2026-07-02 14:32 CST (prompt-layer capture moved to marker ownership after an fzf false
positive)

Status: implemented. The revision replaces the original "S1 + S3 completion detector"
decision: bracketed-paste (S1) is dropped as a load-bearing signal, "command completion"
is reframed as "output stabilization", and the `#?` semantics are made explicit. The
revised design — including the pending-trigger interaction — now is what
`crates/koshell-rs` implements; see "Implementation status" for what landed and the
recorded residuals.

## Context

koshell's `#?` trigger originally relied entirely on shell integration: temporary bash/zsh
rc files emit OSC 777 command-boundary markers, and `SessionState::handle_marker` turns a
`command_end` carrying `#?` into a `Trigger`. This works only while the shell's own prompt
hooks run. The moment the user launches a foreground CLI program — a REPL such as `python3`
or `node`, a remote shell over `ssh`, `psql`, etc. — the shell blocks on that child and its
hooks go dormant.

koshell's product goal is to adapt seamlessly to _any_ CLI program (full-screen TUIs are
explicitly deferred), so this gap must be closed without per-program integration. We chose
the mirror-based approach: koshell already reconstructs the full screen and cursor position
for every foreground program, so both the question and its surrounding context can be read
from the terminal mirror.

The original version of this document asked "how does koshell decide that a command has
completed and the prompt has returned?" and answered with a layered detector (bracketed-paste
edges + output quiescence). A subsequent design review — driven by crash/hang scenarios,
never-terminating commands like `pnpm dev`, and changing prompts over `ssh` — showed that
"completion" was the wrong frame and that the trigger semantics needed to be pinned down
first. This revision records both.

## `#?` semantics

### A marker token, not comment syntax

`#?` is a message to koshell smuggled through the ordinary input stream. The contract is
purely lexical: **the first occurrence of `#?` in a submitted line splits it into a left
part and a question.** Making the line inert in the host program is the user's job, using
the host's own comment syntax: bare `#?` where `#` starts a comment (shells, Python),
`// #?` in JavaScript hosts, `-- #?` in SQL hosts. koshell never modifies forwarded input;
if the user omits a needed comment prefix and the host prints a syntax error, the error is
visible but harmless and the trigger still fires. There is no per-program whitelist.

### One firing rule, two emergent forms

There is a single firing rule: **every `#?` fires at the next completion or stabilization
point of whatever its line does.** The referent is always the line's output span; context
packaging is uniform, with no branch selecting between forms. The two familiar usages are
the two ends of this one rule:

- **Standalone** (left part empty, or only a comment prefix): the line is a no-op, so its
  completion point arrives immediately and its output span is empty — surrounding context
  (current screen, recent timeline) naturally becomes the primary material. Effectively a
  question about the _past and present_.
- **Inline** (`cmd #? question`, left part non-empty): the command produces output, so
  firing waits for its completion/stabilization and the output span is the primary
  material. Effectively a question about the command's _future output_.

Accepted residuals of the unification: inside programs, a standalone `#?` inherits the
stabilization detector's sensitivity to unrelated output noise (background jobs writing to
the terminal delay the quiescence window; the bounded max-wait still guarantees firing),
and when the host does not treat the line as a comment (bare `#?` in node prints a
SyntaxError) the line's "output span" is that error output — harmless, since surrounding
context is included either way.

### Arming condition: echo, not a whitelist

`#?` is armed only where **typed input is echoed back as text** (verified through the
mirror: a typed character appears near the cursor) **and the alternate screen is not
active**. Line-oriented conversations — shell prompts, REPLs, remote shells reached through
`ssh`/`docker exec`/`kubectl exec` — echo, and therefore arm with zero per-program work.
Key-intercepting programs (dev servers with hotkeys, TUIs) do not echo, so keystrokes are
never accumulated there and accidental triggers cannot happen. This is a second decisive
argument for reading the mirror rather than reconstructing keystrokes: the mirror shows not
only _what_ was typed but _whether typing is conversation at all_.

### Quote false positives

`echo "#? not a question"` must not trigger. A lightweight quote-parity tracker (single
quote, double quote, backslash — the common lexical subset across shells and REPL
languages) suppresses `#?` inside unclosed quotes. Heredocs and triple quotes are accepted
misses. The residual cost is bounded: koshell never alters what executes, so the worst case
is one unwanted AI answer.

## The core problem, reframed

A human running `some-batch-job` waits for the prompt; a human running `pnpm dev` waits for
the startup log to _settle_. The general signal humans use is not "the command finished"
but "**the output stabilized**" — for terminating commands the two coincide. Detecting
stabilization needs two orthogonal signals:

- a **time** signal — has output stopped? Only observable locally, by watching the stream.
- a **space** signal — does the resting screen look like it awaits input? A single frame
  can suggest this, but can never prove output has stopped.

Neither substitutes for the other, and no in-band protocol signal is universal (see the
experimental findings below). The design therefore rests on local quiescence as the
load-bearing signal, with everything else demoted to modulation or annotation.

## Experimental findings (macOS, 2026-07-01)

Probes drove `python3` (3.14, PyREPL) and `node` (24) under a PTY, observing both the output
byte stream and `tcgetattr(master)`:

- `tcgetattr(master)` **does** reflect the slave's line-discipline state on macOS.
- **PyREPL** emits bracketed-paste toggles per command (`\x1b[?2004h` at prompt,
  `\x1b[?2004l` at submit) and toggles termios cooked↔raw. Crisp boundaries.
- **node** emits **neither**: always raw, no bracketed-paste. Boundaries invisible at the
  tty layer.

Conclusion: there is no single universal in-band completion signal, and any in-band signal
(bracketed-paste, termios) fails exactly when programs misbehave — a wedged program's
closing `?2004h` never arrives. In-band edges are therefore unfit to carry pending `#?`
triggers. This supersedes the original decision to ship bracketed-paste (S1) as a primary
layer.

## Design: layered firing authority

The layering below applies to the unified firing rule; a no-op line is the trivial case
whose completion point arrives immediately.

### Layer 0 — shell command spans (authoritative, no timeout)

For commands launched from an integrated shell, OSC `command_end` is the authoritative
completion signal, **including crashes and non-zero exits** — control always returns to the
prompt, so the marker always arrives. No short timeout applies here: a command that hangs
silently for minutes and then exits fires exactly once, at the marker. (The earlier idea of
a global ~30s max-wait was wrong at this layer; it would truncate slow batch jobs.)

For **non-terminating commands** (`pnpm dev`, `ssh`, watchers), `command_end` never
arrives. The stabilization point is the secondary signal: fire at settle, annotated that
the command is still running. `ssh example-pc #? explain this` needs no special case — it
fires when the login banner and remote prompt settle, with the login output as material.

### Layer 1 — inside programs (echo-armed, stabilization-driven)

Where shell markers are dormant (REPLs, remote shells), the echo-armed `#?` uses
stabilization directly:

- **Quiescence with escalating debounce** (indicative tiers: ~150 ms → ~500 ms → ~2–5 s).
  Quiescence is measured from the later of the last PTY output and the question's
  submission (fix 0003): the echo of the user's own typing before a thinking pause
  must not count as "settled", or the question fires before the program has had any
  chance to respond to the Enter.
- **A generic prompt-shape heuristic as a debounce modulator, never a gate**: cursor
  resting at the end of a short, prompt-like line (`$`, `#`, `>`, `%`, `:` + space, etc.)
  selects the fast tier; anything else selects a slower tier _but still fires_. Gating on
  prompt-likeness is exactly what would wedge on `pnpm dev` (settles on a log line) and on
  changed prompts (remote hosts). No learned prompt templates — shapes only.
- **A bounded max-wait fallback** so a pending question is never silently lost even if
  output never goes quiet (continuous streaming); it fires with a "stabilization not
  confirmed" annotation.

### AI self-check (lands with the agent runtime)

The consumer of stabilization detection is the AI request itself. Two consequences:

- When the AI daemon is absent, `#?` can only print a local notice, so detection precision
  is moot — the no-daemon invariant is naturally preserved.
- Once the daemon runs an agent with terminal tools, the request is annotated "the command
  may still be running", and the agent can re-fetch a screen snapshot itself if the context
  looks truncated. Detection on the terminal side therefore only needs to be _not absurdly
  early_ and _never lossy_; semantic precision is the agent's job. No separate
  completion-arbitration call is planned.

### Signals explicitly demoted or rejected

- **Bracketed-paste edges (S1)**: removed from the trigger path. Zero node coverage, and
  the closing edge never arrives when a program wedges. May return later purely as a
  latency optimization (skipping one debounce window) if dogfooding shows the need.
- **termios cooked↔raw (S2)**: still out; every prober that toggled termios also emitted
  bracketed-paste, so its marginal value is negative once S1 itself is out.
- **OSC 133 (S0)**: consume opportunistically as programs adopt it; it upgrades a session
  to authoritative boundaries but is never required.

## `#?` capture: mirror-read

Detection reads the rendered line, not keystrokes. At the submit instant (Enter observed
while armed), the mirror holds the pre-execution screen; koshell reads the cursor's logical
line (joining soft-wrapped rows), scans for `#?` outside quotes, and extracts the question.
This is robust to line editing (history recall, arrow edits, paste, multibyte input)
because the mirror reflects the final rendered text — and the same mirror read powers the
echo-verification arming check.

## Interaction with the shell OSC path

The two paths are mutually exclusive by the same gate as before: outside a command span the
OSC path owns `#?`; inside a span (and not on the alternate screen) the in-program path
owns it. Deferred fires reuse `build_context_package`, so the downstream trigger path is
identical.

## Pending-trigger interaction

A deferred `#?` (submitted, not yet fired) is a visible interaction state:

- **Delayed receipt.** If the trigger fires within roughly one second, nothing extra is
  printed. Past that threshold, presentation prints one dim "waiting for output to settle"
  line so the pending question is visible. The threshold keeps fast paths free of chrome.
- **A user-typed Ctrl+C cancels the pending question** (with a one-line notice). koshell
  sees the `^C` byte in the input stream, so a user interrupt is distinguishable from a
  command failing on its own — autonomous crashes and non-zero exits still fire, because
  `command_end` is authoritative including failures. An inline `#?` asks about the line's
  future output; interrupting the command withdraws that future, and a follow-up
  standalone `#?` is the natural way to ask about the aborted past.
- **A bare Esc cancels the pending question without killing the command.** While at least
  one question is pending and the trigger context is armed (not on the alternate screen —
  `vim file #? question` must never lose vim's Esc), a bare Esc is consumed: it cancels
  the most recently submitted pending question (LIFO, one per press), prints a one-line
  notice, and is not forwarded. The same key aborts a streaming AI response. In every
  other state ESC is forwarded untouched, so baseline transparency is intact. A bare Esc
  is disambiguated from escape-sequence prefixes (arrow keys, Alt combinations) by a
  short continuation timeout (~25–50 ms), active only inside the pending/streaming
  window. Accepted residual: a non-alt-screen program that binds ESC (for example
  `less -X`) loses one keypress while a question is pending — bounded, visible through
  the notice, recoverable by pressing again.
- **FIFO concurrency.** Pending triggers are per-line and independent, each firing at its
  own line's completion point; AI requests serialize through the single conversation (the
  agent session is sequential by nature) with a small defensive queue cap. Typed input
  during a streaming AI response forwards to the foreground program as normal.

## Line-semantics corners (accepted)

- **Background commands** (`cmd & #? question`): the line completes immediately when the
  shell backgrounds the job, so the trigger fires with an empty output span; the job's
  later output lands on the terminal and stays reachable through surrounding context.
  Revisit if dogfooding shows users expect the trigger to track the background job.
- **Multi-line input**: shell integration reports the full logical command at the shell
  layer, so continuation lines (PS2, trailing backslashes, quotes spanning lines) are
  covered there. Inside REPLs, mirror-read captures the cursor's logical line only —
  multi-line constructs are an accepted miss.
- **`#? /` is a reserved command namespace** for session commands, parsed consistently
  with pi's slash-command conventions; genuine questions starting with a literal `/` are
  not supported.

## Known limitations

- Programs that read stdin without echoing or printing a prompt: `#?` is disarmed there by
  design; the future entry point is a koshell-owned escape hotkey (koshell owns stdin
  forwarding), deferred. Bare Esc is now taken by pending-question cancel, so that entry
  hotkey needs a different binding (for example double-Esc or a Ctrl combination).
- Quote-parity misses: heredocs, triple quotes.
- Full-screen TUIs (alt-screen): deferred, separate design.
- Continuous tracking of a long-running command (AI keeps watching `pnpm dev` and speaks on
  new errors) is a distinct future capability ("watch"), to be built on stabilization
  events plus an intervention policy with rate limiting. Out of scope here; this design
  only guarantees the event primitive it would consume.
- Presentation of AI responses relative to live program output (buffering, insertion
  points, the mirror-feed invariant) is decided in
  `design-0002-ai-output-and-context-boundaries.md`; out of scope here.

## Testing strategy

Follow the existing real-PTY regression pattern (`tests/shell_integration_pty.rs`,
`tests/repl_completion_pty.rs`): spawn the real `koshell` binary in a PTY, drive input,
assert on `[koshell] #?` feedback and its position relative to output sentinels.

- python3 and node REPLs: inline `#?` fires _after_ the statement's output (both now via
  stabilization; node exercises the no-signal floor).
- Standalone `#?` at a REPL prompt: fires promptly through the same path (the no-op line's
  completion point arrives immediately).
- A command that pauses mid-output longer than the fast debounce tier: must not fire early
  once the prompt-shape modulator lands (slow tier applies while resting on a non-prompt
  line).
- Echo arming: `#?` typed into a non-echoing program must not trigger.
- Quote parity: `echo "#? x"` at a REPL must not trigger.
- Skip cleanly when an interpreter is absent.

## Decision

- `#?` semantics: marker token; one firing rule — every `#?` fires at the next
  completion/stabilization point of its line (immediate for no-op lines); echo +
  non-alt-screen arming; quote-parity suppression.
- Firing authority: shell `command_end` authoritative (no short timeout); stabilization
  (escalating debounce, prompt-shape as modulator only, bounded max-wait) for
  non-terminating commands and in-program contexts.
- Bracketed-paste is removed from the trigger path; termios stays out; OSC 133 is
  opportunistic.
- Post-stabilization semantic precision is delegated to the agent's snapshot self-check
  once the AI runtime lands.
- Pending-trigger interaction: delayed receipt (about one second), user-typed Ctrl+C
  cancels the pending question while autonomous failures still fire, bare Esc cancels
  without killing the command (and aborts a streaming response), FIFO serialization
  through one conversation.
- Accepted corners: background commands fire immediately with an empty span; REPL
  multi-line input is a miss (the shell layer is covered by shell integration); `#? /` is
  reserved for session commands.

## Implementation status

Implemented in `crates/koshell-rs` on 2026-07-02, replacing the earlier S1 + S3 prototype
(commit `36ed0a4`). All deltas listed by the revision landed:

- `BracketedPasteScanner`, `ReplDetector`, and keystroke reconstruction are removed;
  python and node both fire through stabilization (real-PTY tests updated accordingly).
- Capture is a mirror read of the cursor's logical line (soft-wrapped rows joined) at the
  Enter instant, which doubles as the echo-verification arming check; quote parity
  suppresses `#?` inside unclosed quotes on both the capture and the marker-text paths.
- Stabilization fires on quiescence with debounce tiers selected by the prompt-shape
  modulator — in-program: 150 ms prompt-like / 500 ms short / 3 s otherwise, max-wait
  30 s; shell-layer: 750 ms / 3 s / 10 s, max-wait 120 s, conservative so the
  authoritative `command_end` wins the race for terminating commands. All values are
  dogfooding-tunable constants in `trigger.rs`.
- An inline `#?` on a non-terminating command fires at stabilization annotated
  still-running; the later `command_end` does not re-fire it.
- The context package carries trigger metadata: `form` (standalone/inline), `completion`
  (`command_end` / `stabilized` / `max_wait`), `stillRunning`, and the exit code when the
  authoritative marker fired.
- Pending-trigger interaction: the ~1 s delayed receipt notice, Ctrl+C cancel (with
  `command_end` re-fire suppression so the interrupt's own failure marker stays quiet),
  and bare-Esc cancel with a 40 ms continuation-timeout disambiguation in the stdin
  thread. If the cancel race is lost (the question fired just before the keypress), the
  swallowed Esc is forwarded after all, preserving transparency.
- Presentation output (notices, fire feedback) is fed to the terminal mirror — the
  mirror-feed invariant of design 0002, applied to the presentation lines that exist
  today.

Implementation notes recorded for dogfooding:

- **At the integrated shell prompt, the marker layer owns `#?` exclusively; submit-time
  mirror capture is armed only inside command spans and in shells without integration
  hooks.** The first implementation also captured at the prompt and dogfooding
  immediately hit a false positive: fzf's Ctrl+R widget renders `#?` history entries and
  its own query line right where the cursor rests, so confirming a selection captured
  the rendered UI text (a bogus empty question that later double-fired at `command_end`).
  Echo verification cannot help — the fzf query line _is_ echoed typed input. A marker,
  by contrast, only exists for a line the shell really accepted. To keep stabilization
  firing on non-terminating commands, the `command_start` marker now carries the full
  typed line: zsh's `preexec` already does, and the bash `DEBUG` trap reads it back from
  history (with a `$BASH_COMMAND` fallback), emitting one start per accepted line.
- The `command_end` marker fires the span's pending question; extraction from the end
  marker's text remains the fallback when no pending exists, which also covers
  multi-line shell input (the marker carries the full logical command). In shells
  without integration hooks, prompt-line capture plus stabilization is the only path.
- Same-write submits (a paste whose trailing newline arrives in the same chunk as the
  text) are covered at the shell layer by the markers, but are an accepted miss inside
  programs without bracketed paste: the mirror has not rendered the line when Enter is
  processed.
- Rendered-UI `#?` text remains an accepted residual where capture is armed: inside
  command spans (a program printing a `#?`-shaped prompt line, a finder launched as a
  command) and at non-integrated prompts. Bounded cost: one unwanted answer.
- While the alternate screen is active, pending deadlines, notices, and the Esc cancel
  are suspended so nothing scribbles over a full-screen program; `command_end` still
  fires, and leaving the alternate screen re-arms pending questions.
- A quiet terminating command (a silent batch job) can reach the shell-layer slow tier
  and fire before its `command_end`, annotated still-running — the accepted imprecision
  the agent's snapshot self-check is designed to absorb.

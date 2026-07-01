# Design 0001 — `#?` inside CLI programs: generic command-completion detection

Date: 2026-07-01 16:55:37 CST

Status: MVP prototype implemented (S1 + S3). See "Prototype status" for what landed and how
it deviates from this design.

## Context

koshell's `#?` trigger currently relies entirely on shell integration: temporary bash/zsh
rc files emit OSC 777 command-boundary markers, and `SessionState::handle_marker` turns a
`command_end` carrying `#?` into a `Trigger`. This works only while the shell's own prompt
hooks run.

The moment the user launches a foreground CLI program — a REPL such as `python3` or `node`,
or `psql`, `sqlite3`, etc. — the shell blocks on that child and its hooks go dormant. No
OSC markers are produced, so a `#?` typed at the program's own prompt is invisible to
koshell. koshell's product goal is to adapt seamlessly to _any_ CLI program (full-screen
TUIs are explicitly deferred), so this gap must be closed without requiring per-program
integration.

We chose **Approach B** (see the conversation that produced this doc): detect `#?` from the
terminal mirror rather than by injecting program-specific hooks. koshell already reconstructs
the full screen and cursor position for every foreground program, so:

- **Context is essentially free** — whatever the user sees (results, tracebacks, output) is
  already in the mirror and flows into the existing context package.
- **Detection** of a submitted `#?` line can read the rendered logical line from the mirror
  at submit time, which is robust to line editing (backspace, history recall, paste) because
  it reads the final rendered text, not raw keystrokes.

This document addresses the remaining hard problem that Approach B exposes: one behavior of
`#?` is that when a real command precedes it (inline form `cmd #? question`), the AI launch
is **deferred until the command finishes**, so the AI can see the command's output. In the
shell this is trivial (`command_end`). Inside an arbitrary CLI program there is no marker —
a human knows the command finished because _the prompt reappears_. The question this design
answers: **how does koshell decide, generically, that a command has completed and the prompt
has returned?**

## Goals and non-goals

Goals:

- Detect command completion / prompt reappearance inside foreground CLI programs with no
  per-program integration, well enough to drive deferred `#?` launches.
- Reuse koshell's existing assets: the terminal mirror, the shell command-span gating, the
  alternate-screen signal, and the context package.
- Degrade safely: never hang; when detection is uncertain, prefer an explicit, bounded
  fallback over a wrong or lost trigger.

Non-goals (for this design):

- Full-screen TUIs (alt-screen apps like `vim`, `less`, `htop`). Explicitly deferred.
- Per-program deep integration (e.g. IPython event hooks). Noted as an optional future
  escalation, not part of the generic path.
- Reconstructing structured program state (variable values, exception objects). Context
  comes from the rendered screen, which is sufficient for "explain what I'm looking at".

## Background: what koshell already has

- **Terminal mirror** (`TerminalMirror`, `alacritty_terminal`): full screen text, cursor
  position, and `alt_screen` flag, updated from the raw PTY byte stream.
- **Marker scanner** (`MarkerScanner` → `Segment::Visible` / `Segment::Marker`): already
  splits the PTY stream and can be extended to recognize additional in-band control
  sequences without disturbing visible output.
- **Shell command spans**: `command_start` / `command_end` markers tell koshell when a
  foreground child begins and ends. This is the natural gate for "are we inside a program?"
- **Session state & context** (`SessionState`, `build_context_package`): the assembly point
  for a `Trigger`'s context package.

## The core problem

The generic signal a human uses is "the prompt came back". Programmatically, the fundamental
truth is: **the program stopped emitting output and is now blocked reading stdin.** koshell
cannot directly observe "blocked on read", so it must infer completion from observable
proxies. Industry consensus is that this is hard — serious terminals require shell
integration / OSC 133 precisely because generic detection is unreliable. koshell's approach
is therefore a **layered, confidence-ranked detector** with a heuristic floor, not a single
signal.

## Experimental findings (macOS, 2026-07-01)

Probes drove `python3` (3.14, PyREPL) and `node` (24) under a PTY, observing both the output
byte stream and `tcgetattr(master)`:

- `tcgetattr(master)` **does** reflect the slave's line-discipline state on macOS, so
  termios raw/cooked transitions are observable from the master fd.
- **PyREPL**: emits bracketed-paste toggles per command (`\x1b[?2004h` when entering line
  edit = prompt ready; `\x1b[?2004l` when submitting = running) **and** toggles termios
  cooked (while running) ↔ raw (while editing). Command boundaries are crisp.
- **node**: emits **neither** per command. It stays in raw mode continuously and does not
  emit bracketed-paste sequences. Command boundaries are invisible at the tty layer.

Conclusion: **there is no single universal in-band completion signal.** Even the promising
tty-level signals fail for node. The detector must be layered, and must always have a
program-agnostic heuristic floor.

## Signal catalog (ranked by confidence; with coverage)

- **S0 — explicit markers** (OSC 133 `A/B/C/D`, or a koshell OSC a program opts into).
  Authoritative. Rare in REPLs today but growing; koshell should consume OSC 133 when
  present.
- **S1 — bracketed-paste toggle in the byte stream**: `\x1b[?2004h` ⇒ prompt/edit ready
  (i.e. the previous command completed); `\x1b[?2004l` ⇒ input submitted / command running.
  Zero polling — folds into `MarkerScanner`. Covers readline/libedit/PyREPL/psql/shells.
  **Does not cover node.**
- **S2 — termios cooked↔raw** via `tcgetattr(master)` polling, or event-driven via Linux
  PTY packet mode (`TIOCPKT`). Covers programs that drop to cooked mode to run (PyREPL).
  **Does not cover node** (always raw). Adds ioctl/polling and portability cost. In testing,
  every program that toggled termios also emitted S1, so S2's marginal value is low.
- **S3 — output quiescence + learned-prompt reappearance** (the universal floor): after a
  `#?` submission, when the PTY output goes idle for a debounce window **and** the cursor's
  resting logical line matches the learned prompt template, treat the command as complete.
  Needed for node and "dumb" cooked-mode prompts. Least reliable (mid-command pauses and
  sub-prompts can false-positive), mitigated by the prompt-template match and debounce.

## Design: layered completion detector

### Gating (reuse what koshell already knows)

The detector is active only when both hold:

1. **Inside a foreground child** — between a shell `command_start` and its matching
   `command_end`. Outside this (at the shell prompt) the existing OSC path owns `#?`, so
   there is no double-handling. The `command_end` also guarantees a hard upper bound: when
   the child exits, the detector is torn down.
2. **Normal screen** — `mirror.alt_screen == false`. Alt-screen (TUI) is out of scope; the
   detector stays dormant there.

### Detection layering

Run signals by priority and take the highest-confidence one available; keep S3 armed as the
floor:

- If S0 is present, use it (authoritative).
- Else if S1 is observed, use bracketed-paste edges.
- Else fall to S3 (quiescence + prompt template). (S2 is out of MVP; see Open issues.)

The detector is a small state machine per foreground-child span:

- `Editing` — the program is at a prompt awaiting input (S1: last edge was `?2004h`; S3:
  output idle and cursor on a prompt-matching line).
- `Running` — a line was submitted and the command is executing (S1: last edge was
  `?2004l`; S3: output active after a submit).
- Transition `Running → Editing` is the **completion event**.

### Prompt learning (for S3 and false-positive suppression)

Each time output quiesces and the cursor comes to rest, capture the tail of the cursor's
logical line as a candidate prompt. Across observations, derive a template: take the stable
suffix and mask variable regions (digit runs, paths, git branches) so dynamic prompts
(`In [1]:` → `In [2]:`, `user@host:~/dir$`) still match. The template both recognizes
"prompt reappeared" and guards S3 against mid-command output pauses (a pause does not
reproduce the prompt).

### Deferred `#?` launch semantics

Uniform rule, matching the shell: **a `#?` submission fires on the next completion event.**

1. On a submitted line containing `#?` (detected via Approach B — read the rendered logical
   line from the mirror at submit time), record a pending trigger with the extracted
   question (`#?` to end-of-line, same as `extract_question`).
2. Fire when the next `Running → Editing` completion event occurs.
3. Context region = the output produced between submission and completion:
   - S1/S2: precisely the bytes between `?2004l` and the next `?2004h` (resp. cooked→raw).
   - S3: approximated by the mirror scrollback delta over the interval.
4. A bare `#? q` (no real command) is not a special case: the command is a no-op, the
   completion event arrives almost immediately, and it fires — identical to the shell's
   "always fire at command end".

### Nested input-prompt counting

Commands that themselves read input (nested REPL, `sudo` password prompt, a pager) produce
nested `Editing`/`Running` cycles — e.g. a nested `?2004h`. Track a **pairing depth** by
counting submit/prompt edges (`?2004l` vs `?2004h`, or Running/Editing transitions); only the
transition that returns depth to zero counts as completion of the user's original line. This
is best-effort: sub-programs may not balance their edges, so S3 stays conservative here.

### Timeout, degradation, and child exit

- **Max-wait timeout**: if no completion event arrives within a bounded window after a
  `#?` submission, degrade explicitly (fire with whatever context is available, or surface
  "could not detect completion") rather than hang.
- **Child exit**: if `command_end` arrives (the whole program exited) before completion is
  detected, tear down the detector and fall back to the shell path / cancel the pending
  trigger. The detector must never outlive its span or block the terminal hot path.

## `#?` detection inside the program (Approach B recap)

Detection reads the rendered line, not keystrokes:

- The submit instant is known from the same signals: S1 `?2004l`, S2 cooked transition, or
  (S3) the Enter keystroke seen on stdin while `Editing`.
- At that instant the mirror holds the pre-execution screen; koshell reads the cursor's
  logical line (joining soft-wrapped rows), scans for `#?`, and extracts the question. This
  is robust to editing because the mirror reflects the final rendered line.
- The prompt prefix (`>>> `, `> `, `In [1]: `) is irrelevant to detection — we scan for the
  `#?` token anywhere on the line.

## Interaction with the existing shell OSC path

The gate makes the two paths mutually exclusive: at the shell prompt, `command_start` has
not opened a child span (or the previous one closed with `command_end`), so the OSC path
handles `#?`. Inside a child span, the OSC path is dormant and the completion detector owns
`#?`. Bracketed-paste sequences that shells also emit are only interpreted by the detector
when inside a child span, so there is no conflict.

## Known limitations / hard cases

- **Commands that prompt for input** (nested readline, password prompts, pagers, nested
  REPLs): mitigated by pairing-depth counting, but not fully solvable generically.
- **Always-raw REPLs with no markers** (node): fall back to S3, the weakest layer.
- **Dynamic prompts**: template masking helps but cannot guarantee 100% recognition.
- **Programs with no visible prompt** (e.g. reading stdin without printing a prompt): no
  reliable completion signal; `#?` semantics there are out of scope — degrade gracefully.

## Testing strategy

Follow the existing real-PTY regression pattern
(`crates/koshell-rs/tests/shell_integration_pty.rs`): spawn the real `koshell` binary in a
PTY, launch a program, drive input, and assert on `[koshell] #?` feedback and timing.

- **python3 (S1 path)**: `x = 2 + 2  #? explain` — assert the `#?` fires _after_ the
  statement completes (bracketed-paste `?2004h` returns), not at submit time.
- **node (S3 path)**: same shape — assert completion is detected via quiescence + prompt
  reappearance, exercising the floor layer for an always-raw REPL.
- Bare `#? q` at a REPL prompt — assert it fires promptly.
- Skip cleanly when the interpreter is absent (as the existing tests do).

## Open issues / future work

- **S2 via Linux PTY packet mode (`TIOCPKT`)**: an elegant event-driven raw/cooked signal,
  but `portable-pty` may not expose it, macOS `TIOCPKT` semantics differ, and node would not
  benefit. Deferred.
- **OSC 133 consumption (S0)**: adopt as programs increasingly emit it; would upgrade many
  REPLs from S3 to authoritative detection.
- **Opt-in per-program integration** for high-value REPLs (e.g. IPython `pre_run_cell` /
  `post_run_cell`) to get authoritative boundaries and richer structured context. Optional
  escalation, not the generic path.
- **TUI / alt-screen** handling — separate future design.

## Decision & MVP scope

- Ship **S1 + S3** as the MVP completion detector, gated by shell command spans and
  non-alt-screen, driving deferred `#?` launches. Rationale: S1 gives crisp, zero-polling
  boundaries for the large readline/libedit/PyREPL family; S3 is the universal floor for
  node and dumb prompts; S2's marginal coverage did not justify its ioctl/polling and
  portability cost.
- Keep S0/S2 and per-program integration as documented future enhancements.

## Prototype status (implemented)

Landed in `crates/koshell-rs`:

- `ReplDetector` and `BracketedPasteScanner` in `trigger.rs`, owned by `SessionState`. Gated
  by `repl_gated()` (`command_active && !mirror.is_alt_screen()`), so the shell OSC path and
  the in-program path never overlap. The detector resets at every `command_start` /
  `command_end`.
- S1: `on_output` scans visible bytes for bracketed-paste `ESC[?2004h`/`ESC[?2004l` edges
  (with cross-chunk carry) and completes a pending `#?` on the `?2004h` edge.
- S3: `session.rs` uses `recv_timeout(REPL_QUIESCENCE_DEBOUNCE)` (150 ms) while a pending
  `#?` is armed and no bracketed-paste edge has been seen (node-style programs); an idle tick
  then fires via `on_quiescence`.
- Deferred fire reuses `build_context_package`, so the AI context is the post-completion
  screen — identical downstream path to the shell trigger.
- Tests: `tests/repl_completion_pty.rs` drives real python (S1) and node (S3) REPLs under a
  PTY and asserts the `#?` fires _after_ the command's output sentinel. They skip when the
  interpreter is absent.

Deviations from the design above, deliberately deferred:

- **`#?` capture uses keystrokes, not the mirror.** The prototype reconstructs the submitted
  line from input bytes (with minimal backspace handling) rather than reading the rendered
  logical line from the mirror at the submit instant. This is simpler and avoids an
  echo-vs-input timing race, but is not robust to in-line editing (history recall, arrow
  edits, paste). The mirror-read described in "`#?` detection inside the program" is the
  intended productionization.
- **No prompt learning yet.** S3 uses pure output quiescence without the learned-prompt
  template, so a node command that pauses mid-output could fire early. Acceptable for the
  prototype; the template match is the documented hardening step.
- **No nested-prompt pairing, no max-wait timeout.** Both remain as designed-but-unbuilt
  safety/robustness items.

# Architecture

Koshell is a hybrid monorepo with two runtimes that keep the human as the primary
terminal operator while AI assists from beside the shell.

## Processes

- **`koshell-rs` (Rust, foreground)** — one process per terminal window. Owns:
  - PTY spawn, stdin/stdout forwarding, resize (rows/cols and pixel geometry), signal
    forwarding (`SIGHUP`/`SIGTERM`/`SIGINT` relayed to the inner shell), a tty-scoped
    nested-start guard (the child is branded with its controlling tty in the single
    `KOSHELL` variable, so a shell re-wraps unless that brand equals its own `$(tty)`; this
    makes every tmux pane wrap and `#?` work there — see
    `design-0009-tty-scoped-nesting-marker.md` and
    `design-0017-consolidate-environment-into-koshell.md`),
    faithful exit-code propagation (a signal death surfaces as `128 + signo` via a
    direct `waitpid`), fail-open startup safety (a `preflight` gate plus exec-into-the-real-shell
    on any pre-takeover error, so the `exec koshell` auto-wrap cannot lock out a terminal),
    and working-directory mirroring (a `precmd` cwd marker moves koshell's own process cwd
    so `tmux pane_current_path` reads the inner shell's directory; see
    `fix-0005-pty-wrapper-transparency.md` and `fix-0006-exit-code-fidelity-and-fail-open.md`);
  - CLI launch modes: `koshell` wraps the default shell; `koshell <command> [args...]`
    launches that program directly (explicit bash/zsh still gets integration, appended
    before user arguments; any other program runs without integration, so `#?` uses the
    non-integrated mirror-capture + stabilization path). `--` reserves the option
    namespace for future flags. `koshell shell-init <shell>` prints the rc snippet for
    `eval "$(koshell shell-init zsh)"`-style auto-wrap installs (see
    `design-0003-shell-init-auto-wrap.md`);
  - the terminal mirror (via `alacritty_terminal`), screen snapshots, alternate-screen
    detection, and line-level screen diffs;
  - the bounded in-memory terminal timeline (age-tiered snapshot downsampling plus a
    recent-character budget for raw text, so a long-lived session stays bounded; see
    `fix-0007-timeline-memory-retention.md`) and local terminal context;
  - shell integration (temporary rc files emitting OSC command-boundary markers) and
    `#?` trigger detection — the marker layer owns `#?` at the integrated shell prompt
    (start markers carry the full typed line, `command_end` is authoritative);
    mirror-read capture at submit (echo arming, quote-parity suppression) applies inside
    foreground CLI programs and in shells without integration; output-stabilization
    firing covers REPLs and non-terminating commands; pending-trigger interaction
    (delayed receipt, Ctrl+C / bare-Esc cancel). See
    `design-0001-repl-command-completion.md` for the trigger semantics and detector
    design.
  - It remains usable as a transparent shell wrapper when the AI daemon is absent.
- **`koshell-ai-daemon` (Bun, shared)** — one process per user session. Receives
  `#?` requests over IPC and answers them through pi-backed agent conversations, one
  persistent conversation per terminal session, discarded on disconnect (see
  `design-0002-ai-output-and-context-boundaries.md`). Requests are serialized FIFO per
  conversation; responses stream back as `ai_delta` messages. The terminal auto-spawns
  the daemon on demand and it is single-instance per user (the socket is the lock),
  exiting itself after an idle period; lifecycle and the Bun runtime choice are owned by
  `design-0008-daemon-lifecycle-auto-spawn-and-bun-runtime.md`. Provider/model/auth
  resolution is Koshell-owned: the daemon reads `koshell.toml`, adapts the selected model
  and credentials into pi, and supports stored OAuth credentials without reading pi's
  configuration files. The read-only terminal tool loop is not wired yet, so each request
  relies on a bounded context package pushed by the terminal.

## Dependency boundaries

- Terminal-core (Rust) must not depend on any LLM provider or the pi packages.
- Provider/model/auth and the pi agent session live only in the AI daemon.
- The daemon's source uses `node:` APIs only; Bun is its runtime and packager, not an
  API surface, so the runtime choice stays reversible.
- The two runtimes communicate only through `koshell-proto` messages.

## IPC

Newline-delimited JSON (JSONL) over a Unix domain socket at
`$XDG_RUNTIME_DIR/koshell/daemon.sock` (falling back under `$XDG_CACHE_HOME/koshell/`).
The terminal connects lazily; if the daemon is unavailable the terminal keeps working and
`#?` degrades explicitly. A `hello` handshake negotiates the protocol version, and the
daemon enforces it: `ai_request`s are served only after a version-matching `hello`;
otherwise each request is answered with an explicit `ai_error` naming both versions, so
a mixed-version fleet (long-lived terminals, independently restarted daemon) degrades
readably instead of failing on message-shape mismatches. Protocol evolution is additive
by default — unknown message types are ignored by both ends, the `hello` shape is
frozen, and the version is bumped only for breaking changes (see the `koshell-proto`
crate docs and `design-0004-ipc-version-enforcement.md`).

Messages (see `crates/koshell-proto`):

- Terminal → daemon: `hello`, `ai_request` (carries the assembled context package),
  `ai_cancel` (best-effort withdrawal after a user interrupt; see
  `design-0006-interrupting-ai-responses.md`), `tool_response` (reserved), `bye`.
- Daemon → terminal: `ack`, then per request zero or more `ai_delta` chunks followed by
  exactly one of `ai_response_end` or `ai_error` (a cancelled request still gets its
  terminal marker). `ai_tool_call` is reserved for the tool round-trip stage.

## Implementation status

Status updated: 2026-07-10 10:16 CST +0800.

The current stage delivers the full Rust terminal-core and a pi-backed AI daemon: `#?`
requests reach one FIFO-serialized conversation per terminal session and answers stream
back into the terminal. The terminal auto-spawns the daemon; Koshell-owned
`koshell.toml` configuration, the full pi builtin provider catalog, interactive OAuth,
per-instance status, and config reload are implemented. The `#?` detector implements
the stabilization-based design of `design-0001-repl-command-completion.md`, including
pending-trigger interaction and Ctrl+C cancellation. Response presentation implements
bounded stream/block separation and anchored streaming.

Two dogfooding gaps remain on the core context path:

- Context is push-only. When command output exceeds the bounded pushed window, the agent
  cannot retrieve older off-screen output; a real observed case left it with only the
  current screen. The reserved `ai_tool_call` / `tool_response` round trip and read-only
  terminal tool catalog are not implemented.
- Conversations live only in daemon memory. `koshell reload` intentionally replaces the
  current agent session and loses its transcript, and no transcript can be resumed after
  a terminal disconnect or daemon restart. Conversation persistence and resume semantics
  are not designed or implemented.

The pre-rewrite TypeScript prototype is frozen under `reference/` as algorithm and
behavior reference.

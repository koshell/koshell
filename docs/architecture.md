# Architecture

Koshell is a hybrid monorepo with two runtimes that keep the human as the primary
terminal operator while AI assists from beside the shell.

## Processes

- **`koshell-rs` (Rust, foreground)** — one process per terminal window. Owns:
  - PTY spawn, stdin/stdout forwarding, resize, signals, nested-start guard;
  - CLI launch modes: `koshell` wraps the default shell; `koshell <command> [args...]`
    launches that program directly (explicit bash/zsh still gets integration, appended
    before user arguments; any other program runs without integration, so `#?` uses the
    non-integrated mirror-capture + stabilization path). `--` reserves the option
    namespace for future flags. `koshell shell-init <shell>` prints the rc snippet for
    `eval "$(koshell shell-init zsh)"`-style auto-wrap installs (see
    `design-0003-shell-init-auto-wrap.md`);
  - the terminal mirror (via `alacritty_terminal`), screen snapshots, alternate-screen
    detection, and line-level screen diffs;
  - the append-only terminal timeline and local terminal context;
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
- **`koshell-ai-daemon` (Node.js, shared)** — one process per user session. Receives
  `#?` requests over IPC and answers them through pi-backed agent conversations, one
  persistent conversation per terminal session, discarded on disconnect (see
  `design-0002-ai-output-and-context-boundaries.md`). Requests are serialized FIFO per
  conversation; responses stream back as `ai_delta` messages. Provider/model/auth
  resolution currently delegates to pi's own default chain (`~/.pi/agent/auth.json`,
  then provider environment variables such as `ANTHROPIC_API_KEY`); Koshell-owned
  XDG/TOML provider configuration and the read-only terminal tool loop are later
  stages.

## Dependency boundaries

- Terminal-core (Rust) must not depend on any LLM provider or the pi packages.
- Provider/model/auth and the pi agent session live only in the Node daemon.
- The two runtimes communicate only through `koshell-proto` messages.

## IPC

Newline-delimited JSON (JSONL) over a Unix domain socket at
`$XDG_RUNTIME_DIR/koshell/daemon.sock` (falling back under `$XDG_CACHE_HOME/koshell/`).
The terminal connects lazily; if the daemon is unavailable the terminal keeps working and
`#?` degrades explicitly. A `hello` handshake negotiates the protocol version.

Messages (see `crates/koshell-proto`):

- Terminal → daemon: `hello`, `ai_request` (carries the assembled context package),
  `tool_response` (reserved), `bye`.
- Daemon → terminal: `ack`, then per request zero or more `ai_delta` chunks followed by
  exactly one of `ai_response_end` or `ai_error`. `ai_tool_call` is reserved for the
  tool round-trip stage.

## Implementation status

The current stage delivers the full Rust terminal-core plus the pi-backed AI daemon
prototype: `#?` requests reach a persistent per-terminal pi conversation and the answer
streams back into the terminal. The `#?` detector implements the revised
stabilization-based design of `design-0001-repl-command-completion.md`, including the
pending-trigger interaction; the debounce tiers and cancel paths await real-use tuning
(see that document's implementation notes). Response presentation implements the
prototype simplification of `design-0002-ai-output-and-context-boundaries.md` (see that
document's status note). Koshell-owned provider configuration and the terminal tool loop
(pull-side context) are the next stage.

The pre-rewrite TypeScript prototype is frozen under `reference/` as algorithm and
behavior reference.

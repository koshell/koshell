# Architecture

Koshell is a hybrid monorepo with two runtimes that keep the human as the primary
terminal operator while AI assists from beside the shell.

## Processes

- **`koshell-rs` (Rust, foreground)** — one process per terminal window. Owns:
  - PTY spawn, stdin/stdout forwarding, resize, signals, nested-start guard;
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
  `#?` requests over IPC. In a later stage it will own the pi-backed agent conversations
  (one per terminal session, discarded on disconnect; see
  `design-0002-ai-output-and-context-boundaries.md`), provider/model/auth configuration,
  the read-only terminal tool loop, and streaming AI responses.

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
- Daemon → terminal: `ack` (current stage). `ai_delta`, `ai_tool_call`,
  `ai_response_end`, and `ai_error` arrive with pi integration.

## Implementation status

The current stage delivers the full Rust terminal-core (Phases 1–4) plus a minimal Node
receiver that acknowledges `#?` requests (Phase 5-min). The `#?` detector implements the
revised stabilization-based design of `design-0001-repl-command-completion.md`, including
the pending-trigger interaction; the debounce tiers and cancel paths await real-use
tuning (see that document's implementation notes). pi integration, provider
configuration, the tool loop, and streaming AI responses are the next stage.

The pre-rewrite TypeScript prototype is frozen under `reference/` as algorithm and
behavior reference.

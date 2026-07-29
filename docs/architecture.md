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
    `design-0003-shell-init-auto-wrap.md`). The plain-stdio command surface also owns the
    searchable crossterm `koshell model` picker and scripted model list/show/set clients;
    model and credential semantics remain daemon-side (see
    `design-0018-model-discovery-and-runtime-switching.md`);
  - the terminal mirror (via `alacritty_terminal`), screen snapshots, alternate-screen
    detection, and line-level screen diffs;
  - the bounded in-memory terminal timeline (age-tiered snapshot downsampling with a
    burst floor spacing and hard byte cap, a recent-character budget for raw text, and
    an idle compaction tick, so a long-lived session stays bounded even after an output
    burst goes quiet; see `fix-0007-timeline-memory-retention.md` and
    `fix-0009-burst-snapshot-retention.md`) and local terminal context;
  - the bounded completed-command index: one stable id spans each real
    `command_start`/`command_end` pair, marker-clean bytes between them are retained
    per command (10 commands, 1 MiB each, 4 MiB per session, recent tail kept, every
    omission reported), and the read-only tools serve it over the IPC round trip. It is
    separate from the timeline and shares only the command id (see
    `design-0020-completed-command-output-tools.md`);
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
  and credentials into pi, supports stored OAuth credentials without reading pi's
  configuration files, exposes the live pi/custom model catalog, and source-preservingly
  updates the configured default. A live conversation switches models through pi's
  existing AgentSession without losing messages. The read-only terminal tool loop is not
  wired yet, so each request
  relies on a bounded context package pushed by the terminal.
  Koshell-owned custom tools are assembled per conversation from what the config
  actually enables (`tools.ts`): with none the session keeps pi's `noTools: "all"`, and
  with any it uses `noTools: "builtin"` so pi's own file, shell, edit, and write tools
  stay disabled. The first occupant is `web_search`, backed by a dedicated search API
  because pi's tool abstraction cannot carry a provider-native server tool and ships no
  MCP client — see `design-0019-web-search-tool.md`. The static system prompt is built
  to match the session's real tool set, so it never denies a capability the session has
  or advertises one it lacks.

## Dependency boundaries

- Terminal-core (Rust) must not depend on any LLM provider or the pi packages.
- Provider/model/auth, the pi agent session, and the custom-tool catalog live only in
  the AI daemon. `tools.ts` is the single place a Koshell capability becomes a pi tool,
  so the observe-only boundary is auditable from one array.
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

- Terminal → daemon: `hello` (carrying an optional capability list), `ai_request`
  (carries the assembled context package), `ai_cancel` (best-effort withdrawal after a
  user interrupt; see `design-0006-interrupting-ai-responses.md`), `tool_response`
  (settles exactly one `ai_tool_call`), auth request/prompt messages, model
  list/show/set requests, instance status/reload requests, and `bye`.
- Daemon → terminal: `ack`, then per AI request zero or more `ai_delta` chunks,
  `ai_tool_activity` lines, and `ai_tool_call`s, followed by exactly one of
  `ai_response_end` or `ai_error` (a cancelled request still gets its terminal marker);
  auth flow/status replies; model catalog/state/result replies; and instance
  status/reload replies.

`ai_tool_activity` is what makes the tool loop visible: the daemon announces every tool
call — including `web_search`, which it serves itself and which therefore produces no
other terminal-bound message — and the terminal renders it as a dim line so the user can
watch the work and decide whether to interrupt. It carries ready-to-render display text
rather than a code the terminal formats, so a new tool needs no matching terminal
release (see `design-0021-visible-tool-activity.md`).

The terminal advertises `command_output_tools_v1` in its `hello`, and the daemon
registers terminal-backed tools only for connections that did — so a new daemon with an
old terminal stays push-only rather than issuing calls that can never be answered, and a
new terminal with an old daemon has its extra field ignored. Neither direction needs a
version bump (see `design-0020-completed-command-output-tools.md`).

## Implementation status

Status updated: 2026-07-28 15:55 CST +0800.

The current stage delivers the full Rust terminal-core and a pi-backed AI daemon: `#?`
requests reach one FIFO-serialized conversation per terminal session and answers stream
back into the terminal. The terminal auto-spawns the daemon; Koshell-owned
`koshell.toml` configuration, the full pi builtin provider catalog, interactive OAuth,
per-instance status, config reload, searchable model discovery, source-preserving default
selection, and transcript-preserving per-conversation runtime model switching are
implemented. The `#?` detector implements
the stabilization-based design of `design-0001-repl-command-completion.md`, including
pending-trigger interaction and Ctrl+C cancellation. Response presentation implements
bounded stream/block separation and anchored streaming.

The custom-tool seam is open: a conversation registers Koshell-owned pi tools from what
`koshell.toml` enables, with pi's builtins still disabled. The optional `[search]` block
adds a `web_search` tool (`design-0019-web-search-tool.md`). The terminal-side tool round
trip is a separate, still-unimplemented path — see the first gap below.

The pull arm of the context contract is implemented for completed shell commands: the
terminal keeps a bounded per-command output index, advertises it in the pushed
`koshell_ai_context_v2` inventory, and serves `list_recent_commands` /
`read_command_output` over the `ai_tool_call` / `tool_response` round trip, gated on the
`command_output_tools_v1` hello capability. Output that has scrolled off the screen is
therefore reachable — see `design-0020-completed-command-output-tools.md`.

One dogfooding gap remains on the core context path, and the pull arm is deliberately
narrow:

- The catalog covers completed integrated-shell commands only. A still-running span,
  REPL statements, commands typed over SSH, non-integrated shells, screen snapshots,
  timeline ranges, and previous-question anchors are still not retrievable.
- Conversations live only in daemon memory. Model selection and model-only reload now
  preserve the active transcript, but a provider/credential/thinking configuration
  rebuild, terminal disconnect, or daemon restart still has no transcript to resume.
  Conversation persistence and resume semantics are not designed or implemented.

The pre-rewrite TypeScript prototype is frozen under `reference/` as algorithm and
behavior reference.

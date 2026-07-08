# koshell

Koshell is a human-centric shared terminal: AI beside your terminal, not above it.

The project keeps the human as the primary terminal operator while giving AI enough
shared terminal context to explain, diagnose, and assist — without turning the terminal
into a separate chat room or an agent-owned execution loop. When the terminal raises a
question, you type `#?` and ask in place.

## Usage

```bash
koshell                    # wrap the default shell (bash/zsh get shell integration)
koshell python3 -i         # launch a program directly instead of a shell; everything
                           # after the first positional argument goes to the program
koshell -- some-command    # `--` reserves room for future koshell options and allows
                           # command names that start with a dash
```

To start koshell automatically in every new terminal, install the auto-wrap snippet at
the top of your shell rc file:

```bash
eval "$(koshell shell-init zsh)"    # first line of ~/.zshrc
eval "$(koshell shell-init bash)"   # first line of ~/.bashrc (on macOS, make sure
                                    # ~/.bash_profile sources ~/.bashrc)
```

The snippet `exec`s the just-started interactive shell into koshell; the shell koshell
spawns re-sources your rc and skips the exec because it is already wrapped on this
terminal (`KOSHELL_TTY` matches its `$(tty)`), so your configuration loads exactly once,
inside the wrap. The marker is tty-scoped, so a shell that lands on a fresh terminal — a
new tmux pane — wraps itself instead, and `#?` works in every pane (see
`docs/design-0009-tty-scoped-nesting-marker.md`). It stays inert for non-interactive
shells, non-TTY stdio, and `TERM=dumb`. To opt out for one shell, start it with
`KOSHELL_NO_AUTO=1`; to disable auto-wrap without a working shell, create
`~/.config/koshell/no-auto` (see `docs/design-0003-shell-init-auto-wrap.md`).

`#?` works in every form: type `#? <question>` (or `command #? <question>`) and the
question fires when the line's output completes or stabilizes. Directly launched
programs use the output-stabilization path, since shell integration markers only exist
inside bash/zsh.

Answers come from the AI daemon, which the terminal starts automatically on the first
`#?` — you no longer run it by hand (see `docs/design-0008-daemon-lifecycle-auto-spawn-and-bun-runtime.md`).
One daemon is shared per user session, and it exits on its own after 10 idle minutes.
Manage it explicitly when you need to:

```bash
koshell daemon status    # is it running? pid, version, uptime, connections
koshell daemon start     # start it without waiting for a #?
koshell daemon stop      # stop it
koshell daemon restart   # stop (if running), then start
```

To debug the daemon in the foreground, run it directly (logs to stdout):

```bash
bun packages/ai-daemon/src/index.ts
```

The daemon runs on [Bun](https://bun.com) (≥ 1.3), the project's only JS toolchain.

To find the daemon, auto-spawn looks for a `koshell-ai-daemon` executable next to the
`koshell` binary, then on `PATH`. Working from a source checkout (no installed binary),
point it at the source instead:

```bash
export KOSHELL_DAEMON_CMD="bun $PWD/packages/ai-daemon/src/index.ts"
```

Auto-spawn is controlled by two environment variables: `KOSHELL_NO_DAEMON_SPAWN=1`
disables it (the terminal then degrades inline until you start the daemon yourself), and
`KOSHELL_DAEMON_CMD` overrides the launch command entirely (used verbatim).

Provider, model, and auth currently resolve through pi's own defaults: an existing pi
setup (`~/.pi/agent/auth.json`) or a provider API key in the environment (for example
`ANTHROPIC_API_KEY`) is enough. If no provider is configured, the terminal keeps working
and `#?` reports the degradation inline.

Both processes log at a configurable level, set by `--log-level <level>` or the
`KOSHELL_LOG` environment variable (the argument wins). The terminal owns the screen,
so its logs go to `$XDG_STATE_HOME/koshell/koshell.log` (default level `warn`,
`env_logger` filter syntax). The daemon logs to its own stdout when run in the
foreground, and to `$XDG_STATE_HOME/koshell/daemon.log` when auto-spawned (default level
`info`).

## Architecture

Koshell is a hybrid monorepo with two runtimes:

- **`koshell-rs` (Rust, foreground)** — one process per terminal window. Owns the PTY,
  the terminal mirror, screen snapshots, alternate-screen detection, the timeline, local
  terminal context, and `#?` detection. It stays usable as a transparent shell wrapper
  even when the AI daemon is unavailable.
- **`koshell-ai-daemon` (Bun, shared)** — one process per user session, auto-spawned by
  the terminal and single-instance per user (the socket is the lock). Receives `#?`
  requests over IPC and (in a later stage) runs the pi-backed agent session, provider
  configuration, tool loop, and streaming AI responses. Its source uses `node:` APIs
  only, so Bun is the runtime and packager, not an API dependency.

The two communicate over newline-delimited JSON (JSONL) on a Unix domain socket.

## Repository layout

```
crates/koshell-rs      Rust foreground terminal process (binary `koshell`)
crates/koshell-proto   Shared IPC message types
packages/ai-daemon     Bun AI daemon
docs/                  Public docs
reference/             Frozen pre-rewrite TypeScript prototype (not built)
```

## Requirements

- Rust 1.96 or newer (pinned via `rust-toolchain.toml`)
- Bun 1.3 or newer

Bun is the entire JS toolchain — package manager, task runner, test runner, the daemon's
runtime, and its packager. There is no Node or pnpm dependency. The floor is enforced
through `package.json` engines.

## Development

Rust:

```bash
cargo build            # build all crates
cargo test             # run Rust tests
cargo clippy           # lint
cargo fmt              # format
```

JS (Bun):

```bash
bun install
bun run check          # format check, lint, typecheck, and tests across packages
```

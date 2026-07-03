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
spawns re-sources your rc and skips the exec (`KOSHELL=1`), so your configuration loads
exactly once, inside the wrap. It stays inert for non-interactive shells, non-TTY
stdio, and `TERM=dumb`. To opt out for one shell, start it with `KOSHELL_NO_AUTO=1`; to
disable auto-wrap without a working shell, create `~/.config/koshell/no-auto` (see
`docs/design-0003-shell-init-auto-wrap.md`).

`#?` works in every form: type `#? <question>` (or `command #? <question>`) and the
question fires when the line's output completes or stabilizes. Directly launched
programs use the output-stabilization path, since shell integration markers only exist
inside bash/zsh.

Answers come from the AI daemon, started separately (one per user session):

```bash
pnpm --filter @koshell/ai-daemon start
```

Provider, model, and auth currently resolve through pi's own defaults: an existing pi
setup (`~/.pi/agent/auth.json`) or a provider API key in the environment (for example
`ANTHROPIC_API_KEY`) is enough. If the daemon is not running or no provider is
configured, the terminal keeps working and `#?` reports the degradation inline.

Both processes log at a configurable level, set by `--log-level <level>` or the
`KOSHELL_LOG` environment variable (the argument wins). The terminal owns the screen,
so its logs go to `$XDG_STATE_HOME/koshell/koshell.log` (default level `warn`,
`env_logger` filter syntax); the daemon logs to its own stdout (default level `info`).

## Architecture

Koshell is a hybrid monorepo with two runtimes:

- **`koshell-rs` (Rust, foreground)** — one process per terminal window. Owns the PTY,
  the terminal mirror, screen snapshots, alternate-screen detection, the timeline, local
  terminal context, and `#?` detection. It stays usable as a transparent shell wrapper
  even when the AI daemon is unavailable.
- **`koshell-ai-daemon` (Node.js, shared)** — one process per user session. Receives `#?`
  requests over IPC and (in a later stage) runs the pi-backed agent session, provider
  configuration, tool loop, and streaming AI responses.

The two communicate over newline-delimited JSON (JSONL) on a Unix domain socket.

## Repository layout

```
crates/koshell-rs      Rust foreground terminal process (binary `koshell`)
crates/koshell-proto   Shared IPC message types
packages/ai-daemon     Node.js AI daemon
docs/                  Public docs
reference/             Frozen pre-rewrite TypeScript prototype (not built)
```

## Requirements

- Rust 1.96 or newer (pinned via `rust-toolchain.toml`)
- Node.js 24 or newer
- pnpm 11 or newer

Node/pnpm floors are enforced through `.node-version`, `package.json` engines, and
`.npmrc` `engine-strict=true`.

## Development

Rust:

```bash
cargo build            # build all crates
cargo test             # run Rust tests
cargo clippy           # lint
cargo fmt              # format
```

Node:

```bash
pnpm install
pnpm check             # format check, lint, typecheck, and tests across packages
```

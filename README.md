# koshell

Koshell is a human-centric shared terminal: AI beside your terminal, not above it.

The project keeps the human as the primary terminal operator while giving AI enough
shared terminal context to explain, diagnose, and assist — without turning the terminal
into a separate chat room or an agent-owned execution loop. When the terminal raises a
question, you type `#?` and ask in place.

## Installation

Build both binaries, then install them (with the man pages) under `/usr/local`:

```bash
make                   # cargo release build + Bun-compiled daemon binary
sudo make install      # /usr/local/bin/{koshell,koshell-ai-daemon} + man pages
```

Run `make` and `make install` as two steps: installing does not rebuild, so
nothing runs under sudo except the file copies. For a user install without sudo:

```bash
make && make install PREFIX=$HOME/.local
```

Installing the two binaries side by side is what lets the terminal find and
auto-start the daemon with zero configuration (the daemon binary is a
self-contained Bun executable, so it is large — tens of MB). After installing,
`man koshell` and `man koshell.toml` document the CLI and the config format,
and `sudo make uninstall` removes exactly what was installed. See
`docs/design-0012-system-install-makefile-and-man-pages.md`.

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
terminal (the tty branded into `KOSHELL` matches its `$(tty)`), so your configuration
loads exactly once, inside the wrap. The marker is tty-scoped, so a shell that lands on a
fresh terminal — a new tmux pane — wraps itself instead, and `#?` works in every pane (see
`docs/design-0009-tty-scoped-nesting-marker.md` and
`docs/design-0017-consolidate-environment-into-koshell.md`). It stays inert for non-interactive
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

Provider, model, and auth come from Koshell's own config at
`$XDG_CONFIG_HOME/koshell/koshell.toml` (default `~/.config/koshell/koshell.toml`).
Export a provider key (or sign in with `koshell auth login`), then choose a model without
hand-editing TOML:

```bash
export ANTHROPIC_API_KEY=...       # export before the daemon starts
koshell model                      # searchable interactive picker
koshell model list                 # credential-available models
koshell model list --all sonnet    # full catalog, filtered; marks missing credentials
koshell model set anthropic/claude-sonnet-4-5
koshell model show                 # configured default and this conversation's active model
```

The builtin catalog is the embedded pi runtime's full provider catalog (30+ providers,
1,000+ models), so Koshell keeps no static model list. If a key is exported after the
daemon has already started, run `koshell daemon restart` so the daemon inherits it; this
still loses memory-only conversations. The resulting minimal config is:

```toml
model = "anthropic/claude-sonnet-4-5"
```

Most providers need nothing beyond their conventional API-key environment variable
(`man koshell.toml` lists the common ones). Or put the key in the config instead:

```toml
model = "anthropic/claude-sonnet-4-5"

[providers.anthropic]
api_key = "sk-ant-..."       # literal, "$ENV_VAR", or "!command" (e.g. a keychain read)
```

Subscription providers sign in interactively instead: `koshell auth login anthropic`
(Claude Pro/Max), `github-copilot`, or `openai-codex` (ChatGPT Plus/Pro) runs the
provider's OAuth flow and stores the token in `$XDG_DATA_HOME/koshell/auth.json`;
`koshell auth status` shows what is configured and from where.

Inside Koshell, `koshell model` and `model set` switch the current conversation in
place **and** save the choice as the default for future conversations. Add
`--session-only` for a temporary switch that leaves `koshell.toml` byte-for-byte
unchanged. Other terminal conversations keep their own active models. A switch waits for
an in-progress answer and preserves the transcript; if the retained history cannot fit
the target context window, Koshell rejects it instead of dropping or summarizing history.
Outside Koshell, selection updates only the future default.

After manually editing `koshell.toml`, run `koshell reload` for the current terminal or
`koshell reload --all`. A model-only reload uses the same transcript-preserving switch;
provider, credential, thinking-level, or other construction changes rebuild the affected
agent and explicitly report that its in-memory transcript was discarded. Invalid config
is rejected without disturbing a running session. `koshell status` reports the current
instance's active model and conversation state.

Conversations are currently memory-only. Model switches no longer discard them, but a
provider-definition rebuild, terminal disconnect, or daemon restart still leaves no
conversation to resume. Terminal context is likewise bounded:
the daemon receives the current screen and a recent pushed window, but it cannot yet pull
older off-screen output. In dogfooding, output longer than one screen can therefore leave
the AI with only the currently visible portion. Transcript persistence and the read-only
terminal context tool loop are not implemented yet.

A custom provider is a full block (`api`, `base_url`, `api_key`, and at least one
model); this is also how you pin a non-default API type such as `openai-responses`:

```toml
model = "mycorp/mycorp-large"

[providers.mycorp]
api = "openai-completions"
base_url = "https://api.mycorp.example/v1"
api_key = "$MYCORP_API_KEY"

  [[providers.mycorp.models]]
  id = "mycorp-large"
```

The config selects one default model, and each conversation has one active model at any
instant. `koshell model` updates only the root `model` assignment through a locked,
source-preserving atomic replacement, retaining comments, formatting, custom providers,
and file permissions. If the config is missing or invalid, the terminal keeps working
and `#?` reports what to fix inline. See
`docs/design-0018-model-discovery-and-runtime-switching.md`.

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
  requests over IPC, owns Koshell provider/model/auth resolution, and runs one pi-backed
  streaming agent conversation per terminal session. The read-only terminal context tool
  loop is not wired yet, so context is push-only. Its source uses `node:` APIs only, so
  Bun is the runtime and packager, not an API dependency.

The two communicate over newline-delimited JSON (JSONL) on a Unix domain socket.

## Repository layout

```
crates/koshell-rs      Rust foreground terminal process (binary `koshell`)
crates/koshell-proto   Shared IPC message types
packages/ai-daemon     Bun AI daemon
man/                   Hand-written man pages (koshell.1, koshell.toml.5)
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

`make check` runs the full validation for both runtimes with the same commands
as CI.

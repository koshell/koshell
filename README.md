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

`#?` works in every form: type `#? <question>` (or `command #? <question>`) and the
question fires when the line's output completes or stabilizes. Directly launched
programs use the output-stabilization path, since shell integration markers only exist
inside bash/zsh.

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

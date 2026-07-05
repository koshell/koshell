# Contributing to koshell

Thanks for your interest in contributing! koshell is a human-centric shared terminal:
a Rust foreground terminal process (`koshell-rs`) beside a Bun AI daemon
(`koshell-ai-daemon`). This guide covers how to build, test, and submit changes.

## Ground rules

- Be respectful. This project follows our [Code of Conduct](CODE_OF_CONDUCT.md).
- Keep terminal-core (Rust) free of any LLM provider or AI SDK dependency; AI concerns
  live in the AI daemon behind the IPC boundary. See [`docs/architecture.md`](docs/architecture.md).
- The daemon's source uses `node:` APIs only — Bun is its runtime and packager, not an
  API surface.
- All code, configuration, and comments are in English.

## Prerequisites

- Rust 1.96 or newer (pinned via `rust-toolchain.toml`)
- Bun 1.3 or newer (the entire JS toolchain — no Node or pnpm needed)

## Development

Rust:

```bash
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt
```

JS (Bun):

```bash
bun install
bun run check   # format check, lint, typecheck, and tests across packages
```

Please make sure both `cargo test` / `cargo clippy` / `cargo fmt --check` and `bun run check`
pass before opening a pull request. CI runs the same checks.

## Submitting changes

1. Fork the repository and create a branch from `main`.
2. Make your change with focused commits and clear messages.
3. Add or update tests and documentation for behavior changes.
4. Open a pull request against `main` and fill in the template. Describe the motivation,
   the change, and how you verified it.

## Project layout

```
crates/koshell-rs      Rust foreground terminal process (binary `koshell`)
crates/koshell-proto   Shared IPC message types
packages/ai-daemon     Bun AI daemon
docs/                  Architecture and change records
reference/             Frozen pre-rewrite TypeScript prototype (not built)
```

## Reporting bugs and requesting features

Use the issue templates. For security-sensitive reports, please avoid filing a public
issue and contact the maintainers privately instead.

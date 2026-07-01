# Koshell Agent Instructions

## Repository Scope

This repository is the Koshell product source workspace. It is a hybrid monorepo:

- Rust crates under `crates/` implement the foreground terminal process
  (`koshell-rs`, binary `koshell`) and the shared IPC types (`koshell-proto`).
- Node.js packages under `packages/` implement the AI daemon (`@koshell/ai-daemon`).
- `reference/` is a frozen snapshot of the pre-rewrite TypeScript prototype, kept for
  algorithm and behavior reference only. It is excluded from the build, lint, and format.
  Do not add new work there.

Keep it focused on buildable source code, public-facing project metadata, and minimal
operational instructions.

Do not add long-term architecture plans, roadmap notes, future feature specifications,
commercial-edition design notes, extended validation suites, private fixtures, or
evaluation assets to this repository unless the project owner explicitly asks for that
change. Those live in the internal organization workspace.

## Source Repository Guidance

- Keep README and package metadata limited to what users and source contributors need to
  run or inspect the product.
- Terminal-core (PTY, mirror, snapshots, timeline, local context) lives in Rust and must
  not depend on any LLM provider or the pi packages.
- Provider/model/auth, the pi-backed agent session, and the tool loop live in the Node AI
  daemon, behind the IPC boundary.
- The two runtimes communicate only through `koshell-proto` messages over the JSONL Unix
  socket. Do not introduce hidden coupling across the boundary.
- Keep public tests small and source-focused. Extended validation, private fixtures, and
  commercial-edition tests belong in the internal workspace when available.
- Keep all code, configuration, and comments in English.

## Validation

- Rust: `cargo test`, `cargo clippy`, `cargo fmt --check`.
- Node: `pnpm check` (format check, lint, typecheck, tests).

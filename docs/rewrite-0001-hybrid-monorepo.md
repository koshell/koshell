# Rewrite 0001: hybrid Rust + Node monorepo

## Why

The previous single-package TypeScript koshell was a half-finished shell wrapper: the
timeline, terminal-context, ai-context, and screen-diff modules existed only as
test-driven libraries and were never wired into the running process. The project owner
decided to redo the repository around the hybrid architecture already recorded in the
internal workspace (`target-architecture.md`): a native Rust foreground terminal process
plus a Node.js AI daemon.

Decisions confirmed for the rewrite:

- Single monorepo (Cargo workspace + pnpm workspace side by side).
- Rust terminal emulation via `alacritty_terminal` (replacing `@xterm/headless`).
- `#?` detection via OSC shell-integration markers only (no keystroke reconstruction or
  prompt-history fallback in the MVP). _Superseded 2026-07-02: `#?` is now also detected
  inside foreground CLI programs via output stabilization; see
  `design-0001-repl-command-completion.md`._

Scope for the current stage: complete the Rust terminal-core (Phases 1–4) and a minimal
Node receiver that can accept a `#?` request over IPC (Phase 5-min). pi integration,
provider configuration, the tool loop, and streaming AI responses are deferred to the
next stage.

## When

Performed at: 2026-07-01 11:10 CST +0800.

## How (Phase 0)

- Froze the old TypeScript project (`src/`, `test/`, its build config, and the old
  per-feature docs) under `reference/`, excluded from build, lint, and format.
- Created a Cargo workspace with `crates/koshell-rs` (binary `koshell`) and
  `crates/koshell-proto` (shared IPC message types), pinned via `rust-toolchain.toml`.
- Created a pnpm workspace with `packages/ai-daemon` (`@koshell/ai-daemon`), root scripts
  delegating to per-package `lint`/`typecheck`/`test`, and shared Prettier at the root.
- Rewrote `README.md` and `AGENTS.md` for the dual-runtime structure and added
  `docs/architecture.md`.
- Verified `cargo build`/`test`/`clippy`/`fmt --check` and `pnpm check` are green.

## Open issues

- CI ownership and the target runner (esp. PTY-capable) are not decided yet; there is no
  configured remote. Internal validation ownership is tracked in the internal workspace.
- `context_package` is carried as opaque JSON on the IPC wire; its structured shape will
  be pinned when the Rust context module is built (Phase 3) and the daemon consumes it.

## Resolution conditions

- Decide CI ownership and PTY-capable runner requirements when remotes are configured.
- Pin the `context_package` schema once the terminal context assembly is dogfooded.

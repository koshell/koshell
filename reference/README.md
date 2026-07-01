# Frozen reference (pre-rewrite TypeScript prototype)

This directory is a frozen snapshot of the original single-package TypeScript
implementation of koshell, kept as behavioral and algorithmic reference for the
hybrid rewrite (Rust terminal-core + Node AI daemon).

It does not participate in the workspace build and is excluded from linting and
formatting. Do not add new work here. Authoritative algorithms to port:

- `src/shell.ts` — shell resolution, PTY env filtering, `KOSHELL` nested guard.
- `src/timeline.ts` — `TerminalEvent` model and in-memory store queries.
- `src/screen-diff.ts` — line-level LCS diff, summary, hunks.
- `src/terminal-context.ts` — terminal context selection heuristics.
- `src/ai-context.ts` — cache-aware AI context contract (stays conceptually in Node).
- `src/terminal-mirror.ts` / `src/terminal-session.ts` — xterm mirror + PTY orchestration.

See `../docs/` and the plan for how each maps into the new architecture.

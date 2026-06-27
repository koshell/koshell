# koshell

Koshell is a human-centric shared terminal: AI beside your terminal, not above it.

The project aims to keep the human as the primary terminal operator while giving AI enough shared terminal context to explain, diagnose, and assist without turning the terminal into a separate chat room or an agent-owned execution loop.

This repository currently contains the early local runtime foundation: a Node.js and TypeScript shell wrapper that starts a real shell through `node-pty` and mirrors PTY output into a headless xterm instance. That mirrored terminal state is the basis for future context-aware assistance.

## Requirements

- Node.js 24 or newer
- pnpm 11 or newer

The repository uses `.node-version`, `package.json` engines, and `.npmrc` `engine-strict=true` to enforce the minimum runtime and package-manager versions.

## Usage

Install dependencies:

```bash
pnpm install
```

Start an interactive shell wrapper:

```bash
pnpm start
```

`koshell` must be started from an interactive TTY. It forwards stdin to the child shell, writes child-shell output to stdout, and keeps the same output mirrored in headless xterm state.

## Scripts

- `pnpm start`: run `src/index.ts` directly with `node`.
- `pnpm format`: format files with Prettier and list only changed files.
- `pnpm format:check`: check Prettier formatting.
- `pnpm lint`: run ESLint.
- `pnpm typecheck`: run TypeScript without emitting files.
- `pnpm test`: run the basic Vitest suite once.
- `pnpm check`: run formatting check, linting, type checking, and basic tests.

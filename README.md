# koshell

A Node.js and TypeScript project initialized for direct `.ts` execution with Node.js, Vitest, ESLint, Prettier, Husky, and lint-staged.

## Requirements

- Node.js 24 or newer
- pnpm 11 or newer

The repository uses `.node-version`, `package.json` engines, and `.npmrc` `engine-strict=true` to enforce the minimum runtime and package-manager versions.

## Scripts

- `pnpm start`: run `src/index.ts` directly with `node`.
- `pnpm format`: format files with Prettier and list only changed files.
- `pnpm format:check`: check Prettier formatting.
- `pnpm lint`: run ESLint.
- `pnpm typecheck`: run TypeScript without emitting files.
- `pnpm test`: run the basic Vitest suite once.
- `pnpm check`: run formatting check, linting, type checking, and basic tests.

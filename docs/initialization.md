# Project Initialization Notes

## Requirement

This work initialized the repository as a Node.js and pnpm TypeScript project. The setup uses Node.js 24+, pnpm 11+, Prettier, ESLint, Husky, TypeScript, lint-staged, Vitest for basic public tests, direct `.ts` execution through `node`, and a `pnpm check` command that verifies the source workspace.

## Timestamp

Performed at: 2026-06-27 12:41:05 CST +0800.

## Implementation

- Added `.node-version` with major version `24`.
- Added package engine requirements for `node >=24` and `pnpm >=11`.
- Added `.npmrc` with `engine-strict=true` so pnpm enforces the engine requirements.
- Omitted the `packageManager` field after user confirmation because Corepack-style package manager fields require exact versions and would conflict with the requirement to avoid pinning a minor version.
- Added TypeScript configuration for strict ESM and direct Node.js `.ts` execution.
- Added Prettier with an empty `.prettierrc` file containing only `{}`.
- Added ESLint flat config with TypeScript-aware rules and Prettier compatibility.
- Added Vitest with a minimal public source test.
- Added Husky pre-commit integration that runs lint-staged.
- Added `pnpm check` to run formatting checks, linting, type checking, and basic tests.

## Boundary Update

Updated at: 2026-06-27 13:06:10 CST +0800.

Basic public tests are allowed in this source repository. Extended validation, private fixtures, evaluation assets, and paid-edition tests belong in the internal organization workspace when available.

## Open Issues

No functional open issues are known at initialization time.

## Resolution Conditions

If future tooling changes require exact package manager versions, revisit the package-manager policy with the project owner before introducing a Corepack `packageManager` field or a pinned pnpm minor version.

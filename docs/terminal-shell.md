# Terminal Shell Implementation

## Requirement

Implement a Node.js program, informed by `../coshell`, that starts a new shell through `node-pty` and keeps an xterm-backed usable terminal copy while the shell remains interactive. Add unit tests that verify the core behavior.

## Timestamp

Performed at: 2026-06-27 17:42:07 CST +0800.

## Implementation

- Added `node-pty`, `@xterm/headless`, and `@xterm/addon-serialize` as runtime dependencies.
- Approved `node-pty` build scripts through pnpm, which created `pnpm-workspace.yaml` with `allowBuilds.node-pty: true`.
- Deliberately omitted a `node-pty` permission-fix `postinstall` workaround after confirming the approved build flow allows a direct PTY smoke test to start successfully in this workspace.
- Added `src/shell.ts` for shell resolution, PTY environment filtering, and nested `koshell` detection.
- Added `src/terminal-mirror.ts` for a headless xterm terminal mirror with snapshots, serialization, resize, and disposal.
- Added `src/terminal-session.ts` for PTY spawning, stdin/stdout forwarding, xterm mirroring, resize propagation, exit handling, and signal cleanup.
- Replaced the initialization demo entrypoint with an interactive shell wrapper in `src/index.ts`.
- Removed the initial `add()` sample source and test because they no longer represented the product behavior.
- Added public unit tests for shell resolution, PTY environment creation, nested-start prevention, terminal mirroring, and PTY session forwarding through mocks.

## Nested-Start Prevention

Updated at: 2026-06-27 17:57:32 CST +0800.

`createPtyEnv()` marks the child shell with `KOSHELL=1`. Startup checks this marker through `assertNotNestedKoshell()` before spawning another PTY shell. If a user runs `koshell` from inside an existing `koshell` child shell, startup fails with a clear error instead of creating a nested terminal wrapper.

## Fallback PATH

Updated at: 2026-06-27 17:59:51 CST +0800.

The PTY environment fallback `PATH` intentionally uses only system paths: `/usr/bin:/bin:/usr/sbin:/sbin`. Homebrew paths are not injected by default; if the parent environment provides a non-empty `PATH`, koshell preserves it instead.

## Open Issues

The unit tests intentionally mock PTY process behavior instead of launching a real interactive shell. This keeps public tests deterministic and avoids environment-sensitive shell behavior.

The project currently relies on pnpm build approval for `node-pty` instead of carrying a local permission-fix install workaround.

## Resolution Conditions

Add an end-to-end PTY smoke test only if the project owner wants environment-sensitive validation in this source repository and accepts the extra CI/runtime assumptions required for real shell spawning.

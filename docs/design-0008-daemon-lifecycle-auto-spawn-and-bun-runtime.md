# Design 0008 — daemon lifecycle: auto-spawn and the Bun runtime

Date: 2026-07-04 23:00:41 CST

Status: accepted, implementation in progress.

## Why

The AI daemon has so far required a manual `pnpm -F ai-daemon start` before any
`#?` could be answered — the single biggest piece of dogfooding friction, and a
non-starter for anyone else. Daemon lifecycle (who starts it, how it is found,
how it restarts) was an explicitly unresolved question; this design resolves
it: **the terminal starts the daemon on demand, and the daemon runs on Bun.**

Two findings drove the runtime choice:

- pi-coding-agent — the daemon's AI substrate — ships its own official
  releases as Bun-compiled single binaries for six platforms, maintaining the
  hairy parts (WASM asset paths, worker entry points) upstream. Betting on
  Bun rides that maintained path instead of fighting it.
- Empirically, on this repo, Bun 1.3 runs the daemon's TS source directly
  (protocol smoke passes; socket ready in ~200ms) and `bun build --compile`
  produces a working single binary with zero configuration. A distributed
  koshell can therefore be two self-contained executables — the Rust terminal
  and the compiled daemon — with no "which Node is installed" question.

The discipline that keeps this reversible: **daemon source code uses `node:`
APIs only.** Bun is the runner and the packager, never an API surface. The
code never imports `bun:*` outside tests, so a runtime change back (or
onward) is a packaging decision, not a rewrite.

## Semantics

### Daemon startup (single instance per user)

The socket is the lock. On startup the daemon resolves the socket path
(`$XDG_RUNTIME_DIR/koshell/daemon.sock`, the existing chain), rejects paths
longer than the portable `sun_path` limit (Bun silently "listens" on
overlong paths without being connectable — guarded explicitly), then probes
any existing socket file by connecting to it:

- **alive** (connect succeeds) — another daemon is serving; exit 0. The
  terminal that spawned this process connects to the incumbent.
- **stale** (connect refused / times out) — unlink the leftover file and
  bind.
- **absent** — bind.

A bind failure with `EADDRINUSE` re-probes: alive → exit 0 (lost a benign
race to a healthy winner); otherwise exit 1. Two terminals racing to spawn
therefore converge on one daemon. The residual probe→unlink→bind window is
accepted (see open issues).

### Idle exit

The daemon tracks live connections. When the count reaches zero — including
at startup, before any terminal has connected — a 10-minute timer arms; a
new connection cancels it; expiry logs, closes the server, removes the
socket, and exits 0. Respawn costs ~200ms, so nothing of value is lost, and
stale-code daemons drain away naturally after a rebuild: the restart story
is "kill it (or wait); the next `#?` respawns it".

### Auto-spawn from the terminal

`#?` dispatch already connects lazily. On connect failure the terminal now
resolves a daemon command and spawns it fully detached (double-fork through
`/bin/sh -c 'exec … &'`; stdin from `/dev/null`, stdout/stderr appended to
`$XDG_STATE_HOME/koshell/daemon.log`), then retries the connect for up to
one second (50ms steps; empirically the daemon is connectable in ~200ms).
The command resolution chain:

1. `KOSHELL_NO_DAEMON_SPAWN` non-empty — never auto-spawn (the
   `KOSHELL_NO_EVENT_LOG` escape-hatch convention).
2. `KOSHELL_DAEMON_CMD` non-empty — used verbatim as the command line.
3. A `koshell-ai-daemon` executable next to the `koshell` binary — the
   installed layout.
4. A `koshell-ai-daemon` executable on `PATH`.

There is deliberately no source-tree fallback. Baking the build-time repo
path into the binary (via `CARGO_MANIFEST_DIR`) would leak that path and
break the moment the binary is moved or installed elsewhere. In development,
point `KOSHELL_DAEMON_CMD` at the source — for example
`bun packages/ai-daemon/src/index.ts` — or build the daemon binary and put it
on `PATH`.

Spawn attempts are limited to one per 30 seconds per terminal session: a
daemon that dies gets respawned on a later `#?`, while a broken command
costs at most one cheap `sh` fork per 30s and the existing explicit
"AI daemon unavailable" degrade in between. The dispatch-time retry only
runs in the seconds right after this session actually spawned something, so
the terminal never stalls for a daemon someone else is starting.

There is deliberately no session-start pre-warm: a session that never uses
`#?` never materializes a daemon. The cost is ~250ms on the first question;
the event log's `submit_to_dispatch_ms` will show whether that ever matters.

### The `koshell daemon` subcommand

Manual lifecycle control, no PTY, no session:

- `koshell daemon status` — probes the socket; when alive, asks the daemon
  over IPC and prints pid, version, protocol version, uptime, connection
  count, plus the socket and log paths. Exit 0 when running, 1 when not.
- `koshell daemon start` — spawns via the resolution chain when not running.
  `KOSHELL_NO_DAEMON_SPAWN` is ignored here: it gates _auto_-spawn; an
  explicit command is the user's intent.
- `koshell daemon stop` — asks the running daemon for its pid, sends
  SIGTERM, waits for the socket to disappear.
- `koshell daemon restart` — stop (when running), then start.

Like `shell-init`, the `daemon` name shadows a program literally called
`daemon`; the path form (`koshell ./daemon`) still launches such a program.

### The status protocol pair

One additive message pair, no protocol version bump (the additive-evolution
rules from design 0004): the client sends `{"type": "status_request"}` and
the daemon replies `{"type": "status", pid, version, protocol_version,
uptime_ms, connections}`. Status is served **without** a hello handshake —
diagnostics from a version-mismatched terminal is exactly the use case.

## Decision

- Bun ≥ 1.3 is the daemon's runtime and packager: `bun src/index.ts` in
  development, `bun build --compile` for distribution. There is no Node
  execution path to maintain — no dual scripts, no dual CI. The
  `node:`-APIs-only source discipline is what makes that safe.
- Daemon tests run under `bun test` (the suite uses only
  describe/it/expect); vitest is removed.
- Bun takes over the whole JS toolchain: it is the package manager
  (`bun install`, `bun.lock`, workspaces in `package.json`) and task runner
  (`bun run …`), and it drives the existing dev tools (tsc, eslint, prettier).
  pnpm, `.node-version`, and the pnpm-oriented `.npmrc` are gone; CI installs
  and runs everything through Bun alone.
- The daemon-side startup/probe logic lives in a new `lifecycle.ts`; the
  Rust-side resolution/spawn logic in a new `daemon_spawn.rs`; the
  subcommand in `daemon_cli.rs`. `ipc.rs` stays a pure transport client.
- A protocol-level smoke script (spawn daemon → version-mismatched hello →
  expect the rejection) runs in CI against both the TS source under bun and
  the compiled binary. It proves startup, socket serving, framing, and the
  rejection path without touching providers or the network.
- Manual foreground debugging stays trivial: `bun packages/ai-daemon/src/index.ts`
  logs to stdout, exactly as before. Auto-spawned daemons log to
  `$XDG_STATE_HOME/koshell/daemon.log`.

## Open issues

- The probe→unlink→bind sequence has a TOCTOU window where a loser could
  unlink a winner's freshly bound socket. The EADDRINUSE re-probe makes the
  common race convergent; the pathological interleaving degrades one `#?`
  and self-heals on the next spawn window. An flock-based bind lock is the
  known fix if it ever shows up in practice.
- `KOSHELL_DAEMON_CMD` is spliced into `sh -c 'exec <cmd> </dev/null >>… &'`;
  command strings containing their own `&`, redirects, or comments will
  misbehave. It is an expert escape hatch; documented, not engineered around.
- `stop` obtains the pid over IPC, so a daemon that accepts connections but
  never replies cannot be stopped this way; the documented fallback is a
  manual kill. No pidfile machinery unless this bites.
- No pre-warm. Revisit with `submit_to_dispatch_ms` distributions from the
  dogfooding event log (design 0007) if the first-question stall is felt.
- A koshell upgrade does not proactively restart a running older daemon;
  the hello version enforcement (design 0004) reports the mismatch, and
  `koshell daemon restart` (or the idle exit) resolves it. Automatic
  same-version-but-stale-code detection is out of scope.

## Verification

- Daemon unit tests under `bun test`: socket probe states (absent, stale,
  alive), idle-exit arming/suppression/re-arming, the socket-path length
  guard, `status_request` served without hello.
- Protocol smoke in CI: `bun src/index.ts` and the compiled binary both
  answer a version-mismatched hello + request with `ack` then the
  version-mismatch `ai_error`.
- Rust unit tests: resolution-chain precedence (env command, adjacent binary,
  `PATH` binary), shell quoting, the 30s cooldown.
- Real-PTY e2e: with `KOSHELL_DAEMON_CMD` pointing at a stub, a `#?` with no
  daemon running auto-spawns exactly one stub and streams its answer;
  with `KOSHELL_NO_DAEMON_SPAWN=1` it degrades with the unavailable notice
  and spawns nothing.
- Non-PTY e2e for the subcommand: `status` exits 1 with no daemon;
  start → status → stop round-trips against the stub.
- All seven existing PTY test harnesses set `KOSHELL_NO_DAEMON_SPAWN=1`, so
  their no-daemon scenarios stay hermetic on machines where the dev
  fallback would otherwise spawn a real daemon.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test`, `bun run check`.
- The end-to-end acceptance the smoke cannot cover: a real `#?` answered by
  a real pi session under Bun, in daily dogfooding.

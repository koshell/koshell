# Design 0024 — `koshell version` and build-time version stamping

Date: 2026-07-30 15:57:40 CST (+0800)

Status: implemented.

## Why

Three koshell processes can be running on one machine at the same time, all from different
builds, and until now none of them could say which:

- **The binary on `PATH`** — what a new terminal would start.
- **The koshell wrapping the terminal you are typing in.** A shell `exec`s into koshell once
  and keeps that process for the life of the terminal; installing a new build does not reach
  it. This is the version that actually decides what `#?` does in front of you, and it is the
  one nothing reported.
- **The AI daemon**, a separate long-lived process that outlives terminals, is auto-spawned,
  and idles out on its own schedule. It can predate every terminal attached to it.

`--version` answered only the first, and answered it with `0.1.0` from `Cargo.toml` — a
constant that has not changed since the rewrite and cannot distinguish two builds anyway.
So the two questions dogfooding kept producing, "did my rebuild take effect here?" and "is
this terminal still on the old one?", had no answer at all.

## The command

```
$ koshell version
koshell:            20260730.074058  (this binary, protocol v1)
this tty:           20260729.183304  (/dev/ttys003, pid 4821)
  (a different build than this binary — a new terminal runs 20260730.074058)
koshell-ai-daemon:  20260730.074101  (pid 5120, protocol v1)
```

Three rows, one per process, values in one column so they can be compared by eye. Notes
under a row carry the actionable half: a wrapper on a different build, a protocol mismatch
with the daemon, or how to get a daemon at all.

`koshell --version` is unchanged in shape and now prints the stamp
(`koshell 20260730.074058`). It stays the answer to "what is this binary", which is what a
package manager or a bug report wants; the subcommand is for the three-way comparison.

**The exit code is always 0**, unlike `koshell status` and `koshell daemon status`, which
are probes whose non-zero exit _is_ their answer. Every state this command can find is a
state it reports; "the daemon is not running" is a fact, not a failure to produce one.

### Where each version comes from

**This binary** — a compile-time constant (`koshell_rs::VERSION`).

**This tty** — a file beside the tty's liveness marker, written by the wrapper at startup and
removed with it: `<runtime_dir>/koshell/tty/<escaped-tty>.version`, holding the wrapper's
version. A child process cannot ask the process above it anything, and the two channels that
already exist are both wrong for this:

- The **`KOSHELL` brand** would mean a third comma-separated field, and existing readers
  take everything after the first comma as the tty (`${KOSHELL#*,}` in the auto-wrap
  snippet, `str::split_once` in `shell::koshell_tty`). An older koshell or an older
  installed snippet reading a new wrapper's brand would compare `/dev/pts/3,20260730.074058`
  against `$(tty)`, conclude this terminal is unwrapped, and `exec` a second koshell inside
  the first. A version string is not worth reopening the nesting guard.
- The **daemon** knows a connection's terminal, but a terminal connects lazily on its first
  `#?`. `koshell version` in a fresh terminal would report "unknown" for the row that
  matters, and would report nothing at all with no daemon installed.

The marker file itself could not carry it either, for the same reason as the brand: its body
is read as `kill -0 "$(cat …)"`, so a second line there reads as a dead wrap. Hence a
sibling file — additive, invisible to every existing reader, and load-bearing for nothing.
It is written _before_ the pid marker and removed _after_ it, so a reader that finds a live
marker always finds the version beside it.

The row is resolved from **this process's controlling tty**, not from the inherited brand:
the brand can outlive the koshell that wrote it and is inherited across tty boundaries. A
brand naming a different terminal (what a new tmux pane inherits) reports "not wrapped by
koshell" — that pane's koshell is somebody else's, and the pane genuinely has none.

**koshell-ai-daemon** — the running daemon, over the additive `status_request`/`status` pair
`koshell daemon status` already uses. Three outcomes: a version, `unknown` (reachable but
silent — a daemon predating `status_request`, so `koshell daemon restart`), or `not running`
plus the command that would start it.

### Why a stopped daemon's version is not reported

`koshell version` is read-only. It would be possible to run the resolved daemon executable
with `--version` and report an installed-but-stopped daemon, and that was considered and
rejected: a command that prints versions must be safe to run anywhere, and a daemon
predating this design does not recognize `--version` — it would parse it as nothing, find no
daemon on the socket, and _become_ the daemon. Starting a background process as a side
effect of asking a question is exactly the surprise this project avoids elsewhere.

The daemon does now answer `koshell-ai-daemon --version` for anyone who wants to ask it
directly. `koshell version` still does not ask.

## Build-time stamping

There is no hand-maintained release number, and one carried in `Cargo.toml` /
`package.json` would report the same value for every build forever — useless for the
comparison above. Both artifacts resolve their version when they are built:

1. **`KOSHELL_VERSION`** — an explicit version, for a release build
   (`make KOSHELL_VERSION=1.2.0`).
2. **The exact tag on `HEAD`** (`git describe --tags --exact-match`), leading `v` stripped:
   a tagged checkout stamps its release without being told.
3. **The build's UTC timestamp**, `YYYYMMDD.HHMMSS`.

The timestamp format is sortable as text, safe in a filename or a tag, and identical on two
machines building the same moment — none of which a local-time or ISO-with-colons form would
be. UTC is deliberate: build times are compared across machines far more often than they are
read as wall-clock.

The chain lives in three places, which is a duplication accepted for what it buys:

- `crates/koshell-rs/build.rs` stamps `KOSHELL_BUILD_VERSION` for the terminal. It shells out
  to `git` and `date` rather than adding a date or git crate: this crate is Unix-only, those
  two tools _define_ the formats involved, and a hand-rolled civil-date conversion would be
  arithmetic no test in this package can reach (`cargo test` does not run build scripts).
- `packages/ai-daemon/scripts/version.ts` does the same for the daemon and is unit-tested,
  since it is ordinary TypeScript. `scripts/build-binary.ts` passes the result to
  `bun build --define`, substituting the `KOSHELL_BUILD_VERSION` identifier in `index.ts`
  with a string literal while parsing. A _declared global_ rather than a `process.env` read:
  `--define` substitutes bare identifiers, and `process` is an explicit `node:process`
  import there, so a `process.env.X` form would silently never be replaced — a failure that
  looks exactly like a successful build.
- The **Makefile** resolves the chain once and exports `KOSHELL_VERSION`, so a single `make`
  cannot stamp two different build times into koshell and the daemon.

A source run of the daemon (`bun src/index.ts`, the development setup) has no substitution
and reports `0.1.0+source` — nothing stamped it, and saying so beats a bare `0.1.0` that
reads like a stale binary in the three-way comparison.

### Rebuild cost

`build.rs` declares `rerun-if-env-changed=KOSHELL_VERSION` plus the package's files, so
`make` with no tag on `HEAD` changes the variable on every invocation and rebuilds
`koshell-rs` (only that crate; dependencies are untouched). That includes `make check`. A
plain `cargo build` / `cargo test` inherits nothing from `make`, resolves its own timestamp,
and re-runs the build script only when a package file changes — so the ordinary development
loop is unaffected.

## Accepted consequences and residuals

- **`version` is a reserved subcommand name**, the residual every koshell subcommand carries:
  a program literally named `version` needs a path form (`koshell ./version`).
- **A wrapper predating this design records no version**, and its row reports `unknown` with
  the pid that owns the terminal. That is the honest answer during exactly one upgrade.
- **"A different build" is not "an older build".** Two stamps sort, but a stamp and a tag do
  not, so the note says the builds differ and names what a new terminal would run rather than
  claiming a direction.
- **The daemon and the terminal are compared only on protocol version**, not on build
  version: they are independent artifacts, and a mismatch matters only when it breaks the
  handshake, which is what the protocol row says.
- **A `SIGKILL`ed wrapper leaks its version file** alongside its marker, exactly as before —
  and the pid the marker names is then dead, so the terminal reports "no live marker" rather
  than a stale version.

## Tests

- `cli.rs` — `version` parses, takes no arguments, and a path form still launches a program
  of that name; `--version` prints the stamp rather than the `Cargo.toml` constant.
- `shell.rs` — the version file sits beside the marker and never in it; a recorded version
  reads back; a missing, empty, or blank one is "unknown" rather than an empty version.
- `version_cli.rs` — every row and note in isolation (`format_lines` is pure): a wrapper on
  another build is called out, a matching one is not, an older wrapper is `unknown` with its
  pid, a protocol mismatch names its fix, a silent daemon is not mistaken for a stopped one,
  and the three values share one column. Plus `inspect_tty` (outside, tmux-pane brand, stale
  brand) and `inspect_daemon` against a stub socket in each of its three states.
- `tests/version_cli_pty.rs` — the whole loop in a real session: the wrapper writes its
  version where a child finds it, so both terminal rows name this build; a version file
  rewritten in place (the state an older wrapper leaves behind) is reported as a different
  build, which also pins the path convention, since the shell reconstructs it from `$(tty)`
  exactly as `shell::tty_version_path` does; and outside a session the command still
  succeeds and says the terminal is unwrapped.
- `packages/ai-daemon/test/version.test.ts` — the resolution chain and the UTC stamp,
  including blank sources and the cross-timezone equality the format exists for.

## Open issues

- **The installed-but-stopped daemon's version stays unknowable** without executing it. If
  dogfooding shows people hitting it, the safe form is a daemon that writes its version into
  the state directory when it starts, which would also cover a daemon that has since exited.
- **`build.rs` is untested** — build scripts are outside `cargo test`. Its logic is a
  three-rung chain verified by hand and mirrored by the tested TypeScript implementation;
  the day it grows a fourth rung it should move into the library and be called from the
  build script.
- **The man page no longer names a version** in its `.TH` header, since a build stamp there
  would go stale on every build. If distributions want one, the install step is the place to
  substitute it.

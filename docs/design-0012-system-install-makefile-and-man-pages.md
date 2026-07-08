# Design 0012 — system install: Makefile and man pages

Date: 2026-07-08 14:18:41 CST

Status: accepted, implemented.

## Why

Koshell's distribution premise is two self-contained executables (design 0008):
the Rust terminal `koshell` and the Bun-compiled daemon `koshell-ai-daemon`,
found at runtime by looking next to the `koshell` binary, then on `PATH`. Until
now nothing turned a source checkout into that layout: there was no Makefile, no
install script, and no README section describing a system install. Anyone
outside the source tree had to hand-copy binaries or export
`KOSHELL_DAEMON_CMD` forever. There was also no manual page, so `koshell(1)`
and the config format had no offline, conventional documentation surface.

This design adds a root `Makefile` that builds, installs, and uninstalls both
runtimes, and two hand-written man pages installed alongside the binaries.

## Semantics

### Install layout

`make install` places, under `$(DESTDIR)$(PREFIX)` (default `PREFIX=/usr/local`):

- `bin/koshell` and `bin/koshell-ai-daemon` — side by side, which is exactly
  what the daemon discovery chain's "adjacent" rung expects
  (`crates/koshell-rs/src/daemon_spawn.rs`), so a fresh install answers `#?`
  with zero configuration beyond `config.toml`.
- `share/man/man1/koshell.1` and `share/man/man5/koshell.toml.5`.

`PREFIX` and `DESTDIR` follow the usual contract: `PREFIX` is the runtime
install root (`make install PREFIX=$HOME/.local` for a user install), `DESTDIR`
is a staging prefix for packaging. `make uninstall` removes exactly the four
installed files and never touches user configuration or state.

### Man pages

Two pages, split along the two user-visible surfaces:

- `koshell(1)` — the binary: subcommands, options, the `#?` interaction,
  environment variables, file paths, exit status.
- `koshell.toml(5)` — the config format: the single-active-model rule, the
  builtin-auth vs. custom-provider block shapes, credential syntax, error
  behavior. The page is named for the format; the file on disk is
  `config.toml`.

They are hand-written troff (classic man macros) checked in under `man/`, so
the man source is the install artifact: no scdoc/pandoc build dependency, and
both groff (Linux) and mandoc (macOS) render them. `man/` sits at the top level
rather than in `docs/` because `docs/` holds numbered design prose while the
man pages are shipped artifacts referenced by the Makefile.

## Mechanics

Targets: `all` (default; builds both), `koshell` (`cargo build --release`),
`daemon` (`bun install --frozen-lockfile`, then the package's own
`build:binary` script), `debug` (`cargo build`; the daemon has no debug
variant — in development it runs from source via `KOSHELL_DAEMON_CMD`),
`install`, `uninstall`, `check` (the same commands CI runs), `clean`.

Load-bearing decisions:

- **`install` does not depend on the build targets; it precondition-checks
  instead.** The natural flow is `make && sudo make install`, and a build
  dependency would re-run cargo under sudo, leaving root-owned files in
  `target/`. A missing artifact produces a clear "run 'make' first" error.
- **`bun install --frozen-lockfile`**: the build entry point is reproducible
  and fails loudly on lockfile drift; intentional dependency changes run plain
  `bun install` by hand.
- **`clean` runs `cargo clean --release`**, preserving `target/debug`
  incremental caches that dominate daily development, plus removes
  `packages/ai-daemon/dist`. `node_modules` is `bun install` state, not a build
  artifact, and is left alone.
- **Portability floor**: GNU make 3.81 (macOS's `/usr/bin/make`) and the
  BSD/GNU-common subset of `install(1)` (`-d`, `-m`; no `-D`, `-t`, `-v`).

## Ownership and drift

The man pages are maintained by hand and must be updated in the same change as
whatever they describe: a `cli.rs` flag or subcommand change updates
`koshell.1`; a `config.ts` schema change updates `koshell.toml.5`. The `.TH`
version string ("koshell 0.1.0") tracks the workspace version manually, so a
version bump touches both pages. There is deliberately no generated-from-clap
pipeline; the pages carry prose (discovery chain, trust model, degradation
behavior) that `--help` cannot.

## Open issues

- No automated drift check between `--help`/the config schema and the man
  pages. Resolvable later with a test that diffs the subcommand and key lists.
- The Bun-compiled daemon binary is large (tens of MB); expected for a
  self-contained runtime, noted in the README.

## Verification

- `mandoc -T lint man/koshell.1 man/koshell.toml.5` is clean; both pages
  render correctly with `mandoc -T ascii`.
- `make` builds both binaries; `make install DESTDIR=<stage>` produces exactly
  the four files; the staged `koshell` runs `--version` and
  `daemon start`/`status`/`stop` finds the adjacent staged daemon;
  `make uninstall DESTDIR=<stage>` leaves the stage empty; after `make clean`,
  `make install` fails with the "run 'make' first" guard; `make check` passes.

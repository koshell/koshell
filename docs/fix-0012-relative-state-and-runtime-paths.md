# Fix 0012 — koshell's own directories are never resolved relative to the cwd

Date: 2026-07-30 13:05 CST (+0800)

Status: implemented.

## Why

Found while reviewing the fix-0011 / design-0023 work: a new test ran `koshell` with a
cleared environment, and koshell created a `.local/state/koshell/koshell.log` **inside the
repository**, in whatever directory the test happened to run from.

Both of koshell's own directory resolvers fell back through `HOME` without checking that it
gave them anything, and `PathBuf::from("").join(".local")` is the relative path `.local`:

- `logging::state_dir()` → `.local/state/koshell` (terminal log, auto-spawned daemon log)
- `ipc::runtime_dir()` → `.cache/koshell` (daemon socket, per-tty liveness markers)

A relative path is uniquely bad **for this process**. koshell's working directory is not its
own: it follows the inner shell's `cd`, on purpose, so external tooling reading the pane
process sees the real directory (design 0005 working-directory mirroring, and the broader
re-publication duty in the internal `terminal-transparency` audit). So a relative koshell
path does not just land somewhere unexpected once — it means _different_ directories at
different moments, and different directories in different processes.

The consequences were graded, and the smallest one is the one that got noticed:

- **The log** is opened once at startup, so it lands in the startup directory. The
  auto-spawned **daemon log** path is resolved at first `#?` instead, by which time cwd
  mirroring has been following the user around, so it could be created in an unrelated
  project directory. Cosmetic, but it writes files into directories koshell has no business
  writing to.
- **The daemon socket** and the **tty liveness markers** are worse in kind, because they are
  the two paths that exist precisely so that _separate processes_ agree: the terminal and
  the daemon on where to connect, and koshell, `koshell preflight`, and the shell auto-wrap
  snippet on which koshell owns a tty. Two processes with different working directories
  resolving `.cache/koshell/tty/...` are not talking about the same file — and a liveness
  marker that reads as absent is read as "no live koshell owns this tty", which is exactly
  the input to the nested-launch guard fix 0011 had just made load-bearing.

`ipc::runtime_dir()` had a second, related problem: the shell snippet duplicates this XDG
precedence inline as `${XDG_RUNTIME_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}}/koshell`, and
design 0017 records that the two must be kept in sync. With an empty `HOME` they had already
drifted — shell's literal `$HOME/.cache` expands to the _absolute_ `/.cache`, while Rust
produced the _relative_ `.cache`.

Nobody hit this in practice: an interactive login shell essentially always has `HOME`. It is
a robustness and hygiene defect, reachable through `env -i`, a stripped service environment,
or a test.

## How

Two different answers, because the two directories have different obligations.

**`logging::state_dir()` now returns `Option<PathBuf>`** — `None` when neither variable
yields an _absolute_ base. A log is best-effort by contract (`init` already discarded
logging when the file could not be opened, rather than failing startup or writing into the
terminal), so "nowhere to put a log" is simply "no file logging". The one caller that needs
a path regardless, the daemon spawn redirection, falls back to `/dev/null`: a missing log
must never keep the daemon from starting. `daemon_log_path()` became `Option` with it, and
the three places that _show_ the user the log path go through a new
`daemon_log_description()`, which prints the reason instead of a bare path when there is
none — a wrong path in a "check the log" hint is worse than an explanation.

**`ipc::runtime_dir()` stays infallible and is now always absolute.** Returning `Option`
here would mean threading "there is no socket path" through every caller to express a state
that already has a designed failure: an unwritable directory makes the daemon unreachable
and the terminal degrades to a transparent shell wrapper, which is the no-daemon invariant
working as intended. So an empty `HOME` now resolves the way the snippet's `$HOME/.cache`
already did — rooted at `/`, giving `/.cache/koshell` — which removes the relative path and
closes the drift against the snippet in the same change. `/.cache` is normally unwritable,
so the outcome is the designed degrade rather than a path that silently moves.

Both resolvers grew a pure inner function (`resolve_state_dir`, `resolve_runtime_dir`) taking
the environment explicitly, following `daemon_spawn::resolve_plan`'s shape, so the absent,
blank, and relative cases are tested without mutating process-global environment — which
would race the rest of the suite.

## Tests

- `logging.rs`: `xdg_state_home_wins_and_home_is_the_fallback` (precedence, and blank
  treated as unset) and `a_relative_or_absent_base_yields_no_state_directory` (no `HOME`,
  blank `HOME`, relative `HOME`, and a relative `XDG_STATE_HOME` that an absolute `HOME`
  must not rescue — it was chosen).
- `ipc.rs`: `the_runtime_directory_follows_the_xdg_precedence` and
  `the_runtime_directory_is_always_absolute`, the latter also asserting the ambient
  environment and pinning the `/.cache/koshell` spelling the snippet agrees with.
- `tests/control_cli_pty.rs` sets an isolated `HOME`/`XDG_STATE_HOME` for its
  cleared-environment case, so a test never writes into the developer's own state directory
  either.

Verified by hand: `cd` into an empty directory and run `env -i PATH=... koshell new` — no
`.local` is created (it was, before), and `env -i PATH=... koshell daemon status` reports
`socket: /.cache/koshell/daemon.sock` rather than the relative `.cache/...`.

## Open issues

- The shell snippet still duplicates `runtime_dir`'s precedence inline (design 0017's
  recorded "keep the two in sync" note). This fix makes the two agree in one more case
  instead of removing the duplication; a snippet that asked koshell for the path would need
  a subprocess on every shell start, which the snippet deliberately avoids.
- Nothing asserts the Rust and shell spellings agree. The agreement is verified by hand at
  design time (design 0009 did this for the tty spellings) and by the `shell_init_pty`
  end-to-end tests, which exercise the real marker path only under a normal `HOME`.

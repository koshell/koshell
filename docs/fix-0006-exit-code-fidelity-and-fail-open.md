# Fix 0006 — Exit-code fidelity on signal death, and fail-open startup safety

Date: 2026-07-05 16:30 CST

## Why

Two transparency residuals left open after `fix-0005` (tracked in the internal
`architecture/terminal-transparency` audit as obligations 7 and 16):

- **Exit code on signal death (#7).** A shell killed by a signal must surface as
  `128 + signo` — the value `$?` reports for a signal-killed child, and what a parent
  shell would see for koshell had koshell been killed directly. koshell collapsed every
  signal death to `1`, so `exec koshell` handed the outer world a wrong `$?` and tmux /
  scripts reading koshell's exit status could not tell a `SIGINT` from a clean `exit 1`.
- **Fail-open on startup failure (#16).** The `exec koshell` auto-wrap replaces the login
  shell. If koshell then died during startup it did `exit(1)`, closing the terminal before
  the error was readable. On a Linux virtual console or login TTY — often the user's only
  entry point — that is a lock-out, not an annoyance.

A third item, `SHLVL` accuracy (#12), was re-examined at the same time and needed only a
documentation correction (see below); no code changed.

## Root cause

- **#7.** `portable-pty` 0.9's `ExitStatus` discards the numeric terminating signal: its
  `From<std::process::ExitStatus>` maps a signal death to `code = 1` and renders the
  signal to a localized `strsignal` string, so `exit_code()` cannot recover `128 + signo`.
  `fix-0005` concluded this was unfixable through the crate's API. That was true of the
  API but missed a bypass: koshell already holds the child's pid, and the concrete Unix
  `Child` behind portable-pty is a `std::process::Child` whose `Drop` does not reap, with
  no `SIGCHLD` handler installed in the parent — so koshell can `waitpid` the pid itself.
- **#16.** `main.rs` turned every startup `Err` into `exit(1)`, and there was no panic
  guard. Under `exec`, both close the terminal. The snippet's pre-`exec` guards
  (`command -v koshell`, TTY checks) cannot catch a binary that loads but then fails, or
  panics, during its own startup.

## The fix

- **#7 — faithful exit code (`session.rs`).** A new `reap_child` reaps the shell's
  specific pid via `libc::waitpid` and reads the wait status directly: `WIFSIGNALED` →
  `128 + WTERMSIG`, `WIFEXITED` → `WEXITSTATUS`; it retries on `EINTR` (a forwarded
  `SIGWINCH` can interrupt the wait) and falls back to portable-pty's `child.wait()` when
  the pid is unknown or `waitpid` fails. This is the only reaper — the targeted `waitpid`
  never touches the AI daemon or any other child, and there is no double-reap because the
  underlying `std::process::Child::Drop` does not reap. koshell then `exit`s with that
  code (a normal exit carrying the value), so `exec koshell` reproduces the shell's own
  `$?` exactly.
- **#16 — fail open (`main.rs`, `session.rs`, `shell_init.rs`, `cli.rs`).** Two layers:
  - _Pre-`exec` (snippet):_ a new `koshell preflight` subcommand — a fast, TTY-free probe
    that exits 0 only when koshell runs at all and a real shell is resolvable — is added to
    the snippet as `… && koshell preflight >/dev/null 2>&1; then exec koshell`. A binary
    too broken to start makes the probe itself fail, and the shell is kept.
  - _Post-`exec` (koshell):_ `main` wraps `run_interactive_shell` in `catch_unwind`; on any
    startup error or panic it calls `exec_fallback_shell`, which `exec`s the user's real
    shell with `KOSHELL_NO_AUTO=1` (so the fresh rc does not loop back into the same
    crash). Every fallible step runs before koshell takes over the terminal, and
    `RawModeGuard` restores cooked mode while unwinding, so the fallback shell lands on a
    usable terminal. Fail-open applies only to the bare `exec koshell` form; an explicit
    `koshell <command>` still exits non-zero (its parent shell is alive).

## Verification

- New real-PTY tests `crates/koshell-rs/tests/signal_exit_pty.rs`: a normal exit
  propagates verbatim (`exit 42` → 42), and signal deaths surface as `128 + signo`
  (`kill -TERM $$` → 143, `kill -KILL $$` → 137).
- New real-PTY test `crates/koshell-rs/tests/fail_open_pty.rs`: koshell driven into a
  deterministic pre-takeover failure falls open to a live shell carrying
  `KOSHELL_NO_AUTO=1`.
- New unit test in `cli.rs` for `preflight` parsing; the `shell_init.rs` guard-presence
  test now also asserts the `koshell preflight` gate.
- `cargo test` (138 unit + all PTY suites), `cargo clippy --all-targets`,
  `cargo fmt --check`: green.

## The `SHLVL` correction (#12)

`design-0003` previously listed `SHLVL` as "off by one … cosmetic; no action planned."
That was a stale misdiagnosis. Empirical PTY probing (parent `SHLVL=1`) showed a shell
launched under the wrap reports the same `SHLVL` as a bare one, and a non-shell
(`koshell vim`) inherits the value unchanged. koshell never touches `SHLVL`: the shell
increments it on startup and decrements it before any `exec`, so `exec koshell` receives
an already-decremented value and the inner shell re-increments to the correct level.
Corrected `design-0003`; the audit row moves to met. No code was needed, and none should
modify `SHLVL`.

## Residuals

- **Post-takeover catastrophic failures still cannot fail open cleanly.** A panic _after_
  `enable_raw_mode` unwinds through `RawModeGuard` (cooked mode restored) and still reaches
  the fallback, but the already-spawned inner shell is orphaned and a hard crash
  (`SIGSEGV`, `abort`) bypasses the unwind entirely. These are rare and shared with any
  binary; `preflight` covers the common "won't start" case.
- **Exit code fidelity is Unix-specific** by construction (`waitpid`, `WIFSIGNALED`). This
  matches the rest of koshell's PTY layer.
- **Current-command reflection (#14) remains open** — declined as process-identity
  spoofing; see the internal transparency note and `design-0003`.

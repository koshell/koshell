# Fix 0005 — PTY-wrapper transparency: cwd mirroring, pixel size, signal forwarding

Date: 2026-07-05 12:53 CST

## Why

Reported from tmux use: with `Ctrl+b %` configured to reuse the current pane's working
directory, a split reused koshell's _startup_ directory instead of the inner shell's
current one. After `cd`-ing inside the wrapped shell and splitting, the new pane opened
in the pre-`cd` path.

An audit of the wrapper's transparency obligations (owned by the internal
`architecture/terminal-transparency` note) surfaced two more process-visible gaps in the
same class: the inner PTY advertised no pixel geometry, and termination signals sent to
koshell were never forwarded to the shell.

## Root cause

koshell is the foreground process on the terminal's TTY; the real shell runs one level
down on an inner PTY. Anything that introspects the outer TTY reads koshell, not the
shell. Three consequences:

1. **Working directory.** tmux `pane_current_path` reads the pane process's cwd (macOS
   `proc_pidinfo`, Linux `/proc/<pid>/cwd`) — koshell's. A child's `chdir(2)` never
   propagates to a parent, so the inner shell's `cd` was invisible; koshell's cwd stayed
   frozen at startup.
2. **Pixel size.** `openpty`/resize passed `pixel_width: 0, pixel_height: 0`, so
   `ws_xpixel`/`ws_ypixel` were unknown inside the wrap and pixel-addressed image
   protocols (sixel, kitty graphics) could not size accurately.
3. **Signals.** Only `SIGWINCH` was handled. A `SIGTERM`/`SIGHUP`/`SIGINT` delivered to
   koshell terminated it by default disposition; the shell only ever saw a hang-up from
   the master closing, running `HUP` traps rather than the original signal's traps.

## The fix

All three keep the shell's process-visible state mirrored outward from the wrapper.

- **cwd mirroring.** The shell integration now emits an OSC 777 `cwd` marker from
  `precmd` (zsh) / `PROMPT_COMMAND` (bash) on every prompt, carrying `$PWD`. koshell
  intercepts the marker in the session loop and calls `std::env::set_current_dir` on
  itself, so `pane_current_path` (and OSC 7 consumers) read the real directory. Moving
  koshell's own process cwd is side-effect free: every path koshell uses is absolute (XDG
  runtime/cache sockets and logs, temp rc dirs). A stale directory just fails the `chdir`
  and is ignored. The marker is a new `MarkerKind::Cwd` and is stripped from the stream
  by the same `MarkerScanner` path as the command-boundary markers, so it never reaches
  the terminal or the mirror.
- **Pixel size.** `terminal_pixel_size()` reads `ws_xpixel`/`ws_ypixel` from stdout via
  `TIOCGWINSZ` and feeds them to the inner PTY at open and on every `SIGWINCH` resize.
  A terminal that reports no pixels yields `(0, 0)` — the same "unknown" a bare shell
  sees, so nothing regresses.
- **Signal forwarding.** koshell now catches `SIGHUP`/`SIGTERM`/`SIGINT` and forwards the
  original signal to the inner shell (its session-leader pid) instead of dying by its own
  default disposition. The shell runs its real traps, as if it owned the TTY; its exit
  then closes the PTY, the reader hits EOF, and `child.wait()` reaps it. A shell that
  traps and survives keeps koshell alive with it — the transparent outcome. `SIGWINCH`
  keeps its resize path.

## Verification

- New unit test `round_trips_cwd_marker` (marker parse/format).
- New real-PTY tests `zsh_cwd_mirrors_the_inner_shell_working_directory` and
  `bash_cwd_mirrors_the_inner_shell_working_directory`: drive an interactive shell to
  `cd` into a fresh directory and assert koshell's own process cwd follows (read via
  `/proc/<pid>/cwd` on Linux, `lsof` on macOS; the test skips where neither is
  available).
- Existing trigger/presentation/shell-integration/PTY suites unchanged and green:
  `cargo test`, `cargo clippy --all-targets`, `cargo fmt --check`.

## Residuals

- **Exit-code fidelity on signal death is unchanged.** A faithful `128 + signo` mapping
  is not achievable through `portable-pty` 0.9: its `ExitStatus` discards the numeric
  terminating signal and keeps only a localized `strsignal` name, so `exit_code()`
  reports `1` for a signal-killed shell. Reverse-mapping the localized name to a number
  is too fragile to be worth it; left as-is.
- **Mid-command `cd` is not reflected until the command returns.** cwd is reported at
  command boundaries (`precmd`), so `cd foo && long-running-command` leaves koshell's cwd
  at the pre-`cd` path until the command finishes and the next prompt renders. The common
  case (`cd` at the prompt, then split) is covered. Matching a native shell's live cwd
  mid-command would require a different reporting channel.
- **cwd mirroring needs shell integration.** Non-integrated inner programs (REPLs, remote
  shells, `bash --norc`) emit no marker, so koshell's cwd stays at the last integrated
  value. This matches the marker layer's existing scope.
- **Current-command reflection (`pane_current_command`) and `exec` fail-open on Linux
  TTYs remain open**, tracked as design questions in the internal transparency note; they
  were out of scope for this fix.

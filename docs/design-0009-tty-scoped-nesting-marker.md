# Design 0009 — tty-scoped nesting marker (koshell works inside tmux)

Date: 2026-07-08 10:47:12 CST

## Why

koshell was unusable inside tmux. Two independently-correct suppressions collided:

- **The outer koshell is disarmed by the alternate screen.** `#?` arms only where input
  is echoed **and** the alternate screen is not active (`src/context.rs`, `src/trigger.rs`).
  tmux holds the alternate screen for its whole run, so the koshell wrapping the tmux
  client never fires `#?`. This is correct and stays: that koshell only sees tmux's
  composited alt-screen mux stream and must not try to parse it.
- **Every tmux pane shell was suppressed by the flat `KOSHELL=1` marker.** The tmux server
  captures its environment once at start and hands it to every pane. A server started
  inside koshell froze `KOSHELL=1` into that environment, so each pane shell ran the
  auto-wrap snippet, saw `KOSHELL` set, and skipped `exec koshell`. No koshell owned the
  pane, and `#?` never fired anywhere in tmux.

The root cause of the second half is that the recursion guard was **environment-inherited
and coarse**: `KOSHELL=1` means "some koshell is in my ancestry", which is true in a pane
even though no koshell owns the pane's own pty. The fix makes the guard **tty-scoped**.

This is the _spawn/wrap axis_ of the nested-koshell problem only. The cross-boundary
_ownership/dedup axis_ (`koshell → ssh → koshell` double-answer; a pane reading the outer
koshell's composited screen) is out of scope here — see the koshell-internal
`trigger-semantics.md` / `terminal-transparency.md` open decisions.

## The markers

Three environment markers now cooperate:

- **`KOSHELL=1`** — unchanged public "am I inside koshell" signal (user scripts, prompts),
  and the recursion **fallback**.
- **`KOSHELL_TTY=<inner pts path>`** — the **primary**, tty-scoped guard: the controlling
  tty of the shell koshell wrapped.
- **`KOSHELL_TTY_MARKER=<file>`** — the **liveness** gate: a file holding the wrapping
  koshell's PID, written on startup and removed on exit. `KOSHELL_TTY` and
  `KOSHELL_TTY_MARKER` are set together in `session.rs` once the child pts is known — one
  is never present without the other.

Nested-detection rule, applied identically in Rust and in the shell snippet:

- If `KOSHELL_TTY` is set → nested **iff it equals this process's controlling tty AND the
  koshell named by `KOSHELL_TTY_MARKER` is still alive** (`kill -0`). Ignore `KOSHELL`.
  - A pane inherits a `KOSHELL_TTY` naming a _different_ pts → not nested → wraps.
  - A shell on the same tty as a _live_ koshell (the inner shell, or a plain subshell) →
    nested → does not re-wrap.
  - A shell on a _recycled_ pts whose original koshell has died → tty matches but the PID
    is dead → not nested → wraps. (See "Why liveness" below.)
- If `KOSHELL_TTY` is unset → fall back to `KOSHELL == "1"` (the old coarse behavior).

The fallback exists because `MasterPty::tty_name()` — or the marker write — could fail;
`KOSHELL_TTY` is branded **only when the marker was written**, so a child never sees a tty
brand it cannot liveness-check (which would make even the genuine inner shell re-wrap, and
could recurse). Falling back to the flat marker keeps recursion broken. It also means the
fail-open path (`exec_fallback_shell` sets `KOSHELL_NO_AUTO=1`) and the `fail_open` test
(which sets only `KOSHELL=1`) are unchanged.

## Why liveness: the recycled-pts hole

Without the liveness gate, a stale `KOSHELL_TTY` could wrongly suppress a legitimate wrap.
tmux captures its environment once at server start and **does not refresh it on attach**,
so a pane's inherited `KOSHELL_TTY` names the koshell that _started the server_, frozen for
the session's life. That staleness is harmless for the wrap decision as long as the brand
names a _different_ tty than the pane's own — the pane still wraps. The one exception is
**pts-number recycling**: if the branding koshell dies (detach + its outer koshell exits)
and the kernel later reallocates that exact pts to a new pane, the pane's `$(tty)` equals
the stale brand and it would wrongly skip wrapping.

The liveness gate closes this: the marker for a dead koshell is either gone (clean exit) or
holds a dead PID (crash), so `kill -0` fails and the recycled pane wraps. Because koshell's
own `is_nested_koshell` applies the same gate, koshell also does not refuse-and-fail-open in
that case — both sides agree via the same inherited `KOSHELL_TTY_MARKER`.

Residual (smaller, accepted): PID reuse. If a dead koshell's PID is recycled by another live
process **and** the pts is recycled to a pane, `kill -0` succeeds and the pane wrongly skips.
This compounds two independent rare events; bounded (one pane, manually recoverable with
`exec koshell`) and could be tightened later with a boot nonce in the marker.

## Three suppression sites, one rule

All three moved to the rule above. Missing any one leaves the pane broken: the snippet
would `exec koshell`, then koshell would refuse on the inherited `KOSHELL=1` and fail open
back to a bare shell.

1. **The auto-wrap snippet** (`src/shell_init.rs`), for both bash and zsh — the "should
   wrap" condition:

   ```sh
   { { [[ -n "${KOSHELL_TTY-}" ]] &&
       { [[ "${KOSHELL_TTY}" != "$(tty 2>/dev/null)" ]] ||
         ! kill -0 "$(cat "${KOSHELL_TTY_MARKER:-/nonexistent}" 2>/dev/null)" 2>/dev/null; }; } ||
     [[ -z "${KOSHELL_TTY-}" && -z "${KOSHELL-}" ]]; }
   ```

   `$(tty)` is forked only when `KOSHELL_TTY` is set (and after the `-t 0 && -t 1` guard);
   the marker is read only when the tty also matches, so a fresh terminal pays nothing.

2. **`is_nested_koshell` / `assert_not_nested_koshell`** (`src/shell.rs`), used at the top
   of `run_interactive_shell`. `is_nested_koshell` takes the current tty
   (`shell::controlling_tty()`, a `ttyname(0)` wrapper) and a `marker_live` bool;
   `assert_not_nested_koshell` reads the inherited `KOSHELL_TTY_MARKER` via
   `shell::tty_marker_is_live` (a `kill(pid, 0)` check).

3. **`preflight`** (`src/session.rs`), the readiness probe the snippet runs before the
   `exec`, applies the same liveness-gated check.

The three tty spellings agree — verified byte-identical on macOS and Linux (2026-07-08):
`MasterPty::tty_name()` (the brand) == the shell's `$(tty)` (the snippet) == `ttyname(0)`
(koshell's own guard) — `/dev/ttys058` on macOS, `/dev/pts/1` on Linux. tmux panes were
also confirmed to inherit the server's captured `KOSHELL_TTY` while each getting its own
distinct pts.

## Getting the child pts and writing the marker

`portable-pty` 0.9's `MasterPty::tty_name() -> Option<PathBuf>` returns the slave pts
path directly, so no new dependency and no manual `ptsname` are needed. The brand is
injected into `launch.env` in `session.rs` right after `openpty` (the pts is unknown
before then — `create_pty_env` runs earlier and is left setting only `KOSHELL=1`).

The marker file lives under `<runtime_dir>/tty/<pts-with-slashes-escaped>`, where
`runtime_dir()` (factored out of `ipc.rs`) follows koshell's XDG precedence
(`$XDG_RUNTIME_DIR/koshell`, then `$XDG_CACHE_HOME/koshell`, then `~/.cache/koshell`) and
deliberately avoids a world-writable `/tmp`. `shell::register_tty_marker` writes the PID
and returns a `TtyMarker` RAII guard held for the session; its `Drop` removes the file on
normal exit and unwind. A hard crash (`SIGKILL`) leaks the file, but the PID it holds is
then dead, so the liveness check still reports not-live.

## Accepted consequences

- **One koshell per tmux pane.** Every pane now runs a full koshell (PTY mirror, timeline).
  This is the same topology that already existed when tmux is started _outside_ koshell
  (each pane auto-wraps), and its memory footprint is bounded by the tiered timeline
  retention policy (`fix-0007-timeline-memory-retention.md`).
- **Pane-internal presentation rendering** (koshell's `#?` receipt/answer lines) now draws
  inside a tmux pane's constrained viewport. Functionally correct; visual polish under a
  small viewport is a dogfooding item.
- The outer koshell stays disarmed while tmux runs. Unchanged and intended.

## Tests

- `src/shell.rs`:
  - `nested_detection` — tty + liveness cases: same-tty + live ⇒ nested; same-tty + dead ⇒
    not nested (recycled pts); different-tty (pane, even with `KOSHELL=1`) ⇒ not nested;
    `KOSHELL_TTY` unset + `KOSHELL=1` ⇒ nested (fallback); empty `KOSHELL_TTY` ⇒ absent.
  - `tty_marker_liveness` — own pid reads live; a past-`pid_max` pid, garbage, empty, and
    missing files all read not-live.
  - `assert_not_nested_uses_liveness` — same-tty brand trips only with a live marker.
- `src/shell_init.rs` `snippets_carry_the_guards_and_the_exec` — asserts `${KOSHELL_TTY-}`,
  `$(tty`, `${KOSHELL_TTY_MARKER`, `kill -0`, and the `${KOSHELL-}` fallback are present;
  the `bash -n` / `zsh -n` syntax checks validate the grouped condition.
- `tests/shell_init_pty.rs`:
  - `foreign_koshell_tty_still_wraps_like_a_tmux_pane` — a `KOSHELL_TTY` naming a foreign
    pts still wraps (`WRAP-STATE-1`).
  - `stale_marker_on_matching_tty_still_wraps` — `KOSHELL_TTY` == own tty but a dead-pid
    marker (the recycled-pts case) still wraps.
  - `live_marker_on_matching_tty_skips_wrap` — `KOSHELL_TTY` == own tty with a live-pid
    marker skips (`WRAP-STATE-none`).
  - `fail_open_pty.rs` and `escape_hatch_env_keeps_the_original_shell` are unchanged —
    `KOSHELL=1` / `KOSHELL_NO_AUTO=1` semantics are preserved.

## Open issues

- The full real-tmux end-to-end walk (open tmux inside koshell, confirm `#?` fires in each
  pane, `split-window` panes wrap independently) is a manual dogfooding step; the automated
  PTY test covers the branding/guard/preflight/exec loop but not tmux itself.
- Cross-pane / cross-screen `#?` context (a pane reading sibling panes or the outer
  composited screen) is deliberately deferred to the ownership/dedup axis.

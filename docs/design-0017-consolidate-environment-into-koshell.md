# Design 0017 — consolidate koshell's identity environment variables into one `KOSHELL`

Date: 2026-07-09 16:53:10 CST

Status: accepted, implemented.

## Why

koshell was branding each wrapped shell with four separate environment variables to
describe "the current koshell environment":

- `KOSHELL=1` — the public "am I inside koshell" signal and the coarse recursion fallback.
- `KOSHELL_TTY=<pts>` — the tty-scoped nesting guard (design 0009).
- `KOSHELL_TTY_MARKER=<file>` — the path of that tty's liveness marker (design 0009).
- `KOSHELL_SESSION_ID=koshell-<pid>` — the routing address `koshell status`/`reload` use
  (design 0015).

That is four names for one concept. tmux carries the same kind of information in a single
variable — `TMUX=<socket>,<pid>,<session>` — and derives everything it needs by splitting
it. This change adopts that shape: one `KOSHELL` variable holds the necessary fields to
identify the current environment.

The user-facing _input_ toggles (`KOSHELL_LOG`, `KOSHELL_NO_DAEMON_SPAWN`,
`KOSHELL_DAEMON_CMD`, `KOSHELL_NO_AUTO`, `KOSHELL_NO_EVENT_LOG`) are orthogonal knobs a
user sets, not identity, and are out of scope — they keep their own names.

## The format

```
KOSHELL=<session-id>[,<tty>]
```

- **Field 0, `session-id`** (`koshell-<pid>`) — always present. It is the routing address
  child `koshell status`/`reload` use, and its mere presence is the public "am I inside
  koshell" signal and the coarse recursion fallback. `create_pty_env` sets this base value
  immediately (the wrapper pid is known before the child pts is).
- **Field 1, `tty`** — the wrapped controlling terminal, added by `session.rs` only once
  the child pts is resolved _and_ its liveness marker is written. Its absence falls back to
  the coarse guard, exactly as an unset `KOSHELL_TTY` did before.

tty paths and `koshell-<pid>` never contain a comma, so a plain split recovers the fields.

## The liveness marker is now derived, not carried

design 0009 passed the marker file's path explicitly in `KOSHELL_TTY_MARKER`. That field is
gone. The marker still exists — it is what makes the liveness gate robust against pid reuse
after a clean exit (the file is removed on exit, so a recycled pid cannot masquerade as the
original koshell) — but its path is now derived by convention on both sides from the tty:

```
<runtime_dir>/tty/<tty with '/' replaced by '_'>
```

- Rust: `shell::tty_marker_path` (`ipc::runtime_dir().join("tty").join(tty.replace('/', "_"))`).
- The auto-wrap snippet reconstructs the same path inline, only on the tty-match branch:

  ```sh
  "${XDG_RUNTIME_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}}/koshell/tty/${__ks_tty//\//_}"
  ```

  The nested `${:-}` defaults mirror `runtime_dir`'s XDG precedence
  (`$XDG_RUNTIME_DIR/koshell`, then `$XDG_CACHE_HOME/koshell`, then `~/.cache/koshell`)
  exactly, and `${var//\//_}` mirrors the slash escape.

This trades one env field for a small, deliberate coupling: the snippet now duplicates
`runtime_dir`'s XDG precedence. That is the same "one rule, three sites" discipline design
0009 already established for the nesting rule — the coupling is documented here and in the
`shell_init.rs` comment so the two stay in sync. It keeps design 0009's liveness robustness
intact (no regression); only the _encoding_ of the markers changed.

## Nesting rule (unchanged in behavior)

Applied identically in `shell::is_nested_koshell`, the auto-wrap snippet, and `preflight`:

- If `KOSHELL` carries a tty field → nested **iff** it equals this process's controlling
  tty **and** the tty's liveness marker names a live pid (`kill -0`). Ignore otherwise.
- If `KOSHELL` has no tty field → nested iff `KOSHELL` is present at all (coarse fallback).

The snippet forks `$(tty)` and builds the marker path only when the tty field is present, so
a fresh terminal still pays nothing.

## What changed

- `shell.rs`: one `KOSHELL_ENV_KEY`; removed `KOSHELL_ENV_VALUE`, `KOSHELL_TTY_ENV_KEY`,
  `KOSHELL_TTY_MARKER_ENV_KEY`. Added `koshell_session_id` / `koshell_tty` field accessors,
  `koshell_env_value` builder, and `tty_marker_path` / `tty_is_live` for the conventional
  marker path. `create_pty_env` sets the base `KOSHELL=<session-id>`; `is_nested_koshell`
  and `assert_not_nested_koshell` read the fields from `KOSHELL`; `register_tty_marker`
  writes to the conventional path and no longer exposes it.
- `session.rs`: brands `KOSHELL=<session-id>,<tty>` in one insert (dropping the separate
  session-id / tty / marker inserts); `preflight` derives liveness from `KOSHELL`'s tty.
- `ipc.rs`: removed `SESSION_ID_ENV`; `current_session_id` reads field 0 of `KOSHELL`.
- `status_cli.rs`: reads the displayed tty from field 1 of `KOSHELL`.
- `shell_init.rs`: the bash and zsh snippets split `KOSHELL` and reconstruct the marker path.
- Man page, README, and `architecture.md` updated; tests updated across `shell.rs`,
  `shell_init.rs`, `tests/shell_init_pty.rs`, and `tests/fail_open_pty.rs`.

The daemon is untouched: the session id string `koshell-<pid>` still flows over the IPC
`hello` (`terminal_session_id`), computed by `ipc::session_id()`, so the daemon's session
registry keys are unchanged.

## Compatibility

None preserved, by decision — the project has a single user and no deployed installs. A
prompt or script that read the exact value `KOSHELL == "1"` breaks (the value is now
structured); a presence check (`-n "$KOSHELL"`) still works, and `koshell status` still
prints the wrapped tty. There is no migration: an old shell holding the previous four
variables is simply re-wrapped by a new koshell, which overwrites `KOSHELL` with the new
form.

## Relationship to earlier records

This supersedes the _environment-variable encoding_ described in design 0009 (nesting
markers) and design 0015 (session-id routing). The behavior those documents specify — the
tty-scoped, liveness-gated nesting rule, and per-instance status/reload routing — is
unchanged; only the variable names and layout moved into `KOSHELL`. Both documents carry a
one-line pointer to this record; their bodies are left as the dated decision records they
are.

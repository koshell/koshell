# Fix 0011 — a refused nested launch no longer fails open into a nested shell

Date: 2026-07-30 12:33:17 CST (+0800)

Status: implemented.

## Why

Running `koshell` from inside koshell printed the refusal and then nested anyway:

```
$ koshell
koshell failed: koshell is already running in this shell. Start a new regular
terminal session before launching koshell again.
$                     # ...and this is a *new* shell, one level deeper
```

The message was correct and the outcome contradicted it. The user landed in an extra shell
layer they never asked for — with none of koshell's shell integration, needing one more
`exit` to leave, and with the wrapping koshell left holding a `koshell` command span whose
`command_end` would never arrive (the command had `exec`-ed into a shell that outlives it).
Reported as "koshell still runs nested despite the message".

## Root cause

`main.rs` treated _every_ startup error the same way. The fail-open path (design 0003,
transparency audit obligation 16) exists because the auto-wrap snippet runs `exec koshell`:
if koshell dies after replacing the shell's process image, an exiting koshell closes the
terminal, and on a Linux login TTY that can be the user's only way in. So a startup failure
`exec`s the user's real shell instead of exiting.

The nested-launch refusal from `shell::assert_not_nested_koshell` arrived on that same path
as an opaque `anyhow` message, and the fail-open condition was `command.is_empty()` — "no
explicit program was named". A bare `koshell` typed at a prompt satisfies that, so the
refusal fell open and `exec`-ed a bare shell over the refusing process. The proxy was
wrong: what actually determines whether exiting is safe is not "was a program named" but
**"is there still a shell waiting for us to exit"**, and the two only coincide outside the
nested case.

## How

The refusal is now a typed `shell::NestedLaunch` error, and it carries the answer to that
real question.

The discriminator is exact rather than heuristic. The wrapping koshell spawns the inner
shell as its **direct child**, so:

- a plain `koshell` typed at that shell's prompt has the **shell** as its parent — the shell
  is still there, so refuse, exit non-zero, and start nothing;
- an `exec koshell` (the auto-wrap snippet, or a hand-typed one) replaced the shell's
  process image and therefore inherited the shell's parent — the **wrapping koshell**, whose
  pid is exactly what the tty liveness marker already holds. No shell is left, so the
  fail-open must still fire.

So `NestedLaunch::detect` compares `getppid()` against the owner pid read from the marker,
which required `tty_marker_is_live`'s liveness check to yield the pid it already read
(`tty_marker_owner_pid`) instead of collapsing it to a bool. `main.rs` downcasts the error
and only skips the fail-open for a nested launch that did _not_ replace the shell.

When there is no owner pid — the coarse-fallback brand, a `KOSHELL` with no tty field,
which means koshell could not resolve the child pts and wrote no marker — nothing is
provable about who is above us, so `detect` reports `replaced_the_shell: true` and the
terminal-preserving fail-open stays. That keeps the rule honest in one sentence: **fail open
unless a live shell is provably waiting for us.**

The refusal message also stopped being a dead end. It now names the koshell that owns the
terminal and says what the user can actually do here (`#?` already works; `koshell status`
inspects the session; a separate koshell needs a new terminal) instead of only what they
cannot.

## Not a regression risk for the auto-wrap

The snippet and `koshell preflight` apply the same tty-scoped, liveness-gated nesting rule
as koshell itself (design 0009), so the snippet does not `exec` into a koshell that would
refuse. If the guards ever did misfire, that misfire is precisely the `exec` shape, so the
fail-open still covers it.

## Tests

- `shell.rs`:
  - `nested_refusal_is_typed_and_carries_the_fail_open_decision` — the error downcasts to
    `NestedLaunch` rather than being an opaque message.
  - `detect_reads_exec_from_the_parent_being_the_owning_koshell` — all three cases
    (prompt-typed, `exec`-ed, no owner pid) and that the message names the owner.
  - `tty_marker_owner_pid_reads_the_pid_not_just_liveness`.
- `tests/nested_launch_pty.rs` — both sides against the real binary on a real PTY branded as
  already wrapped, choosing the marker pid to select the case: a live _third_ process's pid
  means refuse-and-exit with no shell on the terminal and exit code 1; koshell's own
  parent's pid means the fail-open still puts a live shell there.
- `tests/fail_open_pty.rs` is unchanged and now covers the third case on purpose: it trips
  the guard with a tty-field-less `KOSHELL`, so it asserts the unprovable path still falls
  open.

## Open issues

None. The residual `NestedLaunch` inherits from design 0009 is unchanged: pid reuse plus pts
recycling could still make a dead koshell's marker read as live, which this fix neither
worsens nor addresses.

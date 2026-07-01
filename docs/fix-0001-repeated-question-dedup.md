# Fix 0001 — repeated `#?` question swallowed by history dedup

Date: 2026-07-01 15:28:07 CST

## Why

Asking the same `#?` question twice in a row only triggered the AI daemon on the
first attempt. The first `#? a` printed `[koshell] #? ... a`; an immediately
repeated `#? a` produced no feedback at all, as if `#?` were disabled.

## Root cause

The trigger lives in the generated shell-integration rc files
(`crates/koshell-rs/src/shell_integration.rs`), not in the Rust dispatch path.
The bash `PROMPT_COMMAND` hook and the zsh `precmd` hook detected `#?` from shell
**history** and deduplicated by the **text** of the last history entry:

```sh
history_command=$(... history 1 ...)
if [ "$history_command" != "$__koshell_last_history_command" ]; then ...
```

That guard exists for a real reason: on an empty Enter or a prompt redraw,
`history 1` still returns the _previous_ command line, and without the guard the
last `#?` would re-fire on every subsequent prompt. But keying on the history
entry is fragile:

- Text dedup suppressed a legitimately re-run identical command — including asking
  the same `#?` question twice.
- Worse, on zsh with `setopt hist_ignore_dups` (and friends `hist_save_no_dups`,
  `hist_ignore_space`), the shell collapses consecutive duplicate history entries
  to one, so the repeat is _invisible_ to any history-based check regardless of
  whether it keys on text or number. A bare `#? a` is a comment (with
  `interactive_comments`), so it runs no command and can only be detected via the
  history fallback — exactly the path defeated by `hist_ignore_dups`.

## The fix

### bash — dedup by history _number_, not text

`PROMPT_COMMAND` still reads history, but deduplicates by the history entry
**number**. A re-run advances the number, so each submission is detected; an empty
Enter or redraw adds no history entry, so the number is unchanged and it stays
suppressed. (bash has no reliable "line accepted" hook for the comment case, so it
keeps the history fallback; see Open issues for the `HISTCONTROL=ignoredups`
limitation.)

### zsh — stop using history entirely

zsh detection no longer consults history at all:

- An `accept-line` widget wrapper captures the submitted `$BUFFER` at the moment
  Enter is pressed. This is the only signal that fires for a comment-only `#?`
  line, and it never touches history, so it is immune to `hist_ignore_dups`,
  `hist_ignore_space`, and `hist_save_no_dups`. The wrapper delegates to any
  pre-existing `accept-line` widget so plugin wrappers (fzf, syntax-highlighting,
  autosuggestions, …) still run.
- `preexec` captures the command line for the real-command path; `precmd` emits the
  command-end marker from that captured line plus a per-line `command_active` flag.
- `precmd` clears all per-line state at the end, so a prompt redraw (e.g. resize)
  without a new Enter never re-fires a stale `#?`.

## Regression test

`crates/koshell-rs/tests/shell_integration_pty.rs` spawns the real `koshell`
binary inside a PTY and drives an interactive shell, asking the identical `#?`
question twice and asserting two `[koshell] #?` feedback lines. This class of bug
lives in the interactive shell hooks (line editor / `PROMPT_COMMAND` / `precmd`),
which only run in a real interactive shell, so plain unit tests cannot reach it —
the PTY test is the automated guard that replaces manual re-testing. Both tests
skip cleanly when the shell is not installed.

- `bash_same_question_asked_twice_is_detected_both_times` — bash comment path.
- `zsh_repeated_question_survives_hist_ignore_dups` — reproduces the reported
  failing configuration (`interactive_comments` + `hist_ignore_dups` +
  `hist_save_no_dups` + `hist_ignore_space`) via an isolated `HOME`/`.zshrc`.

## Open issues

- **bash `HISTCONTROL=ignoredups`/`ignoreboth`**: bash still detects the
  comment-only `#?` path via history, so a bash user who drops consecutive
  duplicates would still lose the repeat. bash lacks a clean, non-fragile
  "line accepted" hook (rebinding Return via `bind -x` breaks multiline input and
  vi-mode), so this is left as a known limitation. Not hit by the default bash
  configuration. If it matters, options are a `bind -x` Return capture or
  neutralizing dedup inside koshell's generated bash rc (which would mutate the
  user's history file).
- **zsh inline vs. comment**: the widget path handles both bare `#? q` and inline
  `ls #? q` regardless of `interactive_comments`; the inline form under default
  zsh (comments off) goes through the real-command path instead. No open action.

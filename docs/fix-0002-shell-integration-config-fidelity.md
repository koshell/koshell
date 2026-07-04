# Fix 0002 — shell integration dropped user config (custom ZDOTDIR, DEBUG traps)

Date: 2026-07-04 17:25:25 CST

## Why

An audit of the shell integration's impact on user shell setups (requested before
dogfooding) found two verified high-severity failures:

- **zsh users with a custom `ZDOTDIR` lost their entire configuration.** Both common
  layouts failed: `ZDOTDIR` exported in `~/.zshenv` (the file was sourced, but the
  integration then still looked for `$HOME/.zshrc`, which does not exist in that
  layout), and `ZDOTDIR` set in the environment (silently overwritten by the
  integration's temp-dir value). Reproduced in a sandbox: `user_zshrc_sourced=no` in
  both cases.
- **bash users lost any `DEBUG` trap their rc installed.** bash keeps a single DEBUG
  trap, and the integration installed its own after sourcing the user rc, clobbering
  whatever was there. This silently breaks every bash-preexec consumer (iTerm2 shell
  integration, atuin, ble.sh). Reproduced: `trap -p DEBUG` showed only koshell's trap.

Secondary damage from the same root: the integration's temp `ZDOTDIR` leaked into the
session environment, so anything derived from `${ZDOTDIR:-$HOME}` — oh-my-zsh's
`ZSH_COMPDUMP`, a `HISTFILE` under `ZDOTDIR`, nested zsh startup — landed in the temp
dir and was deleted when the session ended (compdump rebuilt every start; potential
history loss).

## Root cause

The generated zsh rc hard-coded `$HOME` as the location of the user's rc files at
generation time and never restored `ZDOTDIR` after pointing it at the temp injection
dir. The bash hooks installed `trap '__koshell_debug_trap' DEBUG` unconditionally.

## The fix

### zsh — stage-file delegation (VS Code's injection scheme)

`create_zsh_launch_config` now writes three stage files into the temp `ZDOTDIR` and
passes the user's real rc directory in `KOSHELL_USER_ZDOTDIR` (a pre-existing custom
`ZDOTDIR` wins over `HOME`):

- `.zshenv` sources the user's `.zshenv` with `ZDOTDIR` temporarily set to the user
  value, then re-captures `KOSHELL_USER_ZDOTDIR` from `ZDOTDIR` — so a `.zshenv` that
  relocates `ZDOTDIR` is honored by the later stages.
- `.zprofile` delegates to the user's `.zprofile` for login shells only (`[[ -o
login ]]`), matching zsh's own sourcing rule instead of forcing login config on
  every session as before.
- `.zshrc` restores `ZDOTDIR` to the user value _before_ sourcing the user's
  `.zshrc`, repoints a `HISTFILE` that macOS `/etc/zshrc` derived from the temp dir,
  sources the user rc, unsets `KOSHELL_USER_ZDOTDIR`, and then installs the koshell
  hooks. Because `ZDOTDIR` is already restored, a login shell reads the user's own
  `.zlogin` natively and no injection `.zlogin` is needed; compdump, plugin caches,
  and nested zsh all land in the real location.

### bash — cooperative hook installation

The hook installer now branches:

- **bash-preexec imported by the user rc** (`bash_preexec_imported` / legacy
  `__bp_imported` set): register `__koshell_bp_preexec` in `preexec_functions` (it
  receives the history line with a trailing `#?` comment preserved) and
  `__koshell_prompt_command` in `precmd_functions` (bash-preexec restores `$?` for
  precmd functions). `PROMPT_COMMAND` and the DEBUG trap are left to bash-preexec.
- **Otherwise**: prepend to `PROMPT_COMMAND` as before, but read any existing DEBUG
  trap first (via the eval-into-array parse of `trap -p DEBUG`, which survives
  newlines in the trap body) and install a chained trap that runs koshell's hook and
  then `eval`s the user's original body.

## Verification

- Sandbox prototypes of the stage files pass four scenarios: `ZDOTDIR` set in
  `.zshenv`, `ZDOTDIR` in the environment only, plain `HOME` layout, and a login
  shell sourcing the user `.zprofile` plus native `.zlogin`.
- New real-PTY e2e tests in `crates/koshell-rs/tests/shell_integration_pty.rs`:
  custom-`ZDOTDIR` config is loaded, session `ZDOTDIR` is restored and never leaks
  the temp dir, `KOSHELL_USER_ZDOTDIR` is cleaned up, a user DEBUG trap keeps firing
  alongside `#?` detection.
- New unit tests assert the stage files are written, `KOSHELL_USER_ZDOTDIR`
  preserves a custom `ZDOTDIR`, and the generated files parse under `zsh -n` /
  `bash -n`.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`,
  `pnpm check` all pass.

## Residuals

- The child shell is never a login shell. Its environment is inherited from the
  (typically login) parent shell, so PATH and friends are normally correct, but
  macOS bash users whose config lives only in `.bash_profile` still do not get it
  sourced, and `/etc/zprofile` (`path_helper`) does not run for the child. Resolve
  if dogfooding or user reports surface a real case; the fix would be an explicit
  login-mode decision, not a patch to the rc generation.
- Plugins that re-bind the `accept-line` widget without delegation (zsh-vi-mode
  style) can still displace the comment-line `#?` capture; the stabilization path
  remains the fallback.
- The chained DEBUG trap body runs inside a koshell function, so `$_` is not
  faithful for user traps that depend on it (same limitation as VS Code's chain).
- For users who never set `ZDOTDIR`, the session now carries `ZDOTDIR=$HOME`
  (exported by the launch config, restored by the stage rc) instead of the variable
  being unset — semantically identical for zsh.
- The bash-preexec cooperative path has no vendored-bash-preexec e2e test; it is
  covered by the chained-trap e2e (same detection surface) and syntax checks.

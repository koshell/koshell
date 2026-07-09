# Design 0016 — rename the config file to `koshell.toml`

Date: 2026-07-09 16:00:18 CST

Status: accepted, implemented.

## Why

The config file lived at `$XDG_CONFIG_HOME/koshell/config.toml` (default
`~/.config/koshell/config.toml`), but its manual page is section 5
`koshell.toml(5)`. The page name and the on-disk name disagreed, so
`koshell.toml.5` had to carry an explicit apology ("this page is named after the
format; the file on disk is called `config.toml`"), and the first-`#?` guidance
(design 0011, and the follow-up that points users at `man koshell.toml`) told
users to read a page named differently from the file they were told to create.

Renaming the file to `koshell.toml` removes that mismatch: the file, the man page,
and the guidance all use one name. The path stays inside the existing
`koshell/` XDG namespace, so nothing else about the layout changes.

## What changed

- `resolveConfigPath()` in `packages/ai-daemon/src/config.ts` now resolves
  `$XDG_CONFIG_HOME/koshell/koshell.toml` (falling back to
  `~/.config/koshell/koshell.toml`). This is the only functional change; the
  Rust terminal never builds the config path itself.
- All prose and user-facing references to `config.toml` were updated to
  `koshell.toml`: daemon source comments, `koshell reload` / `koshell auth
status` output strings (`reload_cli.rs`, `auth_cli.rs`), CLI/proto doc
  comments, the two man pages (`man/koshell.1`, `man/koshell.toml.5`), the
  README, and the daemon tests. `koshell.toml.5` also dropped its
  name-vs-format disclaimer, since the file now matches the page name.

## Compatibility

None preserved, by decision — the project has a single user and no deployed
installs. There is no migration or fallback: an existing
`~/.config/koshell/config.toml` is simply no longer read; the file must be
renamed to `koshell.toml`. The strict-schema, never-break-the-terminal contract
still holds — a missing `koshell.toml` surfaces the same inline setup guidance on
the next `#?`.

## Historical note

Design records 0011–0015 still say `config.toml`; they are dated decision records
and are intentionally left as written. This document is the point where the name
changed.

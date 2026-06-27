# Terminal Context Layer

## Requirement

Create a small terminal context layer that turns timeline events into context that future AI assistance can consume without treating raw PTY output as the only source of truth.

## Timestamp

Performed at: 2026-06-27 20:55:50 CST +0800.

## Implementation

- Added `TerminalSnapshot.altScreen` using xterm's active buffer type.
- Added automatic `screen_snapshot` recording after ordered mirror writes when a `TerminalSession` has a timeline.
- Added `screen_snapshot` recording after terminal resize when a `TerminalSession` has a timeline.
- Added `buildTerminalContext()` to collect recent human input, recent raw PTY output, recent visible output, the latest screen snapshot, and a selected primary text field.
- The context builder prefers the current screen snapshot while the terminal is in alternate screen mode, so TUI output is not represented primarily by raw escape-heavy PTY text.
- The context builder prefers `visible_output` over raw PTY output when visible output exists.

## Context Sources

- Raw PTY output is the fact stream for replay and low-level debugging.
- Visible output is the cleaned text stream for ordinary command diagnosis when it is available.
- Screen snapshots represent the current visible terminal state, including alternate-screen programs such as editors, pagers, and fuzzy finders.
- Recent human input records what the user sent to the shell, excluding input explicitly marked as hidden.

## Validation

Public unit tests cover normal-screen context selection, visible-output preference, alternate-screen snapshot preference, hidden input filtering, character limits, xterm alternate-buffer detection, and session snapshot recording.

## Open Issues

- Runtime code does not produce `visible_output` yet.
- Screen snapshots are currently recorded after every mirrored output chunk and resize when a timeline is attached; no debounce or ring policy exists yet.
- Alternate-screen support is screen-level, not semantic TUI understanding.
- No redaction policy is applied to screen snapshots yet.

## Resolution Conditions

- Add visible-output extraction when terminal output parsing is introduced.
- Add snapshot cadence, ring retention, and redaction before using snapshots for persistent storage or long-lived AI context.
- Add TUI-specific interpretation only after screen-level behavior has been dogfooded.

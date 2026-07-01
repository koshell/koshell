# Terminal Timeline Foundation

## Requirement

Introduce the first terminal timeline foundation so Koshell can evolve from a PTY shell wrapper into a shared-terminal runtime with context that future AI assistance can consume.

## Timestamp

Performed at: 2026-06-27 18:29:44 CST +0800.

## Implementation

- Added `TerminalEvent` as the initial append-only terminal fact model.
- Added `InMemoryTimelineStore` for deterministic in-process event recording and querying.
- Added timeline queries for all entries, all events, recent terminal text, recent PTY output, and the latest screen snapshot event.
- Added injectable clock and ID generation so tests do not depend on wall-clock timing.
- Connected `TerminalSession` to an optional `TimelineRecorder` interface so terminal orchestration can record `human_input` and `pty_output` without depending on a concrete store implementation.
- Serialized asynchronous mirror writes through an internal Promise queue so PTY output reaches the headless mirror in order.
- Added mirror write error reporting so asynchronous mirror failures are not silently ignored.
- Added automatic `screen_snapshot` events after ordered mirror writes and resize operations when a timeline is attached.
- Added alternate-screen awareness to terminal snapshots through xterm's active buffer type.
- Added line-level screen diff summaries for snapshots after the first comparable snapshot.
- Added snapshot lookup, screen-snapshot listing, latest alternate-screen snapshot lookup, and on-demand detailed snapshot diff APIs.

## Screen Diff Update

Updated at: 2026-06-27 21:22:02 CST +0800.

Screen snapshots now carry lightweight diff metadata when a previous snapshot exists. The summary records added, removed, and total changed line counts. Detailed line-level hunks are available through `diffScreenSnapshots()` when callers need to compare two specific snapshots.

## Validation

Public unit tests cover timeline recording, recent text queries, screen snapshot lookup, latest alternate-screen lookup, on-demand snapshot diffing, reset behavior, terminal-session timeline recording, mirror write ordering, mirror write failure reporting, snapshot recording, diff summary recording, and alternate-screen detection.

## Open Issues

- Timeline storage is memory-only.
- No persistence format is defined.
- No retention policy is defined.
- No redaction policy is defined.
- Shell command span detection is not implemented.
- Snapshot cadence, retention, and redaction policies are not implemented.
- Screen diffs are line-level only; character-level and semantic TUI diffs are not implemented.
- AI request and response events are defined but not produced by runtime code yet.

## Resolution Conditions

- Define persistence after in-memory timeline behavior stabilizes.
- Define retention and redaction before any persistent storage ships.
- Add command span detection when shell integration work begins.
- Add snapshot cadence, retention, and redaction after PTY and TUI behavior has been dogfooded.
- Consider character-level or semantic diffs only after line-level diff summaries are dogfooded.
- Produce AI request and response events when the `#?` trigger and agent runtime adapter are implemented.

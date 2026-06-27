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

## Validation

Public unit tests cover timeline recording, recent text queries, screen snapshot lookup, reset behavior, terminal-session timeline recording, mirror write ordering, and mirror write failure reporting.

## Open Issues

- Timeline storage is memory-only.
- No persistence format is defined.
- No retention policy is defined.
- No redaction policy is defined.
- Shell command span detection is not implemented.
- Automatic screen snapshot cadence is not implemented.
- AI request and response events are defined but not produced by runtime code yet.

## Resolution Conditions

- Define persistence after in-memory timeline behavior stabilizes.
- Define retention and redaction before any persistent storage ships.
- Add command span detection when shell integration work begins.
- Add automatic screen snapshot production after PTY and TUI behavior has been dogfooded.
- Produce AI request and response events when the `#?` trigger and agent runtime adapter are implemented.

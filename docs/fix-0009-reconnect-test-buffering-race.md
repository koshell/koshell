# Fix 0009 — reconnect test buffering race

Date: 2026-07-16 10:00:07 CST (+0800)

Status: implemented.

## Why

The `session::tests::send_request_reconnects_when_the_held_connection_is_dead`
test could run indefinitely under load. The test runner then reported that the
test had been running for more than 60 seconds, even though the reconnect and
request send had succeeded.

## Root cause

The second fake daemon read the `hello` and `ai_request` lines through two
separately constructed `BufReader` instances over clones of the same Unix
stream. When both lines were already available, the first reader could prefetch
them together, return only the `hello`, and discard the buffered `ai_request`
when it was dropped. The second reader then waited forever for bytes that had
already been removed from the socket receive queue.

The test thread simultaneously waited without a timeout for the fake daemon to
forward both lines, which turned the lost buffered data into an indefinite test
stall. This was a test-harness defect; the production `IpcReader` already keeps
one persistent `BufReader` per connection.

## How

The test line helper now reads from a caller-owned `BufRead`. Each fake daemon
constructs one `BufReader` for its accepted connection, and the second daemon
uses that same reader for both protocol lines so prefetched bytes remain
available. The two synchronization receives now have two-second timeouts so a
future coordination defect fails with a useful assertion instead of hanging the
suite.

No production behavior or protocol semantics changed.

## Verification

- The targeted reconnect unit test passes.
- The load-sensitive reproducer passes 200 of 200 runs at 16-way concurrency;
  before the fix, 14 of 200 runs exceeded the two-second external timeout.
- `cargo test` passes.
- `cargo clippy --all-targets -- -D warnings` passes.
- `cargo fmt --check` passes.

## Open issues

No test-specific issue remains. The separate mid-stream daemon-death limitation
recorded in `fix-0008-stale-daemon-connection-after-restart.md` is unchanged and
is not required to resolve this test-harness race.

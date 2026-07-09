# Fix 0008 — stale daemon connection after `daemon restart`

Date: 2026-07-09 11:12:01 CST

Status: implemented.

## Why

After `koshell daemon restart`, the first `#?` in an already-running terminal
**always** failed with `[koshell] #? received (AI daemon unavailable)`, no matter
how long the user waited. The second `#?` then worked. Waiting did not help
because this was not "the daemon has not come up yet" — the new daemon was
already reachable — but a stale connection held by the wrapper.

### Root cause

The wrapper connects to the daemon lazily on the first `#?` and keeps the
`IpcClient` for the life of the session (`session.rs`, the processor thread's
`ipc_client: Option<IpcClient>`). `attach_daemon` spawns a reader thread that
simply exits on EOF; it does **not** reset `ipc_client` when the connection
dies. So after a restart:

1. The old daemon exits; the wrapper's reader thread sees EOF and stops, but
   `ipc_client` still holds `Some(dead_client)`.
2. The next `#?` runs the dispatch path, which only reconnects when
   `ipc_client.is_none()` — false here — so it skips reconnect and sends on the
   dead socket. AF_UNIX returns `EPIPE`, so `send` fails; the old code set
   `ipc_client = None` and degraded to "AI daemon unavailable".
3. Only the _following_ `#?` sees `ipc_client.is_none()` and reconnects.

The held connection was never revalidated, and a send failure was terminal
rather than a trigger to reconnect — so exactly one `#?` was always lost after a
restart.

## How

`dispatch_trigger` now sends through a small `send_request` helper that retries
once on failure against a fresh connection:

```
for _ in 0..2 {
    connect_daemon(...);              // no-op if a connection is already held
    let Some(client) = ipc_client.as_mut() else { break };
    match client.send(request) {
        Ok(()) => return true,
        Err(_) => *ipc_client = None, // stale; the next iteration reconnects
    }
}
```

On the first iteration a held-but-dead connection fails the send and is dropped;
the second iteration finds `ipc_client.is_none()`, reconnects (the restarted
daemon is already listening at the same socket path), replays the `hello`
handshake, and delivers the request. The connect-or-spawn dance was extracted
unchanged into `connect_daemon` so both the initial connect and the reconnect
share it, including the auto-spawn + bounded connect-retry from design 0008.

The cold-start and truly-down behaviours are unchanged: a first `#?` with no
daemon still auto-spawns and connects within the retry budget on iteration one;
if no daemon can be reached at all, `ipc_client` stays `None`, the loop breaks,
and the terminal degrades exactly as before.

This is a client-side send-path retry, not a proactive teardown. Having the
reader thread signal disconnection was considered but is not sufficient on its
own: a `#?` can fire before that signal is processed, so the send-failure retry
is the reliable mechanism and makes a proactive reset redundant.

## Verification

- `session.rs` unit tests (the file's first test module):
  - `send_request_reconnects_when_the_held_connection_is_dead` — connects to a
    first listener that reads the `hello` then closes and removes its socket
    (leaving the client stale), binds a second listener at the same path (the
    restart), and asserts `send_request` delivers the `ai_request` on that first
    call. The teardown is ordered through a channel, so the stale send hits a
    definitively-closed peer; AF_UNIX makes `EPIPE` synchronous, so the test is
    deterministic (stress-run 40× with no flake).
  - `send_request_reports_failure_when_no_daemon_is_reachable` — with auto-spawn
    disabled and no listener, `send_request` returns `false` and holds no
    connection.
- `cargo test -p koshell-rs`, `cargo fmt --check -p koshell-rs`,
  `cargo clippy -p koshell-rs --all-targets -- -D warnings`.

## Open issues

- **Mid-stream daemon death.** If the daemon dies _while_ a response is
  streaming, the reader thread still exits silently and the in-flight response is
  left hanging until its presentation deadline; the connection is only re-made on
  the next `#?`. This fix covers the between-requests case (the reported bug), not
  a daemon that dies mid-answer. A reader-thread disconnect signal that cancels
  the active response would close that gap if it proves to matter in practice.

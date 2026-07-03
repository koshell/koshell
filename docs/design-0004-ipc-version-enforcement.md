# Design 0004 — IPC protocol version enforcement and evolution rules

Date: 2026-07-03 10:58:25 CST

## Why

Production use makes mixed protocol versions inevitable: terminal processes live for
days while the daemon restarts (and upgrades) independently. The protocol docs said
the `hello` handshake "negotiates" [`PROTOCOL_VERSION`], but nothing enforced it — the
terminal sent the version and the daemon only logged it. A future breaking protocol
change would have surfaced as a message-shape parse failure at some arbitrary later
point instead of a readable error. This work closes that gap before the first
production adoption, so later upgrades have a defined failure mode.

## What changed

### Daemon enforces the handshake (`packages/ai-daemon/src/server.ts`)

A connection starts in a rejected state ("no hello handshake yet"). A `hello` with a
matching `protocol_version` clears it; a mismatched `hello` re-arms it with a message
naming both versions and the remedy (upgrade the older side, restart the daemon,
reopen the terminal window). While rejected, every `ai_request` is answered with the
normal per-request contract — `ack`, then one `ai_error` carrying the reason — so the
terminal presents the failure inline through its existing `ai_error` path. No new
message types were needed. A later matching `hello` on the same connection recovers
it.

Behavior change: previously an `ai_request` without any `hello` was served (the
daemon fell back to `process.cwd()`); it is now refused. The real terminal always
sends `hello` immediately on connect (`crates/koshell-rs/src/session.rs`), so this
only affects hand-rolled clients.

### Terminal ignores unknown daemon message types (`crates/koshell-rs/src/ipc.rs`)

`IpcReader::recv` previously returned a hard error on any line that did not decode as
a known `ServerMessage`, which silently terminated the reader thread. It now skips
lines that are valid JSON but unknown (logged at debug) and keeps reading; non-JSON
lines remain hard errors (framing bug, not evolution). This is the terminal-side half
of the additive-evolution rule — a newer daemon may send message types an older
terminal does not know.

### Evolution rules codified (`crates/koshell-proto/src/lib.rs`)

The proto crate docs now state the discipline:

- The `hello` shape is frozen; new fields must be optional.
- Additive changes (new optional fields, new message types) do not bump the version;
  receivers ignore unknown message types.
- `PROTOCOL_VERSION` is bumped only for breaking changes (field removal/retyping,
  semantic changes), keeping `packages/ai-daemon/src/protocol.ts` in lockstep.

`docs/architecture.md` (IPC section) summarizes the same.

## Tests

- `packages/ai-daemon/test/server.test.ts` — refusal before `hello`; refusal after a
  mismatched `hello` (error names both versions) and recovery via a later matching
  `hello`; existing streaming/FIFO/dispose tests updated to perform the handshake.
- `crates/koshell-rs/src/ipc.rs` — `recv` skips an unknown message type and still
  delivers the following `ack` (over a real `UnixStream` pair); non-JSON input still
  errors.

## Open issues

- The rejection is only discovered when the user asks a `#?` question; there is no
  proactive daemon→terminal notice at connect time. Acceptable (the terminal is fully
  usable without the daemon), and a proactive notice would need a new server message,
  which the ignore-unknown rule now makes safe to add later if wanted.
- The daemon does not close the socket on mismatch; a rejected connection idles until
  the terminal exits. Harmless at one-per-terminal scale.
- `koshell-proto` and `protocol.ts` are maintained by hand in lockstep; nothing
  machine-checks that the two definitions agree beyond the version constant.

# Design 0015 — instance status and config reload (`koshell status`, `koshell reload`)

Date: 2026-07-09 10:36:22 CST

Status: accepted, implemented.

> Superseded in part by design-0017: the `KOSHELL_SESSION_ID` variable this record
> describes is now field 0 of the consolidated `KOSHELL=<session-id>,<tty>` variable. The
> routing behavior is unchanged.

## Why

The daemon reads `config.toml` only when a conversation is created
(`agent-runtime.ts`, `resolveProvider(loadConfig())`), then bakes the resolved
provider/model/auth into a pi `AgentSession` memoized on the connection. So a
config edit is invisible to a terminal that already asked a `#?` — it only takes
effect on a new conversation (reconnect). And there was no way to ask "what model
is _this_ terminal talking to right now?".

`koshell reload` re-reads the config and applies it to live sessions;
`koshell status` reports the current instance's live state. Both operate on the
**current** koshell instance — one terminal wraps one koshell process, and many
instances share one daemon — so they must address a specific instance, not the
daemon as a whole.

## Addressing the current instance

Each wrapper already presents a stable identity to the daemon: the `hello`
handshake carries `terminal_session_id = "koshell-<pid>"` (the wrapper pid),
fixed for the life of the session. The daemon previously only logged it.

Two additions make it routable:

- The wrapper exports `KOSHELL_SESSION_ID = "koshell-<pid>"` into the shell
  environment (`session.rs`), so a child `koshell status`/`reload` — spawned
  inside the shell — reads its parent instance's id. Because the id is the fixed
  wrapper pid, the env var never goes stale: reconnects and a future
  `koshell new` all act on the same instance connection, only swapping the
  conversation behind it.
- The daemon keeps a `Map<terminal_session_id, TerminalConnection>` in the
  `startDaemon` closure (`server.ts`): a connection registers on `hello` and
  unregisters on close (identity-checked, so a reconnect under the same id is not
  evicted by the old connection's close).

`koshell status`/`reload` connect to the shared daemon socket on their own
throwaway connections (no `hello`) and carry the target `session_id` in the
message; the daemon routes by that id, decoupled from the requester's own
connection. This mirrors the existing `status_request`/`status` pair, which is
also served without the handshake.

## IPC (additive — no `PROTOCOL_VERSION` bump)

- `reload_request { session_id? }` → `reload { ok, message? }`. `session_id`
  omitted is the `--all` form (every active session).
- `instance_status_request { session_id }` → `instance_status { known,
session_id, cwd?, shell?, model?, conversation, daemon_pid, uptime_ms,
version, protocol_version, connections }`. `known` is whether a live
  connection exists for that id; the per-connection fields are set only when
  known, while the daemon-global fields are always present so `status` can report
  the daemon even for a not-yet-connected instance.

Kept as a flat `ok` + human `message` (reload) and a flat field set (status), so
later Skills/plugin reloading extends the message without a rigid schema.

## Reload semantics

- **Validate first.** The daemon dry-runs `resolveProvider(loadConfig())` once. A
  broken config (parse error, unknown model, missing credential) returns
  `ok: false` with the error and touches nothing — every live session keeps its
  previous, working config.
- **Then reset the target(s).** On success, each target connection's memoized
  agent is dropped so its next `#?` rebuilds from the current config.
  `resetAgent()` defers the teardown onto the connection's FIFO `queue`, so an
  in-flight `#?` finishes on its old session first; the reply is sent immediately
  (it only promises "on the next `#?`").
- **A reset discards the conversation.** The rebuilt session starts a fresh
  conversation — history is intentionally lost, consistent with the MVP bound
  that a conversation dies with its terminal. The client message says so.
- **Scope.** Default is the current instance; `--all` resets every active
  instance. Config is a single global file, so `--all` exists for "apply
  everywhere at once"; the default keeps one terminal's reload from disrupting
  others.
- **No auto-spawn.** If the daemon is not running, `koshell reload` prints a note
  and exits 0 — a freshly started daemon reads the current config anyway.

## Status field sources

`koshell status` composes three sources: local env (session id, `$KOSHELL_TTY`),
the daemon's per-connection snapshot (cwd, shell, active model, whether a
conversation exists), and the daemon-global facts (pid, version, protocol,
uptime, connections). The active model is exposed as `KoshellAgent.modelId`,
cached on the connection when the agent resolves so status reads it
synchronously.

## Exit codes

- `koshell reload`: applied / validated → 0; invalid config or no reply from an
  old daemon → 1; daemon not running → 0.
- `koshell status`: reported → 0; not inside a koshell session, daemon not
  running, or an old daemon that does not answer → 1.

## Forward-compat: `koshell new`

A future `koshell new` (like pi's `/new`) is a sibling on the same routing: a
`new_conversation_request { session_id }` that resets the target instance's agent
to start a fresh conversation (without changing config semantics). Because
`KOSHELL_SESSION_ID` is the fixed wrapper pid, `new` needs no id change and no
env-var update. Skills/plugins, if later loaded daemon-globally, reload as the
shared part of the same `koshell reload`, orthogonal to the per-instance session
reset.

## Trade-offs

- A successful reload / `--all` resets the target instances' conversations
  (history discarded) — the only way to apply new config to a live session; the
  output says so.
- An instance that has not issued a `#?` has no daemon connection, so the daemon
  does not know its `session_id`: reload is a no-op for it (config still
  validated), and status reports `known: false` with the local/daemon-global
  fields only.
- Cross-connection addressing is unauthenticated, which is safe: the socket
  directory is per-user and `0700`.

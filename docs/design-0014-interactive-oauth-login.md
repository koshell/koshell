# Design 0014 — interactive OAuth login (`koshell auth`)

Date: 2026-07-08 16:32:29 CST

Status: accepted, implemented.

## Why

Design 0013 adopted pi's full builtin provider catalog but left one gap:
providers whose credential is an OAuth subscription login. `openai-codex`
(ChatGPT Plus/Pro) has no environment-variable shortcut at all, and
anthropic (Claude Pro/Max) and github-copilot required manually extracting
a token into `ANTHROPIC_OAUTH_TOKEN` / `COPILOT_GITHUB_TOKEN`. This design
ships phase 2 as specified there: `koshell auth login | logout | status
[provider]`, an interactive sign-in that stores the credential in a
koshell-owned file.

The split follows the repository boundary: pi is TypeScript-side and lives
only in the AI daemon, so the login flow runs **inside the daemon**; the
Rust `koshell auth` subcommand is a thin plain-stdio IPC client (no PTY, no
raw mode — the same discipline as `koshell daemon`).

## Semantics

- **`koshell auth login <provider>`** runs pi's OAuth flow for the provider
  (anthropic: browser + PKCE with paste fallback; github-copilot: device
  code, fully headless; openai-codex: a choice of browser or device code)
  and persists the token. The CLI best-effort opens the authorization URL
  in a browser (`open`/`xdg-open`, detached, silent on failure) and always
  prints it. A provider without an OAuth flow is rejected with the list of
  providers that have one, derived from pi's live registry.
- **`koshell auth logout <provider>`** removes the stored credential.
  Idempotent; no token revocation upstream.
- **`koshell auth status [provider]`** reports, per provider, whether a
  usable credential exists and its source: `stored` (a login), then
  `environment` (the conventional variable is set), then `config` (an
  `api_key` in config.toml). With a provider argument the exit code is 0
  only when configured. The provider set reported is the OAuth-capable
  providers plus everything stored plus every config provider carrying an
  `api_key`.
- The commands auto-start the daemon like the first `#?` (same spawn
  resolution chain), and fall back to `koshell daemon start` guidance.
- **Cancellation is the connection.** Ctrl-C keeps its default disposition:
  the CLI dies, the socket closes, and the daemon aborts the flow via the
  `AbortSignal` it passed to pi. The daemon also caps a login at 15 minutes
  so a wedged client cannot pin the connection (and the idle timer) open.

## Credential store

`$XDG_DATA_HOME/koshell/auth.json` (fallback `~/.local/share/koshell/`),
created and written by pi's `FileAuthStorageBackend`: file mode 0600,
directory 0700, cross-process `proper-lockfile` locking, and automatic
in-place refresh of expired OAuth tokens under that lock at request time.
It is koshell's own file — pi's `~/.pi/agent/auth.json` is never read, so
the design-0011 independence boundary holds.

Resolution precedence, verified by test: a stored credential outranks a
config `api_key` for the same provider (pi's request path consults the
auth store first), until `koshell auth logout`. To keep the store free of
config secrets, config `api_key` values no longer pass through
`authStorage.set`; `provider.ts` registers them in memory via
`registerProvider(name, { apiKey, baseUrl?, headers? })`, which pi's
`hasConfiguredAuth` and request path honor equally. A corrupt store file
degrades to "no credentials" with the read error appended to that
`ConfigError`, rather than failing resolution outright.

Because the daemon re-resolves the provider per conversation
(design 0011), a credential stored by `auth login` is picked up by the
next `#?` conversation with no daemon restart.

## IPC contract

Additive message types (design 0004 — no version bump; older receivers
skip unknown types):

- Client → daemon: `auth_login`, `auth_logout`, `auth_status_request`
  (optional `provider`), and `auth_prompt_response` (`value` string or
  null, null meaning the user declined).
- Daemon → client: display events `auth_url`, `auth_device_code`,
  `auth_progress`; the round-trip prompts `auth_prompt` (free text) and
  `auth_select` (option list), correlated by `prompt_id`; and exactly one
  terminal message per request — `auth_result` for login/logout (also the
  error shape for status failures) or `auth_status` with per-provider
  entries.

Each request follows the `ai_request` pattern: `ack` first, then events,
then the terminal message. Auth requests sit behind the normal `hello`
handshake (they mutate a credential store; a version-mismatched daemon
answers with its readable upgrade message). A daemon that predates these
messages never acks, which the CLI turns into "restart the daemon"
guidance after a 2-second wait.

pi's login callbacks map 1:1 onto the messages. `onManualCodeInput` is
deliberately omitted in this version: pi races it against its loopback
callback server, which would leave the single-threaded CLI blocked on
stdin when the browser callback wins. Without it pi falls back to an
`onPrompt` paste when the callback server path is unavailable. The
browser-based flows assume the browser runs on the daemon's machine, which
the Unix-socket transport already implies.

## Mechanics

- `packages/ai-daemon/src/auth-store.ts` — store path resolver and
  `openAuthStorage()`.
- `packages/ai-daemon/src/auth-flow.ts` — `runAuthLogin` (callback→message
  mapping; never rejects), `runAuthLogout`, `buildAuthStatus` (pi's
  `getAuthStatus` reports `configured: true` only for stored credentials,
  so the three-source verdict is composed here).
- `packages/ai-daemon/src/server.ts` — one active login per connection
  with a pending-prompt map; `dispose()` aborts it and resolves pending
  prompts with null so the flow always settles. `openAuthStorage` and
  `loadConfig` are injectable seams for tests.
- `crates/koshell-rs/src/auth_cli.rs` — connect-or-spawn, hello, ack wait,
  then a blocking event loop rendering events and answering prompts from
  stdin. IO-injected `drive()` for tests against a scripted daemon.
- `@earendil-works/pi-ai` became a direct dependency of the daemon (same
  `^0.80.3` range as pi-coding-agent's transitive copy, so it dedupes),
  for the `/oauth` subpath: `OAuthLoginCallbacks`, `getOAuthProviders`,
  and the test-only `registerOAuthProvider`.

## Accepted trade-offs

- Two concurrent logins to the same provider (different CLI invocations)
  serialize on pi's file lock; last write wins.
- If the daemon-side 15-minute cap fires while the CLI sits at a prompt,
  the failure prints only after the user presses Enter (blocking stdin
  read).
- `logout` does not revoke the token upstream; it only forgets it.

## Verification

- `bun test` — protocol parse/serialize cases; server login exchange
  against a fake OAuth provider injected into pi's global registry (full
  sequence, concurrent-login refusal, dispose-mid-prompt, declined prompt,
  unknown provider, logout idempotence, status source composition,
  pre-hello refusal); provider tests for the store default, config keys
  never persisted, stored-credential-only resolution, and
  stored-beats-config precedence.
- `cargo test` — proto round-trips; CLI parse shapes; `auth_cli::drive`
  against a scripted `UnixListener` daemon (login round-trip, select
  mapping, no-ack "too old" hint, failed result, status table and exit
  codes).
- `mandoc -T lint` clean for `koshell.1` and `koshell.toml.5`.
- Manual smoke without real OAuth: a wrapper script registers a fake OAuth
  provider and starts the daemon; `koshell auth login fake` against it
  exercises the full path including the 0600 `auth.json` write.

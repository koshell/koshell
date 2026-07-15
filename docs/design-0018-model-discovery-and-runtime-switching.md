# Design 0018 — model discovery and transcript-preserving runtime switching

Date: 2026-07-12 09:07:24 CST +0800.

Status: implemented, unverified. Product validation was not run for this implementation snapshot before commit at the owner's request (recorded 2026-07-15 11:48:49 CST +0800).

## Why

Koshell requires one configured `provider/id`, but pi 0.80.3 currently supplies more than a thousand builtin models. Requiring a new user to discover an exact id elsewhere and hand-edit `koshell.toml` makes provider setup substantially harder than the `#?` interaction Koshell is trying to simplify.

Changing the model must also not imply changing the conversation. pi's `AgentSession.setModel()` updates the active model, records a `model_change`, and re-clamps the thinking level without clearing the message state. Koshell should expose that capability instead of rebuilding the AgentSession and discarding its transcript.

This design adds a focused `koshell model` command before private beta. It preserves the product rule that one conversation has exactly one active model at any instant; it does not add routing, fallback chains, parallel models, or a `#? /model` command.

## Command contract

```text
koshell model [--session-only]
koshell model show
koshell model list [--all] [QUERY]
koshell model set [--session-only] <provider/id>
```

- Bare `koshell model` opens a searchable terminal picker. It requires interactive stdin and stdout; scripts use `list`, `show`, and `set`.
- The default picker and `list` show models for which the running daemon can currently resolve credentials. `--all` includes the complete live pi catalog and marks unavailable entries.
- Search covers provider id, model id, and display name.
- `show` distinguishes the configured default from the addressed conversation's active model when they differ.
- Inside Koshell, selection changes the current conversation and persists the same model as the default for future conversations. `--session-only` changes only the current conversation and leaves `koshell.toml` byte-for-byte unchanged.
- Outside Koshell, selection changes only the configured default. `--session-only` is rejected.
- Escape or Ctrl+C cancels the picker without changing either state.

The daemon remains the source of model and credential truth. The Rust binary owns only argument parsing, plain table output, and the crossterm picker.

## Catalog and provider boundary

The daemon builds discovery results from a fresh pi `ModelRegistry` and Koshell's own `AuthStorage`. It applies valid custom provider definitions from `koshell.toml`, so custom models appear beside builtins without a second catalog format. Koshell does not ship or cache a static model list.

Each model row carries only concise presentation and validation data: the full `provider/id`, provider, model id, display name, context window, reasoning capability, and whether credentials are currently resolvable. A missing config is valid for builtin discovery. An invalid existing config remains an error, because silently ignoring malformed custom provider or credential settings would make selection misleading.

Environment variables are inherited when the daemon starts. If a key is exported after the daemon is already running, discovery cannot observe it; credential guidance tells the user to restart the daemon in that case. Restarting still loses memory-only conversations, an existing persistence limitation unrelated to model switching.

## Additive IPC

Following design 0004, the feature adds messages without changing protocol version 1:

- `model_list { request_id, query?, all }` → `ack`, then `model_catalog`.
- `model_show { request_id, session_id? }` → `ack`, then `model_state`.
- `model_set { request_id, model, session_id?, session_only }` → `ack`, then `model_result`.

All three requests use a version-matching `hello` on their throwaway CLI connection. `session_id` addresses the parent wrapper from field 0 of `KOSHELL`; the throwaway connection uses its own distinct id and therefore cannot replace the live wrapper in the daemon's session registry. A daemon that predates these additive messages does not acknowledge them; the CLI times out with restart/upgrade guidance instead of hanging.

## Persistent config mutation

Only the root `model` assignment may change. Re-serializing the parsed object with `smol-toml` is prohibited because it would discard comments, ordering, quoting choices, and formatting.

The daemon performs mutation as follows:

1. Create the Koshell config directory with mode `0700` if needed and acquire a `proper-lockfile` lock on that directory.
2. Read the latest file while holding the lock. Replace a supported single-line root `model` value in place, preserving key spelling, whitespace, line ending, and trailing comment; if the root key is absent, insert it before the existing content.
3. Parse and validate the complete proposed text, construct the live registry, resolve the selected model, and require usable credentials. Any unrelated config error aborts without touching the file.
4. Write a unique temporary file in the same directory, fsync it, preserve the existing file's permission bits (or use `0600` for a new file), atomically rename it over `koshell.toml`, and fsync the directory where supported.
5. If applying the corresponding active-session change unexpectedly fails, restore the exact previous bytes while the config lock remains held, then report failure.

Unsupported multiline root model strings are rejected rather than rewritten heuristically. Custom provider blocks, inline credentials, comments, ordering, formatting, and file permissions otherwise remain untouched.

## Runtime switching and ordering

One `KoshellAgent` continues to wrap one pi AgentSession. It exposes a Koshell-owned `setModel(provider/id)` method which resolves the target in the session's existing registry, checks credentials and retained-context capacity, and then delegates to `AgentSession.setModel()`.

The target TerminalConnection serializes a model change on the same FIFO queue as `#?` requests. An answer already in progress finishes on its original model; the change then applies before the next queued question. Other terminal connections are not touched.

Before switching, Koshell compares pi's estimated retained context usage with the target context window and reserves up to 16,384 tokens (bounded by the target's maximum output) for the next response. If the retained history cannot fit, the switch fails without changing the active model or config. Compaction remains disabled, so Koshell never silently summarizes or drops history as part of model selection.

Selecting the already-active model is a runtime no-op. Successful switches update `koshell status` immediately through the agent's mutable model id.

## Reload interaction

A reload still validates the complete new config before applying it. When only the root model selection differs from the active conversation's construction inputs, reload queues the same in-place switch and preserves the transcript. Provider-definition, credential, thinking-level, or future agent-construction changes still require rebuilding the AgentSession and must report that history consequence explicitly.

The first implementation may conservatively rebuild when it cannot prove that the non-model construction inputs are unchanged. It must never describe such a rebuild as a model-only switch.

## Compatibility and failure rules

- Additive protocol messages preserve protocol v1.
- Old daemons ignore requests; the CLI emits explicit upgrade guidance after the bounded acknowledgement wait.
- New daemons ignore unknown old-client messages as before.
- Failed validation, unavailable credentials, insufficient target context, unknown sessions, cancellation, and unsupported config syntax change neither active state nor config.
- `--session-only` requires a live addressed conversation.
- A persistent set may succeed outside Koshell or before the addressed wrapper has created a conversation; it then changes only the future default.

## Verification

Validation status: not run for this implementation snapshot. The following public test coverage was added but has not been executed as part of this commit:

- protocol parsing and Rust JSON round trips;
- catalog breadth, credential filtering, query matching, custom models, and missing/invalid config behavior;
- source-preserving replacement/insertion, comments, CRLF, permissions, mode `0600`, locking, atomic rollback, and validation failure;
- active plus default, session-only, outside-session, unknown-session, same-model, in-flight FIFO, status update, insufficient context, and failed-switch invariants;
- old-daemon acknowledgement timeout and scripted Rust CLI exchanges;
- picker filtering, navigation, cancellation, and non-TTY rejection through pure state helpers;
- model-only reload transcript preservation and explicit rebuild reporting for other config changes;
- README and both man pages.

The intended full validation remains `make check` plus `mandoc -T lint` for both man pages.

## Open issues

- Conversations are still memory-only. Daemon restart, terminal disconnect, and provider-definition rebuild cannot resume them. Resolving that requires the separate persistence design: stable identity, storage permissions, retention/deletion, privacy, and recovery.
- The catalog is pinned to the locked pi dependency and changes only when pi is upgraded.
- The 16,384-token switch reserve is deliberately conservative while compaction is disabled. It can be revisited when Koshell owns an explicit compaction policy and exposes exact capacity guidance.

# Design 0011 — Koshell-owned provider configuration

Date: 2026-07-08 12:57:26 CST

Status: accepted, implemented.

## Why

Until now the AI daemon delegated provider, model, and auth resolution to pi's own
defaults — `~/.pi/agent/auth.json`, then provider environment variables such as
`ANTHROPIC_API_KEY`, with pi picking the first available model. That was fine for a
prototype the owner runs, but it fails the release readiness gate that a stranger
must configure Koshell **without a pre-existing pi setup**, and it leaks pi's full
model catalog into product behavior instead of Koshell choosing one model on
purpose.

This design makes Koshell own its configuration namespace: a single
`config.toml`, validated at the file boundary, adapted into pi's in-memory
auth/model objects, with exactly one active model passed to the pi session factory.
pi's `~/.pi/agent` files are never read.

## Semantics

### Location

`$XDG_CONFIG_HOME/koshell/config.toml`, falling back to
`~/.config/koshell/config.toml`. This matches the XDG namespace the terminal and
daemon already use for the socket, cache, and event log.

### Single active model

The config selects **exactly one** model — the single-active-model rule. There is
no runtime `/model` switching. The daemon reads the config when a conversation is
created, so switching a model is "edit the config, then start a new conversation":
a new terminal (or `#? /new`, once it exists) picks up the change; an in-flight
conversation does not.

### Shape

```toml
# The single active model, as "provider/id". Split on the FIRST slash, so a
# provider whose model ids contain slashes keeps them (openrouter below).
model = "anthropic/claude-sonnet-4-5"

# Optional pi thinking level: off | minimal | low | medium | high | xhigh.
thinking_level = "high"

# A builtin provider (anthropic | openai | openrouter): pi's builtin model
# catalog for this name is used as-is. api_key is optional — omit it to fall
# back to the provider's environment variable (e.g. ANTHROPIC_API_KEY).
[providers.anthropic]
api_key = "sk-ant-..."

# openrouter model ids contain a slash; the first slash splits provider from id,
# so this selects provider "openrouter", model "anthropic/claude-3.5-sonnet".
# model = "openrouter/anthropic/claude-3.5-sonnet"

# A custom provider: a full definition. api, base_url, and api_key are REQUIRED,
# and it must declare at least one model. This is also how you pin a non-default
# API type (openai-completions vs openai-responses vs anthropic-messages, ...).
[providers.mycorp]
api = "openai-completions"
base_url = "https://api.mycorp.example/v1"
api_key = "$MYCORP_API_KEY"

  [[providers.mycorp.models]]
  id = "mycorp-large"
  name = "MyCorp Large"       # optional, defaults to id
  reasoning = true            # optional, default false
  input = ["text"]            # optional, default ["text"]
  context_window = 200000     # optional, default 128000
  max_tokens = 16384          # optional, default 4096
  # cost = { input = 3.0, output = 15.0, ... }  # optional, defaults to 0
```

### API-type exposure and the provider dichotomy

A provider entry is one of two shapes, distinguished by whether it declares
`models`:

- **Builtin auth** (no `models`): only credentials (`api_key`) and, optionally, an
  endpoint or header override (`base_url`, `headers`). pi's builtin catalog for the
  provider name is used unchanged. `api` is rejected here — the builtin models
  already carry their wire format.
- **Custom provider** (`models` present): a full definition. `api`, `base_url`, and
  `api_key` are required. `api` is the pi streaming API the endpoint speaks
  (`anthropic-messages`, `openai-completions`, `openai-responses`, and the rest of
  pi's `Api` set), so a non-default API type is expressed by defining the provider
  fully rather than by a one-off flag on a builtin.

### Credential syntax

`api_key` and `headers` values use pi's own resolution: a literal, `$ENV`/`${ENV}`
interpolation, or `!command` execution (cached for the process lifetime — useful
for a keychain lookup). This is the same trust model as the user's own shell
profile: the config file is user-owned, and `!command` runs with the user's
privileges.

### No-config behavior

An absent, unparseable, or invalid config — or one naming an unknown model or a
model with no configured credentials — raises a `ConfigError` carrying setup
guidance. That error propagates as the `#?` failure, which the terminal already
renders inline (degrade-to-inline), so the shell stays fully usable and the user
sees, in place, what to fix. There is no fallback to pi's default resolution: the
delegation this design removes does not survive as a hidden path.

## Mechanics

Two daemon modules, both `node:`-APIs-only so the Bun-as-packager reversibility
discipline holds:

- `packages/ai-daemon/src/config.ts` — resolves the path, reads the file, parses
  TOML (`smol-toml`), and validates against a Zod schema. Unknown keys are rejected
  (`strictObject`) so a typo surfaces as a config error, not a silent no-op. The
  schema couples `api`/`models` (custom providers) and requires `api`, `base_url`,
  and `api_key` when `models` is present. Exports `KoshellConfig`, `loadConfig`,
  `resolveConfigPath`, `splitModelRef`, and `ConfigError`.
- `packages/ai-daemon/src/provider.ts` — adapts a validated config into
  `AuthStorage.inMemory()` + `ModelRegistry.inMemory()` (neither touches
  `~/.pi/agent`). Builtin providers set an authStorage credential; custom providers
  are registered via `modelRegistry.registerProvider` (pi requires `apiKey` there
  when models are defined). It then resolves the single model with
  `modelRegistry.find(provider, id)`, rejecting an unknown id, and checks
  `modelRegistry.hasConfiguredAuth(model)`, rejecting a model with no key.

`agent-runtime.ts` calls `resolveProvider(loadConfig())` per conversation and
passes `{ authStorage, modelRegistry, model, thinkingLevel }` to
`createAgentSession`. A failed creation clears the connection's memoized agent
promise, so fixing the config and firing another `#?` retries without a restart.

## Ownership

Provider configuration boundary and the single-active-model rule are owned by the
internal `architecture/target-architecture` doc; MVP inclusion is owned by
`product/mvp-scope`. This document is the public implementation record.

## Open issues

- Provider setup UX (guiding a first-time user to write `config.toml`) is not
  designed; the inline error is the current affordance.
- Additional BYOK provider APIs beyond the builtin openai/anthropic/openrouter and
  pi's `Api` set are not curated.
- The terminal tool loop (pull-side context) still does not exist; this design only
  covers provider/model/auth.

## Verification

- `bun test` — new `config.test.ts` (schema accept/reject, missing-file guidance,
  TOML error, model-ref split, custom-provider requirements) and `provider.test.ts`
  (custom literal key, custom `$ENV` key resolved only when the env var is set,
  unknown-model error, builtin-provider auth, missing-credential error). Full daemon
  suite green (51 tests); `bun run typecheck` and `bun run lint` clean.
- End-to-end: driving `createPiAgentFactory()` against a temp `XDG_CONFIG_HOME`
  with no file raises the guided `ConfigError`; with a valid builtin config it
  creates a session whose `model` is defined (no network call at creation).

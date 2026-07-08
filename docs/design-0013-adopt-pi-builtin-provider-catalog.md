# Design 0013 — adopt pi's full builtin provider catalog

Date: 2026-07-08 15:35:18 CST

Status: accepted, implemented.

## Why

Design 0011 gave Koshell its own `config.toml` and described the builtin
provider set as three names: anthropic, openai, openrouter. That enumeration
was documentation, not code — the resolution path has always delegated to
pi's `ModelRegistry`, whose in-memory registry preloads pi's entire builtin
catalog: 30+ providers and over a thousand models, each carrying its
endpoint, wire protocol, context window, output limit, and pricing. A user
who wrote `model = "google/gemini-2.5-flash"` with `GEMINI_API_KEY` exported
was already served correctly; the error messages and docs just refused to
say so, and pointed everyone else at hand-written custom provider blocks.

This design makes the real boundary official: Koshell's builtin provider set
**is** pi's builtin catalog. Users configure one line (`model =
"provider/id"`) plus a credential; model lists, base URLs, and API types stay
pi's job. That is the smallest possible configuration surface, which is the
point.

## Semantics

- **Builtin = pi's catalog, no allowlist.** Any provider pi ships resolves;
  a pi upgrade that adds providers extends Koshell automatically. Koshell
  deliberately keeps no per-provider data of its own beyond error-message
  hints.
- **Errors derive from the registry, not from prose.** An unknown provider
  name lists every builtin provider id (taken from the live registry). An
  unknown model id on a known provider lists that provider's real ids
  (first 8, then a count). The three-name enumeration is gone.
- **Credential errors name the fix.** For commonly used providers the
  "no credentials" error names the conventional environment variable
  (e.g. `GEMINI_API_KEY`). Ambient-credential providers get dedicated
  guidance: `amazon-bedrock` (AWS credential chain — `AWS_PROFILE`, IAM
  keys, `AWS_BEARER_TOKEN_BEDROCK`, container/instance roles) and
  `google-vertex` (Application Default Credentials plus
  `GOOGLE_CLOUD_PROJECT` and `GOOGLE_CLOUD_LOCATION`, or
  `GOOGLE_CLOUD_API_KEY`). Both already pass pi's `hasConfiguredAuth`
  check and stream through the AWS SDK credential chain / ADC — they work
  today with zero Koshell code.
- **Subscription tokens work via the environment.** pi's env conventions
  recognize `ANTHROPIC_OAUTH_TOKEN` (Claude Pro/Max) ahead of
  `ANTHROPIC_API_KEY`, and `COPILOT_GITHUB_TOKEN` for github-copilot, so
  both subscription paths work with an exported token and no login flow.
  `openai-codex` has no env shortcut — it requires the interactive OAuth
  login flow (phase 2 below) and is documented as unsupported until then.

## Mechanics

`packages/ai-daemon/src/provider.ts`:

- `knownProviderIds(registry)` — unique, sorted provider ids from
  `registry.getAll()`; the registry is the single source of truth for what
  "builtin" means.
- The unknown-model error splits into unknown-provider (lists the catalog)
  and unknown-id (lists the provider's ids) cases.
- `ENV_KEY_HINTS` — a small Koshell-side map of provider → conventional
  API-key environment variable, covering the same common subset the man
  page documents. pi resolves these variables itself but does not export
  the name mapping (its `findEnvKeys` reports only variables that are
  currently set), so this copy exists purely for error text. It is kept
  honest by a drift test that sets each hinted variable and asserts pi's
  `hasConfiguredAuth` accepts it — a pi upgrade that renames a variable
  fails the suite instead of silently drifting.

No schema change; `config.ts` is untouched. Custom provider blocks remain
the escape hatch for endpoints pi does not ship.

## Phase 2 (implemented by design-0014): interactive OAuth login

pi's login machinery is cleanly embeddable, so `koshell auth login |
logout | status <provider>` can be a follow-up without re-architecture:

- **Persistence**: `FileAuthStorageBackend(path)` accepts an arbitrary
  path; Koshell would point it at its own store (e.g.
  `$XDG_DATA_HOME/koshell/auth.json`, written 0600 under a
  `proper-lockfile` lock). Koshell would still never read pi's
  `~/.pi/agent/auth.json` — the design-0011 independence boundary holds.
- **Flow**: `AuthStorage.login(providerId, callbacks)` drives the whole
  exchange. The callbacks are async display/prompt hooks ("show this URL",
  "show this device code", "give me a pasted string", `AbortSignal`
  cancellation), so the daemon can marshal them over the IPC socket to the
  Rust CLI for display and input. github-copilot and openai-codex offer
  fully headless device-code flows; anthropic's browser flow binds a
  loopback callback server inside pi and supports paste-back.
- **Config interplay**: once auth storage persists, config-supplied
  `api_key` values must stop going through `authStorage.set` (which would
  write them to disk) and instead be registered via
  `registerProvider(name, { apiKey })`, which `hasConfiguredAuth` and
  request auth already honor.
- **Token refresh** is automatic: `AuthStorage.getApiKey` refreshes expired
  OAuth tokens under the file lock and persists the result.

## Ownership and drift

- `ENV_KEY_HINTS` and the man page's common-provider list are the same
  curated subset and must move together; the hint map is behavior-tested
  against pi, the man page is prose.
- The full catalog is never enumerated in Koshell docs — the error message
  derives it live, and pi's `docs/providers.md` is the reference for the
  rest — so there is no full-list snapshot to drift.

## Open issues

- `openai-codex` was unusable until phase 2 shipped the OAuth login flow
  (design-0014); it now works via `koshell auth login openai-codex`.
- The catalog is pinned by the locked pi version; new upstream providers
  arrive only with a dependency bump.

## Verification

- `bun test` — new cases: any-pi-builtin resolution (google), dynamic
  unknown-provider and unknown-id error listings, env-var name in the
  credential error, catalog breadth (≥ 30 providers), and the
  `ENV_KEY_HINTS` drift test. Full daemon suite green.
- `mandoc -T lint man/koshell.toml.5` clean; the rendered PROVIDERS section
  lists the common subset, ambient-credential guidance, and the pointer to
  pi's provider docs.

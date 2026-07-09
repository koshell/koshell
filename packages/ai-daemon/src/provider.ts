// Adapts a validated Koshell config into pi's auth/model objects.
//
// The model registry stays in memory and never touches pi's ~/.pi/agent files,
// so Koshell owns credential and model resolution end to end. A stranger with
// only a Koshell koshell.toml (and no pi setup) can run `#?`. Credentials come
// from Koshell's own store ($XDG_DATA_HOME/koshell/auth.json, written by
// `koshell auth login` — design 0014) plus the config and the environment;
// config-supplied api_key values are registered in memory and never persisted.
// AgentRuntime passes the single resolved model to the pi session factory — a
// catalog of one — so pi's wider model capability never leaks into product
// behavior.
import { AuthStorage, ModelRegistry } from "@earendil-works/pi-coding-agent";
import { getOAuthProviders } from "@earendil-works/pi-ai/oauth";

import { openAuthStorage } from "./auth-store.ts";
import {
  ConfigError,
  splitModelRef,
  type KoshellConfig,
  type ModelDef,
  type ProviderConfig,
} from "./config.ts";

type RegisterProviderConfig = Parameters<ModelRegistry["registerProvider"]>[1];
type PiModelConfig = NonNullable<RegisterProviderConfig["models"]>[number];
type PiModel = NonNullable<ReturnType<ModelRegistry["find"]>>;

export interface ResolvedProvider {
  authStorage: AuthStorage;
  modelRegistry: ModelRegistry;
  model: PiModel;
  thinkingLevel: KoshellConfig["thinking_level"];
}

// Maps a Koshell model definition (snake_case, defaulted) onto pi's model config.
// Optional keys are only set when present: `exactOptionalPropertyTypes` forbids an
// explicit `undefined`, and the api string union is validated to pi's `Api` values.
function toPiModel(def: ModelDef): PiModelConfig {
  const cost = def.cost;
  const model: PiModelConfig = {
    id: def.id,
    name: def.name ?? def.id,
    reasoning: def.reasoning,
    input: def.input,
    contextWindow: def.context_window,
    maxTokens: def.max_tokens,
    cost: {
      input: cost?.input ?? 0,
      output: cost?.output ?? 0,
      cacheRead: cost?.cache_read ?? 0,
      cacheWrite: cost?.cache_write ?? 0,
    },
  };
  if (def.api !== undefined) {
    model.api = def.api;
  }
  if (def.base_url !== undefined) {
    model.baseUrl = def.base_url;
  }
  return model;
}

function applyProvider(
  registry: ModelRegistry,
  name: string,
  provider: ProviderConfig,
): void {
  if (provider.models !== undefined) {
    // Custom provider: the schema guarantees api, base_url, and api_key. pi requires
    // apiKey on registerProvider when models are defined, so it stays here rather
    // than on authStorage; pi resolves $ENV/!command/literal syntax in the value.
    // The runtime narrowing re-establishes what the schema already enforces, since
    // the inferred type keeps these optional.
    const { api, base_url: baseUrl, api_key: apiKey } = provider;
    if (api === undefined || baseUrl === undefined || apiKey === undefined) {
      throw new ConfigError(
        `custom provider "${name}" is missing api, base_url, or api_key`,
      );
    }
    const config: RegisterProviderConfig = {
      api,
      baseUrl,
      apiKey,
      models: provider.models.map(toPiModel),
    };
    if (provider.headers !== undefined) {
      config.headers = provider.headers;
    }
    registry.registerProvider(name, config);
    return;
  }

  // Builtin provider: use pi's builtin catalog for this name. A config api_key
  // is registered in memory (pi resolves $ENV/!command/literal syntax and both
  // hasConfiguredAuth and the request path honor it) — never written to the
  // persistent auth store, which holds only `koshell auth login` credentials.
  // An endpoint or header override rides the same registration.
  if (
    provider.api_key !== undefined ||
    provider.base_url !== undefined ||
    provider.headers !== undefined
  ) {
    const config: RegisterProviderConfig = {};
    if (provider.api_key !== undefined) {
      config.apiKey = provider.api_key;
    }
    if (provider.base_url !== undefined) {
      config.baseUrl = provider.base_url;
    }
    if (provider.headers !== undefined) {
      config.headers = provider.headers;
    }
    registry.registerProvider(name, config);
  }
}

// How many model ids an unknown-model error lists before "and N more".
const MODEL_SUGGESTION_LIMIT = 8;

// The registry's model list is the source of truth for what "builtin" means:
// every provider pi ships appears here, plus any provider the config registered.
export function knownProviderIds(registry: ModelRegistry): string[] {
  return [...new Set(registry.getAll().map((m) => m.provider))].sort();
}

// Env-var hints for the "no credentials" error, covering the commonly used
// builtin providers (the same subset koshell.toml(5) documents). pi resolves
// these variables itself but does not export the name mapping, so this copy is
// kept honest by a test that sets each variable and asserts pi accepts it.
export const ENV_KEY_HINTS: Record<string, readonly string[]> = {
  anthropic: ["ANTHROPIC_API_KEY", "ANTHROPIC_OAUTH_TOKEN"],
  deepseek: ["DEEPSEEK_API_KEY"],
  "github-copilot": ["COPILOT_GITHUB_TOKEN"],
  google: ["GEMINI_API_KEY"],
  groq: ["GROQ_API_KEY"],
  mistral: ["MISTRAL_API_KEY"],
  moonshotai: ["MOONSHOT_API_KEY"],
  openai: ["OPENAI_API_KEY"],
  openrouter: ["OPENROUTER_API_KEY"],
  xai: ["XAI_API_KEY"],
  zai: ["ZAI_API_KEY"],
};

// Guidance for a "no credentials" failure, per provider. Ambient-credential
// providers (AWS/GCP) and the OAuth-only openai-codex get dedicated hints;
// providers with a login flow (derived from pi's live OAuth registry) offer
// `koshell auth login`; everything else points at api_key and the provider's
// env-var convention.
function credentialsHint(provider: string): string {
  if (provider === "amazon-bedrock") {
    return "configure AWS credentials (AWS_PROFILE, AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY, or AWS_BEARER_TOKEN_BEDROCK).";
  }
  if (provider === "google-vertex") {
    return "set up Application Default Credentials (GOOGLE_APPLICATION_CREDENTIALS or `gcloud auth application-default login`) plus GOOGLE_CLOUD_PROJECT and GOOGLE_CLOUD_LOCATION, or export GOOGLE_CLOUD_API_KEY.";
  }
  if (provider === "openai-codex") {
    return `provider "${provider}" authenticates via OAuth: run \`koshell auth login ${provider}\`.`;
  }
  const login = getOAuthProviders().some((p) => p.id === provider)
    ? `, or run \`koshell auth login ${provider}\``
    : "";
  const envKeys = ENV_KEY_HINTS[provider];
  if (envKeys !== undefined) {
    return `set providers.${provider}.api_key in the config, or export ${envKeys.join(" or ")}${login}.`;
  }
  return `set providers.${provider}.api_key in the config, or export the provider's API key environment variable${login}.`;
}

// Builds the auth/model objects and resolves the single active model. Throws
// ConfigError when the model is unknown or has no configured credentials.
// `authStorage` defaults to Koshell's persistent store so `koshell auth login`
// credentials apply; tests inject AuthStorage.inMemory(). Reading the store is
// best-effort: a corrupt auth.json degrades to "no credentials" and its read
// error is appended to that ConfigError instead of failing resolution outright.
export function resolveProvider(
  config: KoshellConfig,
  authStorage: AuthStorage = openAuthStorage(),
): ResolvedProvider {
  const storeErrors = authStorage.drainErrors();
  const modelRegistry = ModelRegistry.inMemory(authStorage);

  for (const [name, provider] of Object.entries(config.providers)) {
    applyProvider(modelRegistry, name, provider);
  }

  const { provider, id } = splitModelRef(config.model);
  const model = modelRegistry.find(provider, id);
  if (model === undefined) {
    const providers = knownProviderIds(modelRegistry);
    if (!providers.includes(provider)) {
      throw new ConfigError(
        `unknown provider "${provider}" in model "${config.model}". Known providers: ${providers.join(", ")}. A custom provider needs a [providers.${provider}] block with api, base_url, api_key, and models.`,
      );
    }
    const ids = modelRegistry
      .getAll()
      .filter((m) => m.provider === provider)
      .map((m) => m.id);
    const shown = ids.slice(0, MODEL_SUGGESTION_LIMIT);
    const rest = ids.length - shown.length;
    throw new ConfigError(
      `unknown model "${config.model}": provider "${provider}" has no model "${id}". Available models: ${shown.join(", ")}${rest > 0 ? `, and ${String(rest)} more` : ""}.`,
    );
  }
  if (!modelRegistry.hasConfiguredAuth(model)) {
    const storeNote =
      storeErrors.length > 0
        ? ` (note: reading the credential store failed: ${storeErrors.map((e) => e.message).join("; ")})`
        : "";
    throw new ConfigError(
      `no credentials for provider "${provider}": ${credentialsHint(provider)}${storeNote}`,
    );
  }

  return {
    authStorage,
    modelRegistry,
    model,
    thinkingLevel: config.thinking_level,
  };
}

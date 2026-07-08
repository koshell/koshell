// Adapts a validated Koshell config into pi's in-memory auth/model objects.
//
// Everything stays in memory: AuthStorage.inMemory and ModelRegistry.inMemory never
// touch pi's ~/.pi/agent files, so Koshell owns credential and model resolution
// end to end. A stranger with only a Koshell config.toml (and no pi setup) can run
// `#?`. AgentRuntime passes the single resolved model to the pi session factory —
// a catalog of one — so pi's wider model capability never leaks into product
// behavior.
import { AuthStorage, ModelRegistry } from "@earendil-works/pi-coding-agent";

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
  authStorage: AuthStorage,
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

  // Builtin provider: use pi's builtin catalog for this name. Credentials go on
  // authStorage (api_key value or, when absent, pi's provider env-var fallback);
  // an endpoint or header override is registered without touching the models.
  if (provider.api_key !== undefined) {
    authStorage.set(name, { type: "api_key", key: provider.api_key });
  }
  if (provider.base_url !== undefined || provider.headers !== undefined) {
    const config: RegisterProviderConfig = {};
    if (provider.base_url !== undefined) {
      config.baseUrl = provider.base_url;
    }
    if (provider.headers !== undefined) {
      config.headers = provider.headers;
    }
    registry.registerProvider(name, config);
  }
}

// Builds the in-memory auth/model objects and resolves the single active model.
// Throws ConfigError when the model is unknown or has no configured credentials.
export function resolveProvider(config: KoshellConfig): ResolvedProvider {
  const authStorage = AuthStorage.inMemory();
  const modelRegistry = ModelRegistry.inMemory(authStorage);

  for (const [name, provider] of Object.entries(config.providers)) {
    applyProvider(modelRegistry, authStorage, name, provider);
  }

  const { provider, id } = splitModelRef(config.model);
  const model = modelRegistry.find(provider, id);
  if (model === undefined) {
    throw new ConfigError(
      `unknown model "${config.model}": no builtin or configured provider "${provider}" defines model "${id}". Builtin providers are anthropic, openai, and openrouter; a custom provider needs a [providers.${provider}] block with api, base_url, and models.`,
    );
  }
  if (!modelRegistry.hasConfiguredAuth(model)) {
    throw new ConfigError(
      `no credentials for provider "${provider}": set providers.${provider}.api_key in the config, or export the provider's API key environment variable.`,
    );
  }

  return {
    authStorage,
    modelRegistry,
    model,
    thinkingLevel: config.thinking_level,
  };
}

import { afterEach, beforeEach, describe, expect, it } from "bun:test";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { AuthStorage, ModelRegistry } from "@earendil-works/pi-coding-agent";

import { ConfigError, loadConfig } from "../src/config.ts";
import {
  ENV_KEY_HINTS,
  knownProviderIds,
  resolveProvider,
} from "../src/provider.ts";

let dir: string;

// A stable builtin model id for a provider, discovered from pi rather than
// hardcoded so the tests do not drift as pi's catalog changes across releases.
function anyBuiltinId(provider: string): string {
  const registry = ModelRegistry.inMemory(AuthStorage.inMemory());
  const model = registry.getAll().find((m) => m.provider === provider);
  if (model === undefined) {
    throw new Error(`no builtin ${provider} model available in pi`);
  }
  return model.id;
}

// Data-driven env cleanup for the ENV_KEY_HINTS drift test.
function unsetEnv(name: string): void {
  // eslint-disable-next-line @typescript-eslint/no-dynamic-delete -- the var names under test are data
  delete process.env[name];
}

// resolveProvider defaults to the persistent store; tests always inject one so
// the developer's real ~/.local/share/koshell/auth.json never leaks in.
function resolve(
  contents: string,
  authStorage: AuthStorage = AuthStorage.inMemory(),
): ReturnType<typeof resolveProvider> {
  const path = join(dir, "config.toml");
  writeFileSync(path, contents);
  return resolveProvider(loadConfig(path), authStorage);
}

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "koshell-provider-"));
});

afterEach(() => {
  rmSync(dir, { recursive: true, force: true });
  delete process.env.KOSHELL_PROVIDER_TEST_KEY;
});

describe("resolveProvider", () => {
  it("resolves a custom provider with a literal api_key", () => {
    const resolved = resolve(
      [
        'model = "mycorp/big"',
        "[providers.mycorp]",
        'api = "openai-completions"',
        'base_url = "https://api.mycorp.test/v1"',
        'api_key = "sk-literal"',
        "[[providers.mycorp.models]]",
        'id = "big"',
        'name = "Big"',
        "reasoning = true",
        "context_window = 200000",
        "max_tokens = 16384",
        "",
      ].join("\n"),
    );
    expect(resolved.model.provider).toBe("mycorp");
    expect(resolved.model.id).toBe("big");
    expect(resolved.model.api).toBe("openai-completions");
  });

  it("resolves a custom provider whose api_key is an env reference", () => {
    const toml = [
      'model = "mycorp/big"',
      "[providers.mycorp]",
      'api = "openai-completions"',
      'base_url = "https://api.mycorp.test/v1"',
      'api_key = "$KOSHELL_PROVIDER_TEST_KEY"',
      "[[providers.mycorp.models]]",
      'id = "big"',
      "",
    ].join("\n");

    expect(() => resolve(toml)).toThrow(/no credentials/);

    process.env.KOSHELL_PROVIDER_TEST_KEY = "sk-from-env";
    expect(resolve(toml).model.id).toBe("big");
  });

  it("errors on an unknown provider, listing the builtin catalog", () => {
    expect(() => resolve('model = "nope/whatever"\n')).toThrow(ConfigError);
    expect(() => resolve('model = "nope/whatever"\n')).toThrow(
      /unknown provider "nope"/,
    );
    // The provider list is derived from pi's catalog, not hardcoded.
    expect(() => resolve('model = "nope/whatever"\n')).toThrow(
      /Known providers: .*\banthropic\b.*\bgoogle\b.*\bopenrouter\b/,
    );
  });

  it("errors on an unknown model id, listing real ids for the provider", () => {
    const known = anyBuiltinId("anthropic");
    const attempt = (): unknown =>
      resolve('model = "anthropic/definitely-not-a-model"\n');
    expect(attempt).toThrow(ConfigError);
    expect(attempt).toThrow(
      /unknown model "anthropic\/definitely-not-a-model"/,
    );
    expect(attempt).toThrow(
      new RegExp(`Available models: .*${known}|${known}`),
    );
  });

  it("resolves a builtin provider with an api_key", () => {
    const id = anyBuiltinId("anthropic");
    const resolved = resolve(
      [
        `model = "anthropic/${id}"`,
        "[providers.anthropic]",
        'api_key = "sk-ant-literal"',
        "",
      ].join("\n"),
    );
    expect(resolved.model.provider).toBe("anthropic");
    expect(resolved.model.id).toBe(id);
  });

  it("resolves any pi builtin provider, not just the historical three", () => {
    const id = anyBuiltinId("google");
    const resolved = resolve(
      [
        `model = "google/${id}"`,
        "[providers.google]",
        'api_key = "test-google-key"',
        "",
      ].join("\n"),
    );
    expect(resolved.model.provider).toBe("google");
    expect(resolved.model.id).toBe(id);
  });

  it("errors when a builtin provider has no credentials", () => {
    const id = anyBuiltinId("anthropic");
    delete process.env.ANTHROPIC_API_KEY;
    delete process.env.ANTHROPIC_OAUTH_TOKEN;
    expect(() => resolve(`model = "anthropic/${id}"\n`)).toThrow(
      /no credentials/,
    );
    // anthropic has an OAuth login flow, so the error offers it.
    expect(() => resolve(`model = "anthropic/${id}"\n`)).toThrow(
      /koshell auth login anthropic/,
    );
  });

  it("points the OAuth-only openai-codex at koshell auth login", () => {
    const id = anyBuiltinId("openai-codex");
    expect(() => resolve(`model = "openai-codex/${id}"\n`)).toThrow(
      /koshell auth login openai-codex/,
    );
  });

  it("keeps a config api_key out of the persistent credential store", () => {
    const authPath = join(dir, "auth.json");
    const id = anyBuiltinId("anthropic");
    const resolved = resolve(
      [
        `model = "anthropic/${id}"`,
        "[providers.anthropic]",
        'api_key = "sk-ant-secret"',
        "",
      ].join("\n"),
      AuthStorage.create(authPath),
    );
    expect(resolved.model.id).toBe(id);
    expect(readFileSync(authPath, "utf8")).not.toContain("sk-ant-secret");
  });

  it("resolves with a stored OAuth credential and nothing else", () => {
    const authPath = join(dir, "auth.json");
    writeFileSync(
      authPath,
      JSON.stringify({
        anthropic: {
          type: "oauth",
          refresh: "r",
          access: "sk-ant-oat-stored",
          expires: Date.now() + 3_600_000,
        },
      }),
    );
    delete process.env.ANTHROPIC_API_KEY;
    delete process.env.ANTHROPIC_OAUTH_TOKEN;
    const id = anyBuiltinId("anthropic");
    const resolved = resolve(
      `model = "anthropic/${id}"\n`,
      AuthStorage.create(authPath),
    );
    expect(resolved.model.provider).toBe("anthropic");
  });

  it("prefers a stored credential over a config api_key at request time", async () => {
    const authPath = join(dir, "auth.json");
    writeFileSync(
      authPath,
      JSON.stringify({
        anthropic: {
          type: "oauth",
          refresh: "r",
          access: "sk-ant-oat-stored",
          expires: Date.now() + 3_600_000,
        },
      }),
    );
    const id = anyBuiltinId("anthropic");
    const resolved = resolve(
      [
        `model = "anthropic/${id}"`,
        "[providers.anthropic]",
        'api_key = "sk-ant-config"',
        "",
      ].join("\n"),
      AuthStorage.create(authPath),
    );
    // Documented in design-0014: pi's request path consults the auth store
    // before the registered config key, so a login outranks the config until
    // `koshell auth logout`.
    const auth = await resolved.modelRegistry.getApiKeyAndHeaders(
      resolved.model,
    );
    if (!auth.ok) {
      throw new Error(`request auth failed: ${auth.error}`);
    }
    expect(auth.apiKey).toBe("sk-ant-oat-stored");
  });

  it("names the provider's API key environment variable in the error", () => {
    const id = anyBuiltinId("google");
    delete process.env.GEMINI_API_KEY;
    expect(() => resolve(`model = "google/${id}"\n`)).toThrow(/GEMINI_API_KEY/);

    process.env.GEMINI_API_KEY = "test-google-key";
    try {
      expect(resolve(`model = "google/${id}"\n`).model.provider).toBe("google");
    } finally {
      delete process.env.GEMINI_API_KEY;
    }
  });

  it("exposes pi's full builtin catalog", () => {
    const registry = ModelRegistry.inMemory(AuthStorage.inMemory());
    const providers = knownProviderIds(registry);
    // Breadth smoke check: a pi upgrade that shrinks the catalog should fail loudly.
    expect(providers.length).toBeGreaterThanOrEqual(30);
    for (const expected of ["anthropic", "openai", "openrouter", "google"]) {
      expect(providers).toContain(expected);
    }
  });

  // ENV_KEY_HINTS mirrors pi's env-var conventions, which pi does not export.
  // Setting each hinted variable must make pi consider the provider authenticated,
  // so a pi upgrade that renames a variable fails here instead of drifting silently.
  it("env hints match pi's conventions", () => {
    const failures: string[] = [];
    for (const [provider, envVars] of Object.entries(ENV_KEY_HINTS)) {
      for (const envVar of envVars) {
        const saved = new Map(
          envVars.map((name) => [name, process.env[name]] as const),
        );
        try {
          for (const name of envVars) {
            unsetEnv(name);
          }
          process.env[envVar] = "test-env-hint";
          const registry = ModelRegistry.inMemory(AuthStorage.inMemory());
          const model = registry.getAll().find((m) => m.provider === provider);
          if (model === undefined || !registry.hasConfiguredAuth(model)) {
            failures.push(`${provider} via ${envVar}`);
          }
        } finally {
          for (const [name, value] of saved) {
            if (value === undefined) {
              unsetEnv(name);
            } else {
              process.env[name] = value;
            }
          }
        }
      }
    }
    expect(failures).toEqual([]);
  });
});

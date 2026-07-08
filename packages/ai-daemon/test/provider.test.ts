import { afterEach, beforeEach, describe, expect, it } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { AuthStorage, ModelRegistry } from "@earendil-works/pi-coding-agent";

import { ConfigError, loadConfig } from "../src/config.ts";
import { resolveProvider } from "../src/provider.ts";

let dir: string;

// A stable builtin anthropic model id, discovered from pi rather than hardcoded
// so the test does not drift as pi's catalog changes across releases.
function anyBuiltinAnthropicId(): string {
  const registry = ModelRegistry.inMemory(AuthStorage.inMemory());
  const model = registry.getAll().find((m) => m.provider === "anthropic");
  if (model === undefined) {
    throw new Error("no builtin anthropic model available in pi");
  }
  return model.id;
}

function resolve(contents: string): ReturnType<typeof resolveProvider> {
  const path = join(dir, "config.toml");
  writeFileSync(path, contents);
  return resolveProvider(loadConfig(path));
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

  it("errors on an unknown model", () => {
    expect(() => resolve('model = "nope/whatever"\n')).toThrow(ConfigError);
    expect(() => resolve('model = "nope/whatever"\n')).toThrow(/unknown model/);
  });

  it("resolves a builtin provider with an api_key", () => {
    const id = anyBuiltinAnthropicId();
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

  it("errors when a builtin provider has no credentials", () => {
    const id = anyBuiltinAnthropicId();
    delete process.env.ANTHROPIC_API_KEY;
    expect(() => resolve(`model = "anthropic/${id}"\n`)).toThrow(
      /no credentials/,
    );
  });
});

import { afterEach, beforeEach, describe, expect, it } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  ConfigError,
  loadConfig,
  resolveConfigPath,
  splitModelRef,
} from "../src/config.ts";

let dir: string;

function write(contents: string): string {
  const path = join(dir, "koshell.toml");
  writeFileSync(path, contents);
  return path;
}

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "koshell-config-"));
});

afterEach(() => {
  rmSync(dir, { recursive: true, force: true });
});

describe("loadConfig", () => {
  it("accepts a minimal builtin config", () => {
    const config = loadConfig(write('model = "anthropic/claude-sonnet-4-5"\n'));
    expect(config.model).toBe("anthropic/claude-sonnet-4-5");
    expect(config.providers).toEqual({});
  });

  it("reads a builtin provider api_key and a thinking level", () => {
    const config = loadConfig(
      write(
        [
          'model = "anthropic/claude-sonnet-4-5"',
          'thinking_level = "high"',
          "[providers.anthropic]",
          'api_key = "sk-ant-xyz"',
          "",
        ].join("\n"),
      ),
    );
    expect(config.thinking_level).toBe("high");
    expect(config.providers.anthropic?.api_key).toBe("sk-ant-xyz");
  });

  it("defaults custom-model fields and parses a full provider", () => {
    const config = loadConfig(
      write(
        [
          'model = "mycorp/big"',
          "[providers.mycorp]",
          'api = "openai-completions"',
          'base_url = "https://api.mycorp.test/v1"',
          'api_key = "$MYCORP_KEY"',
          "[[providers.mycorp.models]]",
          'id = "big"',
          "",
        ].join("\n"),
      ),
    );
    const model = config.providers.mycorp?.models?.[0];
    expect(model?.id).toBe("big");
    expect(model?.reasoning).toBe(false);
    expect(model?.input).toEqual(["text"]);
    expect(model?.context_window).toBe(128_000);
    expect(model?.max_tokens).toBe(4_096);
  });

  it("guides the user when the file is missing", () => {
    const path = join(dir, "does-not-exist.toml");
    expect(() => loadConfig(path)).toThrow(ConfigError);
    expect(() => loadConfig(path)).toThrow(/no Koshell config/);
  });

  it("reports invalid TOML", () => {
    expect(() => loadConfig(write("model = "))).toThrow(/invalid TOML/);
  });

  it("rejects a model reference without a provider", () => {
    expect(() => loadConfig(write('model = "claude"\n'))).toThrow(
      /provider\/id/,
    );
  });

  it("rejects unknown top-level keys", () => {
    expect(() =>
      loadConfig(write('model = "anthropic/x"\nnope = 1\n')),
    ).toThrow(/invalid config/);
  });

  it("requires api, base_url, and api_key on a custom provider", () => {
    expect(() =>
      loadConfig(
        write(
          [
            'model = "mycorp/big"',
            "[providers.mycorp]",
            "[[providers.mycorp.models]]",
            'id = "big"',
            "",
          ].join("\n"),
        ),
      ),
    ).toThrow(/requires "api"/);
  });

  it("rejects api on a provider without models", () => {
    expect(() =>
      loadConfig(
        write(
          [
            'model = "openai/gpt-x"',
            "[providers.openai]",
            'api = "openai-responses"',
            "",
          ].join("\n"),
        ),
      ),
    ).toThrow(/only applies to a custom provider/);
  });
});

describe("[search]", () => {
  it("is optional", () => {
    expect(
      loadConfig(write('model = "anthropic/claude-sonnet-4-5"')).search,
    ).toBe(undefined);
  });

  it("accepts a backend with a key and defaults max_results", () => {
    const config = loadConfig(
      write(
        [
          'model = "anthropic/claude-sonnet-4-5"',
          "",
          "[search]",
          'provider = "exa"',
          'api_key = "exa-key"',
        ].join("\n"),
      ),
    );
    expect(config.search).toEqual({
      provider: "exa",
      api_key: "exa-key",
      max_results: 5,
    });
  });

  it("accepts a backend without a key, leaving it to the env var", () => {
    const config = loadConfig(
      write(
        [
          'model = "anthropic/claude-sonnet-4-5"',
          "",
          "[search]",
          'provider = "exa"',
        ].join("\n"),
      ),
    );
    expect(config.search?.provider).toBe("exa");
    expect(config.search?.api_key).toBe(undefined);
  });

  it("rejects an unknown backend at the config boundary", () => {
    expect(() =>
      loadConfig(
        write(
          [
            'model = "anthropic/claude-sonnet-4-5"',
            "",
            "[search]",
            'provider = "google"',
          ].join("\n"),
        ),
      ),
    ).toThrow(/search\.provider/);
  });

  // Tavily and Brave adapters were removed before shipping: never run against a live
  // API, so they carried a supported integration's weight while proving nothing.
  // Naming one is a config error, not a silently accepted value.
  it("rejects a backend that was removed", () => {
    expect(() =>
      loadConfig(
        write(
          [
            'model = "anthropic/claude-sonnet-4-5"',
            "",
            "[search]",
            'provider = "brave"',
          ].join("\n"),
        ),
      ),
    ).toThrow(/search\.provider/);
  });

  it("rejects an unknown key in the block", () => {
    expect(() =>
      loadConfig(
        write(
          [
            'model = "anthropic/claude-sonnet-4-5"',
            "",
            "[search]",
            'provider = "exa"',
            'endpoint = "https://example.test"',
          ].join("\n"),
        ),
      ),
    ).toThrow(ConfigError);
  });

  it("bounds max_results", () => {
    expect(() =>
      loadConfig(
        write(
          [
            'model = "anthropic/claude-sonnet-4-5"',
            "",
            "[search]",
            'provider = "exa"',
            "max_results = 50",
          ].join("\n"),
        ),
      ),
    ).toThrow(/max_results/);
  });
});

describe("resolveConfigPath", () => {
  it("honors XDG_CONFIG_HOME", () => {
    expect(resolveConfigPath()).toContain(join("koshell", "koshell.toml"));
  });
});

describe("splitModelRef", () => {
  it("splits on the first slash, keeping slashes in the id", () => {
    expect(splitModelRef("anthropic/claude-sonnet-4-5")).toEqual({
      provider: "anthropic",
      id: "claude-sonnet-4-5",
    });
    expect(splitModelRef("openrouter/anthropic/claude-3.5-sonnet")).toEqual({
      provider: "openrouter",
      id: "anthropic/claude-3.5-sonnet",
    });
  });
});

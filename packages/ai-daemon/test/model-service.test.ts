import { afterEach, describe, expect, it } from "bun:test";
import {
  chmodSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { AuthStorage, ModelRegistry } from "@earendil-works/pi-coding-agent";

import { ConfigError } from "../src/config.ts";
import {
  buildModelCatalog,
  patchRootModel,
  updateDefaultModel,
} from "../src/model-service.ts";

const roots: string[] = [];
const savedAnthropicKey = process.env.ANTHROPIC_API_KEY;

afterEach(() => {
  for (const root of roots.splice(0)) {
    rmSync(root, { recursive: true, force: true });
  }
  if (savedAnthropicKey === undefined) {
    delete process.env.ANTHROPIC_API_KEY;
  } else {
    process.env.ANTHROPIC_API_KEY = savedAnthropicKey;
  }
});

function path(): string {
  const root = mkdtempSync(join(tmpdir(), "koshell-model-test-"));
  roots.push(root);
  const directory = join(root, "koshell");
  mkdirSync(directory);
  return join(directory, "koshell.toml");
}

function customConfig(model = "mycorp/one"): string {
  return [
    `model = "${model}" # keep this comment`,
    'thinking_level = "high"',
    "",
    "[providers.mycorp]",
    'api = "openai-completions"',
    'base_url = "https://example.test/v1"',
    'api_key = "secret"',
    "",
    "[[providers.mycorp.models]]",
    'id = "one"',
    'name = "Model One"',
    "context_window = 64000",
    "",
    "[[providers.mycorp.models]]",
    'id = "two"',
    'name = "Model Two"',
    "context_window = 128000",
    "",
  ].join("\n");
}

describe("patchRootModel", () => {
  it("changes only a one-line root value and preserves formatting", () => {
    const before = [
      "# heading\r\n",
      "\"model\"  =  'old/id'   # chosen\r\n",
      'thinking_level = "high"\r\n',
      "[providers.old]\r\n",
      'api_key = "secret"\r\n',
    ].join("");
    expect(patchRootModel(before, "new/provider/model")).toBe(
      before.replace("'old/id'", '"new/provider/model"'),
    );
  });

  it("inserts a missing root key before existing provider blocks", () => {
    const before = '# custom setup\n[providers.mycorp]\napi_key = "x"\n';
    expect(patchRootModel(before, "mycorp/one")).toBe(
      `model = "mycorp/one"\n${before}`,
    );
  });

  it("rejects multiline model syntax rather than guessing", () => {
    expect(() => patchRootModel('model = """old/id"""\n', "new/id")).toThrow(
      ConfigError,
    );
  });
});

describe("buildModelCatalog", () => {
  it("lists the live builtin catalog without an existing config", () => {
    const catalog = buildModelCatalog({
      all: true,
      path: path(),
      authStorage: AuthStorage.inMemory(),
    });
    expect(catalog.configured_model).toBeUndefined();
    expect(
      new Set(catalog.entries.map((entry) => entry.provider)).size,
    ).toBeGreaterThanOrEqual(30);
    expect(catalog.entries.length).toBeGreaterThan(1_000);
  });

  it("includes available custom models and searches display names", () => {
    const configPath = path();
    writeFileSync(configPath, customConfig());
    const catalog = buildModelCatalog({
      all: false,
      query: "model two",
      path: configPath,
      authStorage: AuthStorage.inMemory(),
    });
    expect(catalog.configured_model).toBe("mycorp/one");
    expect(catalog.entries.map((entry) => entry.ref)).toEqual(["mycorp/two"]);
    expect(catalog.entries[0]?.available).toBe(true);
  });

  it("does not hide an invalid existing config", () => {
    const configPath = path();
    writeFileSync(configPath, "not valid TOML");
    expect(() =>
      buildModelCatalog({
        all: true,
        path: configPath,
        authStorage: AuthStorage.inMemory(),
      }),
    ).toThrow(/invalid/);
  });
});

describe("updateDefaultModel", () => {
  it("preserves source bytes outside the root value and file permissions", async () => {
    const configPath = path();
    const before = customConfig();
    writeFileSync(configPath, before, { mode: 0o640 });
    chmodSync(configPath, 0o640);

    const config = await updateDefaultModel("mycorp/two", { path: configPath });

    expect(config.model).toBe("mycorp/two");
    expect(readFileSync(configPath, "utf8")).toBe(
      before.replace('model = "mycorp/one"', 'model = "mycorp/two"'),
    );
    expect(statSync(configPath).mode & 0o777).toBe(0o640);
  });

  it("creates a fresh config with mode 0600", async () => {
    const configPath = path();
    const registry = ModelRegistry.inMemory(AuthStorage.inMemory());
    const model = registry
      .getAll()
      .find((candidate) => candidate.provider === "anthropic");
    if (model === undefined) {
      throw new Error("pi has no anthropic model");
    }
    process.env.ANTHROPIC_API_KEY = "test-key";

    await updateDefaultModel(`anthropic/${model.id}`, { path: configPath });

    expect(readFileSync(configPath, "utf8")).toBe(
      `model = "anthropic/${model.id}"\n`,
    );
    expect(statSync(configPath).mode & 0o777).toBe(0o600);
  });

  it("rolls back exact bytes when active-session application fails", async () => {
    const configPath = path();
    const before = customConfig();
    writeFileSync(configPath, before);

    let failure: unknown;
    try {
      await updateDefaultModel("mycorp/two", {
        path: configPath,
        apply: () => Promise.reject(new Error("context too large")),
      });
    } catch (error) {
      failure = error;
    }
    expect(failure).toBeInstanceOf(Error);
    expect((failure as Error).message).toContain("context too large");
    expect(readFileSync(configPath, "utf8")).toBe(before);
  });

  it("leaves the file untouched when unrelated validation fails", async () => {
    const configPath = path();
    const before = `${customConfig()}unknown = true\n`;
    writeFileSync(configPath, before);

    let failure: unknown;
    try {
      await updateDefaultModel("mycorp/two", { path: configPath });
    } catch (error) {
      failure = error;
    }
    expect(failure).toBeInstanceOf(Error);
    expect((failure as Error).message).toContain("unknown");
    expect(readFileSync(configPath, "utf8")).toBe(before);
  });

  it("serializes concurrent writers without producing partial TOML", async () => {
    const configPath = path();
    writeFileSync(configPath, customConfig());

    await Promise.all([
      updateDefaultModel("mycorp/one", { path: configPath }),
      updateDefaultModel("mycorp/two", { path: configPath }),
    ]);

    const final = readFileSync(configPath, "utf8");
    expect(final).toMatch(
      /^model = "mycorp\/(?:one|two)" # keep this comment$/m,
    );
    expect(final).toContain("[[providers.mycorp.models]]");
  });
});

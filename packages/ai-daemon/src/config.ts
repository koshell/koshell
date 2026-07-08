// Koshell-owned provider configuration.
//
// Koshell owns its configuration namespace instead of delegating to pi's own
// resolution (~/.pi/agent/auth.json, models.json, provider env vars). This module
// resolves, reads, and validates `config.toml` at the file boundary; adapting the
// validated value into pi's in-memory auth/model objects is `provider.ts`.
//
// The config selects exactly one active model (the single-active-model rule). The
// daemon reads it when a conversation is created, so switching a model is "edit the
// config, then start a new conversation" — there is no reload mechanism and no
// runtime `/model` switching in the MVP.
import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import process from "node:process";

import { parse as parseToml, TomlError } from "smol-toml";
import { z } from "zod";

// A configuration problem the user must fix: a missing file, a parse error, an
// invalid schema, an unknown model, or missing credentials. The daemon surfaces
// the message inline on `#?` (as an ai_error), so it must read as setup guidance,
// not a stack trace.
export class ConfigError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ConfigError";
  }
}

// The pi streaming API a provider speaks. Exposed so a custom provider (or a
// builtin override) can declare its wire format explicitly instead of having it
// inferred from a provider name. Mirrors pi's `Api` union; kept as a curated set
// so a typo in the config is rejected at the boundary rather than at request time.
const API_TYPES = [
  "anthropic-messages",
  "openai-completions",
  "openai-responses",
  "azure-openai-responses",
  "openai-codex-responses",
  "mistral-conversations",
  "google-generative-ai",
  "google-vertex",
  "bedrock-converse-stream",
] as const;
const ApiSchema = z.enum(API_TYPES);

// pi thinking levels, forward-compatible with a later per-conversation override.
const ThinkingLevelSchema = z.enum([
  "off",
  "minimal",
  "low",
  "medium",
  "high",
  "xhigh",
]);

// Per-million-token costs. Optional and defaulted to zero: users configuring a
// BYOK model rarely know these, and they only feed pi's usage accounting.
const CostSchema = z.strictObject({
  input: z.number().nonnegative().default(0),
  output: z.number().nonnegative().default(0),
  cache_read: z.number().nonnegative().default(0),
  cache_write: z.number().nonnegative().default(0),
});

// A model definition for a custom provider. Only `id` is required; the rest carry
// defaults sized for a typical hosted chat model.
const ModelDefSchema = z.strictObject({
  id: z.string().min(1),
  name: z.string().min(1).optional(),
  api: ApiSchema.optional(),
  base_url: z.string().min(1).optional(),
  reasoning: z.boolean().default(false),
  input: z
    .array(z.enum(["text", "image"]))
    .min(1)
    .default(["text"]),
  context_window: z.number().int().positive().default(128_000),
  max_tokens: z.number().int().positive().default(4_096),
  cost: CostSchema.optional(),
});

// A provider entry. Two shapes:
//   - builtin auth: only `api_key`/`headers`/`base_url` — pi's builtin model
//     catalog for this provider name is used as-is (with an optional endpoint or
//     header override); credentials come from `api_key` or the provider env var.
//   - custom provider: `models` present — a full definition requiring `api`,
//     `base_url`, and `api_key` (enforced below), replacing pi's catalog for the
//     provider name.
// `api` is only meaningful alongside `models`, so it is coupled to it.
const ProviderSchema = z.strictObject({
  api_key: z.string().min(1).optional(),
  api: ApiSchema.optional(),
  base_url: z.string().min(1).optional(),
  headers: z.record(z.string(), z.string()).optional(),
  models: z.array(ModelDefSchema).min(1).optional(),
});

const ConfigSchema = z
  .strictObject({
    // The single active model as "provider/id". Split on the first "/", so a
    // provider whose model ids contain slashes (e.g. openrouter's
    // "anthropic/claude-3.5-sonnet") keeps the slash in the id.
    model: z.string().min(1),
    thinking_level: ThinkingLevelSchema.optional(),
    providers: z.record(z.string(), ProviderSchema).default({}),
  })
  .superRefine((cfg, ctx) => {
    const slash = cfg.model.indexOf("/");
    if (slash <= 0 || slash >= cfg.model.length - 1) {
      ctx.addIssue({
        code: "custom",
        path: ["model"],
        message: `model must be "provider/id" (got "${cfg.model}")`,
      });
    }
    for (const [name, provider] of Object.entries(cfg.providers)) {
      const isCustom = provider.models !== undefined;
      if (isCustom) {
        for (const field of ["api", "base_url", "api_key"] as const) {
          if (provider[field] === undefined) {
            ctx.addIssue({
              code: "custom",
              path: ["providers", name, field],
              message: `custom provider "${name}" (with models) requires "${field}"`,
            });
          }
        }
      } else if (provider.api !== undefined) {
        ctx.addIssue({
          code: "custom",
          path: ["providers", name, "api"],
          message: `provider "${name}": "api" only applies to a custom provider, which also needs "base_url" and "models"`,
        });
      }
    }
  });

export type KoshellConfig = z.infer<typeof ConfigSchema>;
export type ProviderConfig = z.infer<typeof ProviderSchema>;
export type ModelDef = z.infer<typeof ModelDefSchema>;

// Resolves the config path, following XDG: $XDG_CONFIG_HOME/koshell/config.toml,
// falling back to ~/.config/koshell/config.toml.
export function resolveConfigPath(): string {
  const configHome = process.env.XDG_CONFIG_HOME;
  if (configHome !== undefined && configHome.length > 0) {
    return join(configHome, "koshell", "config.toml");
  }
  return join(homedir(), ".config", "koshell", "config.toml");
}

const SAMPLE_CONFIG = `  # ~/.config/koshell/config.toml
  model = "anthropic/claude-sonnet-4-5"

  [providers.anthropic]
  api_key = "sk-ant-..."   # or omit and export ANTHROPIC_API_KEY`;

// Reads and validates the config. Throws ConfigError with setup guidance when the
// file is missing, unparseable, or invalid; the daemon surfaces the message inline.
export function loadConfig(pathOverride?: string): KoshellConfig {
  const path = pathOverride ?? resolveConfigPath();

  let text: string;
  try {
    text = readFileSync(path, "utf8");
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      throw new ConfigError(
        `no Koshell config at ${path}. Create it to choose your AI model, e.g.:\n\n${SAMPLE_CONFIG}`,
      );
    }
    throw new ConfigError(
      `cannot read ${path}: ${error instanceof Error ? error.message : String(error)}`,
    );
  }

  let parsed: unknown;
  try {
    parsed = parseToml(text);
  } catch (error) {
    const detail = error instanceof TomlError ? error.message : String(error);
    throw new ConfigError(`invalid TOML in ${path}: ${detail}`);
  }

  const result = ConfigSchema.safeParse(parsed);
  if (!result.success) {
    const issues = result.error.issues
      .map((issue) => {
        const where = issue.path.join(".");
        return where.length > 0 ? `${where}: ${issue.message}` : issue.message;
      })
      .join("; ");
    throw new ConfigError(`invalid config in ${path}: ${issues}`);
  }
  return result.data;
}

// Splits a validated "provider/id" model reference on the first slash.
export function splitModelRef(ref: string): { provider: string; id: string } {
  const slash = ref.indexOf("/");
  return { provider: ref.slice(0, slash), id: ref.slice(slash + 1) };
}

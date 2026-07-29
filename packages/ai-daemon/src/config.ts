// Koshell-owned provider configuration.
//
// Koshell owns its configuration namespace instead of delegating to pi's own
// resolution (~/.pi/agent/auth.json, models.json, provider env vars). This module
// resolves, reads, and validates `koshell.toml` at the file boundary; adapting the
// validated value into pi's in-memory auth/model objects is `provider.ts`.
//
// The config selects one default model for new conversations. `koshell model`
// source-preservingly updates that root value, while a live conversation may
// temporarily use a different active model (design 0018).
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

// Web-search backends Koshell can call. pi's tool abstraction carries no provider
// server-tool passthrough (its `Tool` is name/description/parameters only, and every
// API adapter converts to a plain function schema), so Anthropic's `web_search`,
// OpenAI's `web_search`, and Gemini's `google_search` grounding are all unreachable
// through the configured model. Search is therefore a Koshell-owned custom tool over a
// dedicated search API, which also keeps it independent of which of pi's 30+ providers
// the user selected. See `search.ts`.
// `exa` is the only backend, and the adapter follows Exa's own published guidance for
// agent workflows. Tavily and Brave adapters existed briefly and were removed before
// this ever shipped: both were written from vendor documentation, never run against
// the live service, and covered only by fixtures derived from those same documents —
// so they carried the maintenance weight of a supported integration while proving
// nothing. `provider` stays a required enum rather than being dropped so that the
// second backend, if one is ever warranted, needs no config-shape change.
const SEARCH_BACKENDS = ["exa"] as const;
const SearchBackendSchema = z.enum(SEARCH_BACKENDS);

// Absent `[search]` means no web_search tool is registered at all — not a tool that
// fails at call time. An agent that is never told the tool exists cannot promise the
// user a search it cannot run.
const SearchSchema = z.strictObject({
  provider: SearchBackendSchema,
  api_key: z.string().min(1).optional(),
  base_url: z.string().min(1).optional(),
  // Results requested per call, before the per-call character budget trims them.
  max_results: z.number().int().positive().max(20).default(5),
});

const ConfigSchema = z
  .strictObject({
    // The single active model as "provider/id". Split on the first "/", so a
    // provider whose model ids contain slashes (e.g. openrouter's
    // "anthropic/claude-3.5-sonnet") keeps the slash in the id.
    model: z.string().min(1),
    thinking_level: ThinkingLevelSchema.optional(),
    providers: z.record(z.string(), ProviderSchema).default({}),
    search: SearchSchema.optional(),
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
export type SearchConfig = z.infer<typeof SearchSchema>;
export type SearchBackend = (typeof SEARCH_BACKENDS)[number];

// Resolves Koshell's config directory, following XDG: $XDG_CONFIG_HOME/koshell,
// falling back to ~/.config/koshell.
export function resolveConfigDir(): string {
  const configHome = process.env.XDG_CONFIG_HOME;
  if (configHome !== undefined && configHome.length > 0) {
    return join(configHome, "koshell");
  }
  return join(homedir(), ".config", "koshell");
}

export function resolveConfigPath(): string {
  return join(resolveConfigDir(), "koshell.toml");
}

/** The optional standing-instructions file, alongside `koshell.toml`. */
export const USER_INSTRUCTIONS_FILENAME = "AGENTS.md";

// A hard ceiling, because this text is prepended to every `#?` for the life of the
// conversation: an unbounded file would silently spend the context budget that the
// terminal evidence needs. The head is kept rather than the tail — the opposite of
// how terminal context is trimmed — because instructions are written most-important
// first, whereas terminal output ends with the part being asked about.
const USER_INSTRUCTIONS_MAX_BYTES = 16 * 1024;

export interface UserInstructions {
  /** Absolute path, quoted to the model so it can name what it is following. */
  path: string;
  text: string;
  /** True when the file exceeded the byte ceiling and only its head was kept. */
  truncated: boolean;
}

export function resolveUserInstructionsPath(): string {
  return join(resolveConfigDir(), USER_INSTRUCTIONS_FILENAME);
}

// Reads the optional AGENTS.md next to `koshell.toml`. Absent or blank returns
// undefined, which is the common case and not a problem. An unreadable file throws,
// so the caller can say so once rather than silently answering without instructions
// the user believes are in force — a wrong permission bit should not be invisible.
export function loadUserInstructions(
  pathOverride?: string,
): UserInstructions | undefined {
  const path = pathOverride ?? resolveUserInstructionsPath();

  let raw: string;
  try {
    raw = readFileSync(path, "utf8");
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      return undefined;
    }
    throw new ConfigError(
      `cannot read ${path}: ${error instanceof Error ? error.message : String(error)}`,
    );
  }

  const text = raw.trim();
  if (text.length === 0) {
    return undefined;
  }

  const bytes = Buffer.from(text, "utf8");
  if (bytes.byteLength <= USER_INSTRUCTIONS_MAX_BYTES) {
    return { path, text, truncated: false };
  }
  // Decode with the replacement-free default and drop a trailing partial character:
  // slicing bytes can split a multi-byte sequence, and a stray U+FFFD in the middle
  // of the user's instructions is worse than one lost character.
  const head = new TextDecoder("utf-8", { fatal: false })
    .decode(bytes.subarray(0, USER_INSTRUCTIONS_MAX_BYTES))
    .replace(/�$/, "");
  return { path, text: head.trimEnd(), truncated: true };
}

// Parses and validates config text. Exported for the source-preserving model
// updater, which validates proposed bytes before atomically replacing the file.
export function parseConfigText(text: string, path: string): KoshellConfig {
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
        `no Koshell config at ${path}. Run \`koshell model\` to choose your AI model, or \`man koshell.toml\` to configure it manually.`,
      );
    }
    throw new ConfigError(
      `cannot read ${path}: ${error instanceof Error ? error.message : String(error)}`,
    );
  }

  return parseConfigText(text, path);
}

// Splits a validated "provider/id" model reference on the first slash.
export function splitModelRef(ref: string): { provider: string; id: string } {
  const slash = ref.indexOf("/");
  return { provider: ref.slice(0, slash), id: ref.slice(slash + 1) };
}

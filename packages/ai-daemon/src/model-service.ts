import {
  closeSync,
  constants,
  fsyncSync,
  mkdirSync,
  openSync,
  readFileSync,
  renameSync,
  statSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { dirname, join } from "node:path";
import process from "node:process";

import { type AuthStorage } from "@earendil-works/pi-coding-agent";
import { lock } from "proper-lockfile";

import { openAuthStorage } from "./auth-store.ts";
import {
  ConfigError,
  type KoshellConfig,
  parseConfigText,
  resolveConfigPath,
} from "./config.ts";
import { buildModelRegistry, resolveProvider } from "./provider.ts";

export interface ModelCatalogEntry {
  ref: string;
  provider: string;
  id: string;
  name: string;
  available: boolean;
  context_window: number;
  reasoning: boolean;
}

export interface ModelCatalog {
  configured_model?: string;
  entries: ModelCatalogEntry[];
}

export interface ModelCatalogOptions {
  all: boolean;
  query?: string;
  path?: string;
  authStorage?: AuthStorage;
}

function readOptionalConfig(path: string): KoshellConfig | undefined {
  let text: string;
  try {
    text = readFileSync(path, "utf8");
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      return undefined;
    }
    throw new ConfigError(
      `cannot read ${path}: ${error instanceof Error ? error.message : String(error)}`,
    );
  }
  return parseConfigText(text, path);
}

export function buildModelCatalog(options: ModelCatalogOptions): ModelCatalog {
  const path = options.path ?? resolveConfigPath();
  const config = readOptionalConfig(path);
  const authStorage = options.authStorage ?? openAuthStorage();
  // Drain store-load errors so later credential failures do not report stale
  // diagnostics repeatedly. Discovery still shows entries as unavailable.
  authStorage.drainErrors();
  const registry = buildModelRegistry(config, authStorage);
  const query = options.query?.trim().toLocaleLowerCase();
  const entries = registry
    .getAll()
    .map((model): ModelCatalogEntry => {
      const ref = `${model.provider}/${model.id}`;
      return {
        ref,
        provider: model.provider,
        id: model.id,
        name: model.name,
        available: registry.hasConfiguredAuth(model),
        context_window: model.contextWindow,
        reasoning: model.reasoning,
      };
    })
    .filter((entry) => options.all || entry.available)
    .filter(
      (entry) =>
        query === undefined ||
        query.length === 0 ||
        entry.ref.toLocaleLowerCase().includes(query) ||
        entry.provider.toLocaleLowerCase().includes(query) ||
        entry.id.toLocaleLowerCase().includes(query) ||
        entry.name.toLocaleLowerCase().includes(query),
    )
    .sort((left, right) => left.ref.localeCompare(right.ref));

  return {
    ...(config?.model !== undefined ? { configured_model: config.model } : {}),
    entries,
  };
}

// Root model values are intentionally limited to one-line TOML strings. This
// covers the documented format while avoiding a heuristic rewrite of multiline
// strings or comments containing quote characters.
const ROOT_MODEL_LINE =
  /^(\s*(?:model|"model"|'model')\s*=\s*)(?:"(?:\\.|[^"\\])*"|'[^']*')([ \t]*(?:#.*)?)(\r?\n)?$/;
const ROOT_MODEL_PREFIX = /^\s*(?:model|"model"|'model')\s*=/;
const TABLE_HEADER = /^\s*\[/;

export function patchRootModel(text: string, ref: string): string {
  const replacement = JSON.stringify(ref);
  let offset = 0;
  for (const line of text.match(/[^\n]*(?:\n|$)/g) ?? []) {
    if (line.length === 0) {
      continue;
    }
    if (TABLE_HEADER.test(line)) {
      break;
    }
    if (ROOT_MODEL_PREFIX.test(line)) {
      const match = ROOT_MODEL_LINE.exec(line);
      if (match === null) {
        throw new ConfigError(
          "the root model value must be a single-line TOML string before `koshell model` can update it",
        );
      }
      const prefix = match[1];
      const suffix = match[2];
      const newline = match[3] ?? "";
      if (prefix === undefined || suffix === undefined) {
        throw new ConfigError("could not identify the root model assignment");
      }
      const updated = `${prefix}${replacement}${suffix}${newline}`;
      return `${text.slice(0, offset)}${updated}${text.slice(offset + line.length)}`;
    }
    offset += line.length;
  }
  return `model = ${replacement}\n${text}`;
}

function fsyncDirectory(path: string): void {
  let fd: number | undefined;
  try {
    fd = openSync(path, constants.O_RDONLY);
    fsyncSync(fd);
  } catch (error) {
    // Some filesystems do not support fsync on directories. The file itself was
    // already fsynced before rename; directory fsync is best-effort portability.
    if ((error as NodeJS.ErrnoException).code !== "EINVAL") {
      throw error;
    }
  } finally {
    if (fd !== undefined) {
      closeSync(fd);
    }
  }
}

function atomicWrite(path: string, text: string, mode: number): void {
  const directory = dirname(path);
  const temporary = join(
    directory,
    `.koshell.toml.${String(process.pid)}.${crypto.randomUUID()}.tmp`,
  );
  let fd: number | undefined;
  try {
    fd = openSync(
      temporary,
      constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL,
      mode,
    );
    writeFileSync(fd, text, "utf8");
    fsyncSync(fd);
    closeSync(fd);
    fd = undefined;
    renameSync(temporary, path);
    fsyncDirectory(directory);
  } catch (error) {
    if (fd !== undefined) {
      closeSync(fd);
    }
    try {
      unlinkSync(temporary);
    } catch {
      // The rename may already have consumed the temporary path.
    }
    throw error;
  }
}

export interface UpdateDefaultModelOptions {
  path?: string;
  // Runs after the validated bytes are installed while the config lock remains
  // held. A failure restores the exact previous bytes before it is rethrown.
  apply?: (config: KoshellConfig) => Promise<void>;
}

export async function updateDefaultModel(
  ref: string,
  options: UpdateDefaultModelOptions = {},
): Promise<KoshellConfig> {
  const path = options.path ?? resolveConfigPath();
  const directory = dirname(path);
  mkdirSync(directory, { recursive: true, mode: 0o700 });
  const release = await lock(directory, {
    realpath: false,
    lockfilePath: join(directory, ".koshell.toml.lock"),
    retries: { retries: 4, minTimeout: 25, maxTimeout: 200 },
  });

  try {
    let previous: string | undefined;
    let mode = 0o600;
    try {
      previous = readFileSync(path, "utf8");
      mode = statSync(path).mode & 0o777;
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT") {
        throw error;
      }
    }

    const updated = patchRootModel(previous ?? "", ref);
    const config = parseConfigText(updated, path);
    // A faithful dry run validates model existence and credentials using the
    // complete proposed config, including preserved custom provider blocks.
    resolveProvider(config);

    const changed = updated !== previous;
    if (changed) {
      atomicWrite(path, updated, mode);
    }
    try {
      await options.apply?.(config);
    } catch (error) {
      if (changed) {
        if (previous === undefined) {
          try {
            unlinkSync(path);
            fsyncDirectory(directory);
          } catch (rollbackError) {
            throw new AggregateError(
              [error, rollbackError],
              "model switch failed and the new config could not be removed",
              { cause: rollbackError },
            );
          }
        } else {
          try {
            atomicWrite(path, previous, mode);
          } catch (rollbackError) {
            throw new AggregateError(
              [error, rollbackError],
              "model switch failed and koshell.toml rollback also failed",
              { cause: rollbackError },
            );
          }
        }
      }
      throw error;
    }
    return config;
  } finally {
    await release();
  }
}

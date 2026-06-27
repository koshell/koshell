import { accessSync, constants } from "node:fs";
import { delimiter, isAbsolute, join } from "node:path";

const FALLBACK_SHELLS = [
  "/bin/zsh",
  "/bin/bash",
  "/bin/sh",
  "/usr/bin/zsh",
  "/usr/bin/bash",
  "/usr/bin/sh",
] as const;

const FALLBACK_PATH = "/usr/bin:/bin:/usr/sbin:/sbin";

const KOSHELL_ENV_VALUE = "1";

export function isNestedKoshell(env: NodeJS.ProcessEnv = process.env): boolean {
  return env.KOSHELL === KOSHELL_ENV_VALUE;
}

export function assertNotNestedKoshell(
  env: NodeJS.ProcessEnv = process.env,
): void {
  if (isNestedKoshell(env)) {
    throw new Error(
      "koshell is already running in this shell. Start a new regular terminal session before launching koshell again.",
    );
  }
}

export function resolveShell(env: NodeJS.ProcessEnv = process.env): string {
  const configuredShell = env.SHELL?.trim();

  if (configuredShell) {
    const resolvedConfiguredShell = resolveExecutable(
      configuredShell,
      env.PATH,
    );

    if (resolvedConfiguredShell) {
      return resolvedConfiguredShell;
    }
  }

  for (const fallbackShell of FALLBACK_SHELLS) {
    if (isExecutable(fallbackShell)) {
      return fallbackShell;
    }
  }

  throw new Error(
    `No executable shell found. SHELL was ${configuredShell ? JSON.stringify(configuredShell) : "unset or empty"}.`,
  );
}

export function createPtyEnv(
  source: NodeJS.ProcessEnv = process.env,
): Record<string, string> {
  const env: Record<string, string> = {};

  for (const [key, value] of Object.entries(source)) {
    if (typeof value === "string") {
      env[key] = value;
    }
  }

  const sourcePath = source.PATH;
  env.KOSHELL = KOSHELL_ENV_VALUE;
  env.PATH =
    typeof sourcePath === "string" && sourcePath.trim()
      ? sourcePath
      : FALLBACK_PATH;

  return env;
}

function resolveExecutable(
  command: string,
  pathValue: string | undefined,
): string | undefined {
  if (isAbsolute(command)) {
    return isExecutable(command) ? command : undefined;
  }

  const paths = pathValue?.trim()
    ? pathValue.split(delimiter)
    : FALLBACK_PATH.split(delimiter);

  for (const pathEntry of paths) {
    const candidate = join(pathEntry, command);

    if (isExecutable(candidate)) {
      return candidate;
    }
  }

  return undefined;
}

function isExecutable(path: string): boolean {
  try {
    accessSync(path, constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

// Leveled logging for the daemon console. The level resolves in priority order:
// `--log-level <level>` (or `--log-level=<level>`) argument, then the KOSHELL_LOG
// environment variable, then "info". Unlike the Rust terminal process the daemon
// owns its stdout, so log lines go straight to the console.

export type LogLevel = "off" | "error" | "warn" | "info" | "debug";

export interface Logger {
  error(message: string): void;
  warn(message: string): void;
  info(message: string): void;
  debug(message: string): void;
}

const LEVEL_RANK: Record<LogLevel, number> = {
  off: 0,
  error: 1,
  warn: 2,
  info: 3,
  debug: 4,
};

const DEFAULT_LEVEL: LogLevel = "info";

function asLogLevel(value: string | undefined): LogLevel | undefined {
  return value !== undefined && value in LEVEL_RANK
    ? (value as LogLevel)
    : undefined;
}

// Resolves the effective log level from CLI arguments and the environment.
export function resolveLogLevel(
  argv: readonly string[],
  env: Readonly<Record<string, string | undefined>>,
): LogLevel {
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === "--log-level") {
      return asLogLevel(argv[i + 1]) ?? DEFAULT_LEVEL;
    }
    if (arg?.startsWith("--log-level=")) {
      return asLogLevel(arg.slice("--log-level=".length)) ?? DEFAULT_LEVEL;
    }
  }
  return asLogLevel(env.KOSHELL_LOG) ?? DEFAULT_LEVEL;
}

// Creates a logger that writes level-tagged lines to the sink, dropping messages
// below the configured level.
export function createLogger(
  level: LogLevel,
  sink: (line: string) => void,
): Logger {
  const threshold = LEVEL_RANK[level];
  const emit = (messageLevel: Exclude<LogLevel, "off">, message: string) => {
    if (LEVEL_RANK[messageLevel] <= threshold) {
      sink(`${messageLevel}: ${message}`);
    }
  };
  return {
    error: (message) => {
      emit("error", message);
    },
    warn: (message) => {
      emit("warn", message);
    },
    info: (message) => {
      emit("info", message);
    },
    debug: (message) => {
      emit("debug", message);
    },
  };
}

#!/usr/bin/env bun
// koshell AI daemon entry point.
//
// Runs on Bun (runner and packager); the source uses node: APIs only, so the
// runtime choice stays a packaging decision, not an API dependency. A JSONL
// Unix-socket daemon answers terminal `#?` requests through a persistent
// pi-backed agent conversation per terminal session, streaming the response as
// `ai_delta` messages. Provider/model/auth come from Koshell's own koshell.toml
// (see config.ts / provider.ts); the terminal tool loop arrives in a later stage.
//
// Startup is single-instance (design 0008): the socket file is the lock. Probe
// any existing socket — a live daemon means exit and let the terminal use it; a
// stale file is unlinked before binding. The terminal auto-spawns this process
// on demand, so it exits itself after an idle period with no terminals attached.
import { existsSync, mkdirSync, rmSync } from "node:fs";
import { dirname } from "node:path";
import process from "node:process";

import pkg from "../package.json" with { type: "json" };
import { createPiAgentFactory } from "./agent-runtime.ts";
import { assertSocketPathBindable, probeSocket } from "./lifecycle.ts";
import { createLogger, resolveLogLevel } from "./logging.ts";
import { startDaemon } from "./server.ts";
import { resolveSocketPath } from "./socket-path.ts";

// With no terminals connected the daemon exits after this long, so a rebuilt or
// stale daemon drains itself; the next `#?` respawns one in ~200ms.
const IDLE_TIMEOUT_MS = 10 * 60 * 1000;

// This build's version (design 0024), substituted for the identifier below by
// `scripts/build-binary.ts` (`bun build --define`) while compiling, so the binary carries
// the build it came from instead of reading its runtime environment.
//
// A declared global rather than a `process.env` read: `--define` substitutes bare
// identifiers, and `process` is an explicit `node:process` import here, so a
// `process.env.X` form would silently never be replaced. `typeof` is what keeps the
// unsubstituted case safe — in a source run (`bun src/index.ts`) the identifier does not
// exist at all. That case reports the package version marked `+source`: nothing stamped
// the run, and saying so beats a bare `0.1.0` that reads like a stale binary in
// `koshell version`'s three-way comparison.
declare const KOSHELL_BUILD_VERSION: string | undefined;
const VERSION =
  typeof KOSHELL_BUILD_VERSION === "string"
    ? KOSHELL_BUILD_VERSION
    : `${pkg.version}+source`;

async function main(): Promise<void> {
  const argv = process.argv.slice(2);

  // Answered before anything else touches the socket: `koshell-ai-daemon --version` must
  // never start, adopt, or contend for a daemon just to print a string.
  if (argv.includes("--version")) {
    process.stdout.write(`koshell-ai-daemon ${VERSION}\n`);
    return;
  }

  const socketPath = resolveSocketPath();

  // Level: --log-level argument, then KOSHELL_LOG, then "info".
  const level = resolveLogLevel(argv, process.env);
  const log = createLogger(level, (line) => {
    process.stdout.write(`[koshell-ai-daemon] ${line}\n`);
  });

  try {
    assertSocketPathBindable(socketPath);
  } catch (error) {
    log.error(error instanceof Error ? error.message : String(error));
    process.exit(1);
  }

  mkdirSync(dirname(socketPath), { recursive: true });

  const state = await probeSocket(socketPath);
  if (state === "alive") {
    log.info(`another daemon is already serving ${socketPath}; exiting`);
    process.exit(0);
  }
  if (state === "stale") {
    rmSync(socketPath);
  }

  const server = startDaemon(socketPath, {
    createAgent: createPiAgentFactory(),
    log,
    version: VERSION,
    idleTimeoutMs: IDLE_TIMEOUT_MS,
    onIdle: () => {
      log.info("no terminals connected within the idle window; exiting");
      stop();
    },
  });

  // Removing the socket file is process.exit's responsibility (it does not
  // unlink the listening socket); server.close() stops accepting first.
  function stop(): never {
    server.close();
    if (existsSync(socketPath)) {
      rmSync(socketPath);
    }
    process.exit(0);
  }

  server.on("error", (error: NodeJS.ErrnoException) => {
    if (error.code === "EADDRINUSE") {
      // Lost a bind race. Defer to the winner if it is healthy, else fail loudly.
      void probeSocket(socketPath).then((raced) => {
        if (raced === "alive") {
          log.info(
            `another daemon won the bind race for ${socketPath}; exiting`,
          );
          process.exit(0);
        }
        log.error(`cannot bind ${socketPath}: ${error.message}`);
        process.exit(1);
      });
      return;
    }
    log.error(`daemon socket error: ${error.message}`);
    process.exit(1);
  });

  const shutdown = (): void => {
    stop();
  };
  process.on("SIGINT", shutdown);
  process.on("SIGTERM", shutdown);
}

void main();

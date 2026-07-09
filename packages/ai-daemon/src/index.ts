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

async function main(): Promise<void> {
  const socketPath = resolveSocketPath();

  // Level: --log-level argument, then KOSHELL_LOG, then "info".
  const level = resolveLogLevel(process.argv.slice(2), process.env);
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
    version: pkg.version,
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

#!/usr/bin/env node
// koshell AI daemon entry point.
//
// Current stage: a JSONL Unix-socket daemon that answers terminal `#?` requests
// through a persistent pi-backed agent conversation per terminal session, streaming
// the response back as `ai_delta` messages. Provider/model/auth resolution is pi's
// default chain for now; Koshell-owned provider configuration and the terminal tool
// loop arrive in later stages.
import { existsSync, mkdirSync, rmSync } from "node:fs";
import { dirname } from "node:path";
import process from "node:process";

import { createPiAgentFactory } from "./agent-runtime.ts";
import { createLogger, resolveLogLevel } from "./logging.ts";
import { startDaemon } from "./server.ts";
import { resolveSocketPath } from "./socket-path.ts";

function main(): void {
  const socketPath = resolveSocketPath();
  mkdirSync(dirname(socketPath), { recursive: true });
  if (existsSync(socketPath)) {
    rmSync(socketPath);
  }

  // Level: --log-level argument, then KOSHELL_LOG, then "info".
  const level = resolveLogLevel(process.argv.slice(2), process.env);
  const log = createLogger(level, (line) => {
    process.stdout.write(`[koshell-ai-daemon] ${line}\n`);
  });

  const server = startDaemon(socketPath, {
    createAgent: createPiAgentFactory(),
    log,
  });

  const shutdown = (): void => {
    server.close();
    if (existsSync(socketPath)) {
      rmSync(socketPath);
    }
    process.exit(0);
  };

  process.on("SIGINT", shutdown);
  process.on("SIGTERM", shutdown);
}

main();

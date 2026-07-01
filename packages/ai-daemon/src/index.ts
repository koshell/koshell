#!/usr/bin/env node
// koshell AI daemon entry point.
//
// Current stage: a minimal JSONL Unix-socket receiver that logs terminal `#?` requests
// and acknowledges them. pi-backed AI, provider configuration, and the tool loop arrive
// in the next stage.
import { existsSync, mkdirSync, rmSync } from "node:fs";
import { dirname } from "node:path";
import process from "node:process";

import { startDaemon } from "./server.ts";
import { resolveSocketPath } from "./socket-path.ts";

function main(): void {
  const socketPath = resolveSocketPath();
  mkdirSync(dirname(socketPath), { recursive: true });
  if (existsSync(socketPath)) {
    rmSync(socketPath);
  }

  const log = (message: string): void => {
    process.stdout.write(`[koshell-ai-daemon] ${message}\n`);
  };

  const server = startDaemon(socketPath, log);

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

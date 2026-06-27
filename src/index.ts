#!/usr/bin/env node
import process from "node:process";
import { runInteractiveTerminalShell } from "./terminal-session.ts";

try {
  runInteractiveTerminalShell();
} catch (error: unknown) {
  const message = error instanceof Error ? error.message : String(error);
  process.stderr.write(`koshell failed: ${message}\n`);
  process.exitCode = 1;
}

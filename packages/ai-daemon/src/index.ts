#!/usr/bin/env node
// koshell AI daemon entry point.
//
// Scaffolding stage: the JSONL-over-Unix-socket receiver that accepts terminal
// #? requests lands in Phase 5-min. pi-backed AI arrives the stage after.
import process from "node:process";

function main(): void {
  process.stderr.write(
    "koshell-ai-daemon: scaffolding. IPC receiver lands in Phase 5-min.\n",
  );
}

main();

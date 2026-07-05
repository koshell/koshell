// Protocol-level smoke test for the daemon, run by bun. Spawns the daemon given
// as arguments, waits for its socket, sends a version-mismatched hello and an
// ai_request, and asserts the daemon replies `ack` then an `ai_error` naming the
// version mismatch. This exercises startup, socket binding, JSONL framing, and
// the handshake-rejection path without touching pi, providers, or the network.
//
// Usage: bun scripts/smoke.ts <daemon-cmd> [args...]   (e.g. bun src/index.ts)

import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import process from "node:process";

const argv = process.argv.slice(2);
const [command, ...commandArgs] = argv;
if (command === undefined) {
  console.error("usage: bun scripts/smoke.ts <daemon-cmd> [args...]");
  process.exit(2);
}

const runtimeDir = mkdtempSync(join(tmpdir(), "koshell-smoke-"));
const socketPath = join(runtimeDir, "koshell", "daemon.sock");

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function waitForSocket(deadlineMs: number): Promise<void> {
  const deadline = Date.now() + deadlineMs;
  while (Date.now() < deadline) {
    if (existsSync(socketPath)) {
      return;
    }
    await sleep(50);
  }
  throw new Error(`daemon socket ${socketPath} never appeared`);
}

function connect(): Promise<net.Socket> {
  return new Promise((resolve, reject) => {
    const socket = net.createConnection(socketPath);
    socket.once("connect", () => {
      resolve(socket);
    });
    socket.once("error", reject);
  });
}

function readLines(
  socket: net.Socket,
  count: number,
  timeoutMs: number,
): Promise<Record<string, unknown>[]> {
  return new Promise((resolve, reject) => {
    const out: Record<string, unknown>[] = [];
    let buffer = "";
    const timer = setTimeout(() => {
      reject(
        new Error(
          `timed out waiting for ${String(count)} replies; got ${String(out.length)}`,
        ),
      );
    }, timeoutMs);
    socket.on("data", (chunk: Buffer) => {
      buffer += chunk.toString("utf8");
      let nl = buffer.indexOf("\n");
      while (nl >= 0) {
        const line = buffer.slice(0, nl);
        buffer = buffer.slice(nl + 1);
        if (line.length > 0) {
          out.push(JSON.parse(line) as Record<string, unknown>);
        }
        if (out.length >= count) {
          clearTimeout(timer);
          resolve(out);
          return;
        }
        nl = buffer.indexOf("\n");
      }
    });
    socket.on("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
  });
}

const child = spawn(command, commandArgs, {
  cwd: process.cwd(),
  env: { ...process.env, XDG_RUNTIME_DIR: runtimeDir },
  stdio: ["ignore", "inherit", "inherit"],
});

let exitCode = 1;
try {
  await waitForSocket(5000);
  const socket = await connect();
  const send = (message: unknown): void => {
    socket.write(`${JSON.stringify(message)}\n`);
  };
  send({
    type: "hello",
    protocol_version: 999,
    terminal_session_id: "smoke",
    cwd: "/",
    shell: "bash",
    rows: 24,
    cols: 80,
  });
  send({
    type: "ai_request",
    request_id: "smoke-1",
    question: "ping",
    trigger: "#?",
    context_package: null,
  });
  const [ack, error] = await readLines(socket, 2, 5000);
  socket.destroy();
  const message = error?.message;
  const ok =
    ack?.type === "ack" &&
    error?.type === "ai_error" &&
    typeof message === "string" &&
    message.includes("version mismatch");
  if (ok) {
    console.log(`smoke PASS (${argv.join(" ")})`);
    exitCode = 0;
  } else {
    console.error(
      `smoke FAIL: unexpected replies ${JSON.stringify([ack, error])}`,
    );
  }
} catch (error) {
  console.error(
    `smoke FAIL: ${error instanceof Error ? error.message : String(error)}`,
  );
} finally {
  child.kill("SIGTERM");
  rmSync(runtimeDir, { recursive: true, force: true });
}

process.exit(exitCode);

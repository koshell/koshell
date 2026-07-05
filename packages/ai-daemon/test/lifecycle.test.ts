import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { describe, expect, it } from "bun:test";

import type { AgentFactory } from "../src/agent-runtime.ts";
import { assertSocketPathBindable, probeSocket } from "../src/lifecycle.ts";
import { type Logger } from "../src/logging.ts";
import { startDaemon } from "../src/server.ts";

const noop = (): void => undefined;
const NOOP_LOGGER: Logger = {
  error: noop,
  warn: noop,
  info: noop,
  debug: noop,
};
const THROWING_FACTORY: AgentFactory = () => {
  throw new Error("lifecycle tests never create an agent");
};

function tempSocketPath(): { dir: string; socketPath: string } {
  const dir = mkdtempSync(join(tmpdir(), "koshell-life-"));
  return { dir, socketPath: join(dir, "daemon.sock") };
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function listening(server: net.Server): Promise<void> {
  return new Promise((resolve) => {
    if (server.listening) {
      resolve();
      return;
    }
    server.once("listening", () => {
      resolve();
    });
  });
}

function connect(socketPath: string): Promise<net.Socket> {
  return new Promise((resolve, reject) => {
    const socket = net.createConnection(socketPath);
    socket.once("connect", () => {
      resolve(socket);
    });
    socket.once("error", reject);
  });
}

describe("probeSocket", () => {
  it("reports absent when no file exists", async () => {
    const { dir, socketPath } = tempSocketPath();
    try {
      expect(await probeSocket(socketPath)).toBe("absent");
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  it("reports stale for a non-socket file", async () => {
    const { dir, socketPath } = tempSocketPath();
    try {
      writeFileSync(socketPath, "not a socket");
      expect(await probeSocket(socketPath)).toBe("stale");
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  it("reports alive for a listening daemon", async () => {
    const { dir, socketPath } = tempSocketPath();
    const server = startDaemon(socketPath, {
      createAgent: THROWING_FACTORY,
      log: NOOP_LOGGER,
      version: "0.0.0",
    });
    try {
      await listening(server);
      expect(await probeSocket(socketPath)).toBe("alive");
    } finally {
      server.close();
      rmSync(dir, { recursive: true, force: true });
    }
  });
});

describe("idle exit", () => {
  it("fires after the timeout when no terminal connects", async () => {
    const { dir, socketPath } = tempSocketPath();
    let idle = 0;
    const server = startDaemon(socketPath, {
      createAgent: THROWING_FACTORY,
      log: NOOP_LOGGER,
      version: "0.0.0",
      idleTimeoutMs: 60,
      onIdle: () => {
        idle += 1;
      },
    });
    try {
      await listening(server);
      await sleep(160);
      expect(idle).toBeGreaterThanOrEqual(1);
    } finally {
      server.close();
      rmSync(dir, { recursive: true, force: true });
    }
  });

  it("is suppressed while a terminal is connected and re-arms after it leaves", async () => {
    const { dir, socketPath } = tempSocketPath();
    let idle = 0;
    // The timer is armed at listen time; the margin here is comfortably wider
    // than the client-connect round-trip so the cancel wins deterministically.
    const server = startDaemon(socketPath, {
      createAgent: THROWING_FACTORY,
      log: NOOP_LOGGER,
      version: "0.0.0",
      idleTimeoutMs: 300,
      onIdle: () => {
        idle += 1;
      },
    });
    try {
      await listening(server);
      const socket = await connect(socketPath);
      // Held open across more than one idle window: it must not fire.
      await sleep(420);
      expect(idle).toBe(0);
      // Disconnecting re-arms the timer, which then fires.
      socket.destroy();
      await sleep(420);
      expect(idle).toBeGreaterThanOrEqual(1);
    } finally {
      server.close();
      rmSync(dir, { recursive: true, force: true });
    }
  });
});

describe("assertSocketPathBindable", () => {
  it("throws for a path over the AF_UNIX limit", () => {
    const longPath = `/tmp/${"x".repeat(110)}.sock`;
    expect(() => {
      assertSocketPathBindable(longPath);
    }).toThrow();
  });

  it("accepts a normal path", () => {
    expect(() => {
      assertSocketPathBindable("/run/user/1000/koshell/daemon.sock");
    }).not.toThrow();
  });
});

// Daemon startup helpers: the single-instance socket probe and the socket-path
// length guard. Kept free of pi and server state so the startup state machine in
// index.ts is unit-testable in isolation. node: APIs only — Bun runs this code,
// it is not a Bun API surface.

import { statSync } from "node:fs";
import net from "node:net";

// The portable AF_UNIX `sun_path` ceiling is 104 bytes on macOS/BSD (108 on
// Linux); 103 leaves room for the NUL terminator on the smaller platform. Bun on
// macOS silently "listens" on an overlong path without ever becoming
// connectable, so the daemon rejects such a path loudly instead of coming up dead.
const MAX_SOCKET_PATH_BYTES = 103;

// Throws when the socket path is too long to bind portably.
export function assertSocketPathBindable(path: string): void {
  const bytes = Buffer.byteLength(path, "utf8");
  if (bytes > MAX_SOCKET_PATH_BYTES) {
    throw new Error(
      `socket path is ${String(bytes)} bytes, over the ` +
        `${String(MAX_SOCKET_PATH_BYTES)}-byte AF_UNIX limit: ${path}`,
    );
  }
}

export type SocketState = "alive" | "stale" | "absent";

// Determines whether an existing socket file has a daemon behind it. No file →
// "absent"; a leftover non-socket file → "stale" without a connect attempt; a
// socket file that accepts a connection → "alive"; a socket file that refuses or
// times out → "stale". The non-socket short-circuit matters: connecting to a
// regular file raises an error the Bun test runner treats as a test failure, so
// the type check keeps the unit test (and production) off that path.
export function probeSocket(
  path: string,
  timeoutMs = 500,
): Promise<SocketState> {
  let isSocketFile: boolean;
  try {
    isSocketFile = statSync(path).isSocket();
  } catch {
    return Promise.resolve<SocketState>("absent");
  }
  if (!isSocketFile) {
    return Promise.resolve<SocketState>("stale");
  }
  return new Promise<SocketState>((resolve) => {
    const socket = net.createConnection(path);
    let settled = false;
    const finish = (state: SocketState): void => {
      if (settled) {
        return;
      }
      settled = true;
      socket.destroy();
      resolve(state);
    };
    socket.setTimeout(timeoutMs);
    socket.on("connect", () => {
      finish("alive");
    });
    socket.on("timeout", () => {
      finish("stale");
    });
    socket.on("error", () => {
      finish("stale");
    });
  });
}

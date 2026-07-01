import net from "node:net";

import { NdjsonDecoder } from "./framing.ts";
import {
  type ClientMessage,
  parseClientMessage,
  respondTo,
} from "./protocol.ts";

export type Logger = (message: string) => void;

// Logs a parsed client message and returns the JSONL reply line, if any.
export function handleMessage(
  message: ClientMessage,
  log: Logger,
): string | null {
  switch (message.type) {
    case "hello":
      log(
        `hello from ${message.terminal_session_id} (${message.shell}, ${String(message.cols)}x${String(message.rows)}) cwd=${message.cwd}`,
      );
      return null;
    case "ai_request": {
      log(`#? [${message.request_id}] ${message.question}`);
      log(`context_package: ${JSON.stringify(message.context_package)}`);
      const reply = respondTo(message);
      return reply === null ? null : `${JSON.stringify(reply)}\n`;
    }
    case "bye":
      log(`bye from ${message.terminal_session_id}`);
      return null;
  }
}

// Starts the JSONL Unix-socket daemon on the given path.
export function startDaemon(socketPath: string, log: Logger): net.Server {
  const server = net.createServer((socket) => {
    const decoder = new NdjsonDecoder();
    log("terminal connected");

    socket.on("data", (chunk: Buffer) => {
      for (const line of decoder.push(chunk.toString("utf8"))) {
        const message = parseClientMessage(line);
        if (message === null) {
          log(`ignored invalid message: ${line}`);
          continue;
        }
        const reply = handleMessage(message, log);
        if (reply !== null) {
          socket.write(reply);
        }
      }
    });

    socket.on("error", () => {
      // A terminal disconnecting mid-write is expected; ignore.
    });
    socket.on("close", () => {
      log("terminal disconnected");
    });
  });

  server.listen(socketPath, () => {
    log(`listening on ${socketPath}`);
  });

  return server;
}

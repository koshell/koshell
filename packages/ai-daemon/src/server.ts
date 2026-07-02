import net from "node:net";
import process from "node:process";

import type { AgentFactory, KoshellAgent } from "./agent-runtime.ts";
import { NdjsonDecoder } from "./framing.ts";
import type { Logger } from "./logging.ts";
import { buildUserPrompt } from "./prompt.ts";
import {
  type AiRequestMessage,
  type HelloMessage,
  type ServerMessage,
  parseClientMessage,
  serializeServerMessage,
} from "./protocol.ts";

export interface DaemonOptions {
  createAgent: AgentFactory;
  log: Logger;
}

// Where a connection's server messages are written. Abstracted from net.Socket so
// tests can drive a TerminalConnection with an array-collecting sink.
export interface MessageSink {
  write(line: string): void;
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

// One terminal connection: holds the hello metadata and one lazy persistent agent
// conversation, and serializes ai_requests FIFO (pi forbids concurrent prompts on
// one session). The conversation is discarded when the terminal disconnects; a
// reconnecting terminal gets a fresh conversation.
export class TerminalConnection {
  private readonly sink: MessageSink;
  private readonly options: DaemonOptions;
  private hello: HelloMessage | undefined;
  private agent: Promise<KoshellAgent> | undefined;
  private queue: Promise<void> = Promise.resolve();
  private closed = false;

  constructor(sink: MessageSink, options: DaemonOptions) {
    this.sink = sink;
    this.options = options;
  }

  // Parses and dispatches one JSONL line from the terminal.
  handleLine(line: string): void {
    const message = parseClientMessage(line);
    if (message === null) {
      this.options.log.warn(`ignored invalid message: ${line}`);
      return;
    }
    switch (message.type) {
      case "hello":
        this.hello = message;
        this.options.log.info(
          `hello from ${message.terminal_session_id} (${message.shell}, ${String(message.cols)}x${String(message.rows)}) cwd=${message.cwd}`,
        );
        break;
      case "ai_request":
        this.options.log.info(`#? [${message.request_id}] ${message.question}`);
        this.options.log.debug(
          `#? [${message.request_id}] context_package: ${JSON.stringify(message.context_package)}`,
        );
        this.send({ type: "ack", request_id: message.request_id });
        this.queue = this.queue.then(() => this.run(message));
        break;
      case "bye":
        this.options.log.info(`bye from ${message.terminal_session_id}`);
        this.dispose();
        break;
    }
  }

  // Idempotent; called on bye and on socket close. An in-flight request keeps
  // running but its sends become no-ops and its errors are swallowed by run().
  dispose(): void {
    if (this.closed) {
      return;
    }
    this.closed = true;
    void this.agent
      ?.then((agent) => {
        agent.dispose();
      })
      .catch(() => undefined);
    this.agent = undefined;
  }

  // Never rejects: exactly one of ai_response_end or ai_error per request.
  private async run(message: AiRequestMessage): Promise<void> {
    try {
      const agent = await this.getAgent();
      await agent.ask({
        prompt: buildUserPrompt(message, this.hello),
        onDelta: (delta) => {
          this.send({
            type: "ai_delta",
            request_id: message.request_id,
            delta,
          });
        },
      });
      this.send({ type: "ai_response_end", request_id: message.request_id });
    } catch (error) {
      this.options.log.error(
        `#? [${message.request_id}] failed: ${errorText(error)}`,
      );
      this.send({
        type: "ai_error",
        request_id: message.request_id,
        message: errorText(error),
      });
    }
  }

  private getAgent(): Promise<KoshellAgent> {
    this.agent ??= this.options
      .createAgent({
        cwd: this.hello?.cwd ?? process.cwd(),
        log: this.options.log,
      })
      .catch((error: unknown) => {
        // A failed creation must not poison the connection: clear the memoized
        // promise so the next request retries (the user may have configured a
        // provider key in the meantime).
        this.agent = undefined;
        throw error;
      });
    return this.agent;
  }

  private send(message: ServerMessage): void {
    if (this.closed) {
      return;
    }
    this.sink.write(serializeServerMessage(message));
  }
}

// Starts the JSONL Unix-socket daemon on the given path.
export function startDaemon(
  socketPath: string,
  options: DaemonOptions,
): net.Server {
  const server = net.createServer((socket) => {
    const decoder = new NdjsonDecoder();
    options.log.info("terminal connected");

    const sink: MessageSink = {
      write(line: string): void {
        if (!socket.destroyed && !socket.writableEnded) {
          socket.write(line);
        }
      },
    };
    const connection = new TerminalConnection(sink, options);

    socket.on("data", (chunk: Buffer) => {
      for (const line of decoder.push(chunk.toString("utf8"))) {
        connection.handleLine(line);
      }
    });

    socket.on("error", () => {
      // A terminal disconnecting mid-write is expected; ignore.
    });
    socket.on("close", () => {
      connection.dispose();
      options.log.info("terminal disconnected");
    });
  });

  server.listen(socketPath, () => {
    options.log.info(`listening on ${socketPath}`);
  });

  return server;
}

import net from "node:net";
import process from "node:process";

import type { AgentFactory, KoshellAgent } from "./agent-runtime.ts";
import { NdjsonDecoder } from "./framing.ts";
import type { Logger } from "./logging.ts";
import { buildUserPrompt } from "./prompt.ts";
import {
  type AiRequestMessage,
  type HelloMessage,
  PROTOCOL_VERSION,
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
//
// The hello handshake is enforced: ai_requests are only served after a hello whose
// protocol_version matches this daemon's. Anything else (no hello yet, or a
// mismatched version) is answered with an explicit ai_error so the terminal shows
// the reason inline instead of failing on a message-shape mismatch later.
export class TerminalConnection {
  private readonly sink: MessageSink;
  private readonly options: DaemonOptions;
  private hello: HelloMessage | undefined;
  // Why ai_requests cannot be served yet; cleared by a version-matching hello.
  private helloRejection: string | undefined =
    "the terminal did not complete the hello handshake on this connection";
  private agent: Promise<KoshellAgent> | undefined;
  private queue: Promise<void> = Promise.resolve();
  // Requests withdrawn by ai_cancel; consumed when run() reaches them (queued
  // requests are skipped without prompting) and cleared when the request ends,
  // so a cancel that raced past its request's completion cannot linger.
  private readonly cancelled = new Set<string>();
  private runningRequestId: string | undefined;
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
        if (message.protocol_version !== PROTOCOL_VERSION) {
          this.hello = undefined;
          this.helloRejection =
            `protocol version mismatch: the terminal speaks v${String(message.protocol_version)}, ` +
            `this daemon speaks v${String(PROTOCOL_VERSION)}. Upgrade the older side, ` +
            `restart the daemon, and reopen this terminal window.`;
          this.options.log.warn(
            `rejected hello from ${message.terminal_session_id}: ${this.helloRejection}`,
          );
          break;
        }
        this.hello = message;
        this.helloRejection = undefined;
        this.options.log.info(
          `hello from ${message.terminal_session_id} (${message.shell}, ${String(message.cols)}x${String(message.rows)}) cwd=${message.cwd}`,
        );
        break;
      case "ai_request":
        if (this.helloRejection !== undefined) {
          // Keep the per-request contract (ack, then exactly one terminal
          // marker) so the terminal's pending-request handling stays uniform.
          this.options.log.warn(
            `refused #? [${message.request_id}]: ${this.helloRejection}`,
          );
          this.send({ type: "ack", request_id: message.request_id });
          this.send({
            type: "ai_error",
            request_id: message.request_id,
            message: this.helloRejection,
          });
          break;
        }
        this.options.log.info(`#? [${message.request_id}] ${message.question}`);
        this.options.log.debug(
          `#? [${message.request_id}] context_package: ${JSON.stringify(message.context_package)}`,
        );
        this.send({ type: "ack", request_id: message.request_id });
        this.queue = this.queue.then(() => this.run(message));
        break;
      case "ai_cancel":
        // Best-effort: the terminal already stopped rendering and suppresses
        // late messages, so this only stops generation and unblocks the queue.
        this.options.log.info(
          `#? [${message.request_id}] cancelled by the terminal`,
        );
        this.cancelled.add(message.request_id);
        if (this.runningRequestId === message.request_id) {
          void this.agent
            ?.then((agent) => {
              agent.abort();
            })
            .catch(() => undefined);
        }
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
  // A cancelled request keeps that contract: skipped before prompting when the
  // cancel arrived while it was queued (or while the agent was being created),
  // and ended normally when abort() cut the prompt short mid-generation.
  private async run(message: AiRequestMessage): Promise<void> {
    if (this.cancelled.has(message.request_id)) {
      this.options.log.info(`#? [${message.request_id}] skipped (cancelled)`);
      this.cancelled.delete(message.request_id);
      this.send({ type: "ai_response_end", request_id: message.request_id });
      return;
    }
    this.runningRequestId = message.request_id;
    try {
      const agent = await this.getAgent();
      if (this.cancelled.has(message.request_id)) {
        this.options.log.info(`#? [${message.request_id}] skipped (cancelled)`);
        this.send({ type: "ai_response_end", request_id: message.request_id });
        return;
      }
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
    } finally {
      this.cancelled.delete(message.request_id);
      this.runningRequestId = undefined;
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

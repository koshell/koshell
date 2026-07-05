import { describe, expect, it } from "bun:test";

import type {
  AgentFactory,
  AskOptions,
  KoshellAgent,
} from "../src/agent-runtime.ts";
import type { Logger } from "../src/logging.ts";
import { PROTOCOL_VERSION } from "../src/protocol.ts";
import { TerminalConnection, type MessageSink } from "../src/server.ts";

const noop = (): void => undefined;

const NOOP_LOGGER: Logger = {
  error: noop,
  warn: noop,
  info: noop,
  debug: noop,
};

const HELLO_LINE = helloLine(PROTOCOL_VERSION);

function helloLine(protocolVersion: number): string {
  return JSON.stringify({
    type: "hello",
    protocol_version: protocolVersion,
    terminal_session_id: "koshell-42",
    cwd: "/tmp",
    shell: "/bin/zsh",
    rows: 24,
    cols: 80,
  });
}

function aiRequestLine(requestId: string, question = "why"): string {
  return JSON.stringify({
    type: "ai_request",
    request_id: requestId,
    question,
    trigger: "#?",
    context_package: null,
  });
}

function collectingSink(): { sink: MessageSink; lines: string[] } {
  const lines: string[] = [];
  return {
    sink: {
      write(line: string): void {
        lines.push(line.trimEnd());
      },
    },
    lines,
  };
}

function types(lines: string[]): string[] {
  return lines.map((line) => (JSON.parse(line) as { type: string }).type);
}

// Resolves once all queued microtasks and the sync continuations behind them ran.
async function settle(): Promise<void> {
  for (let i = 0; i < 20; i += 1) {
    await Promise.resolve();
  }
}

describe("TerminalConnection", () => {
  it("streams ack, deltas, and ai_response_end in order", async () => {
    const { sink, lines } = collectingSink();
    const factory: AgentFactory = () =>
      Promise.resolve<KoshellAgent>({
        ask({ onDelta }: AskOptions): Promise<void> {
          onDelta("Hello ");
          onDelta("world");
          return Promise.resolve();
        },
        abort: noop,
        dispose: noop,
      });
    const connection = new TerminalConnection(sink, {
      createAgent: factory,
      log: NOOP_LOGGER,
    });

    connection.handleLine(HELLO_LINE);
    connection.handleLine(aiRequestLine("r1"));
    await settle();

    expect(types(lines)).toEqual([
      "ack",
      "ai_delta",
      "ai_delta",
      "ai_response_end",
    ]);
    expect(lines[1]).toContain('"delta":"Hello "');
  });

  it("reports a factory failure as ai_error and retries on the next request", async () => {
    const { sink, lines } = collectingSink();
    let calls = 0;
    const factory: AgentFactory = () => {
      calls += 1;
      if (calls === 1) {
        return Promise.reject(new Error("no AI provider configured"));
      }
      return Promise.resolve<KoshellAgent>({
        ask({ onDelta }: AskOptions): Promise<void> {
          onDelta("ok");
          return Promise.resolve();
        },
        abort: noop,
        dispose: noop,
      });
    };
    const connection = new TerminalConnection(sink, {
      createAgent: factory,
      log: NOOP_LOGGER,
    });

    connection.handleLine(HELLO_LINE);
    connection.handleLine(aiRequestLine("r1"));
    await settle();
    connection.handleLine(aiRequestLine("r2"));
    await settle();

    expect(types(lines)).toEqual([
      "ack",
      "ai_error",
      "ack",
      "ai_delta",
      "ai_response_end",
    ]);
    expect(calls).toBe(2);
    expect(lines[1]).toContain("no AI provider configured");
  });

  it("reports an ask failure after partial deltas as ai_error", async () => {
    const { sink, lines } = collectingSink();
    const factory: AgentFactory = () =>
      Promise.resolve<KoshellAgent>({
        ask({ onDelta }: AskOptions): Promise<void> {
          onDelta("partial");
          return Promise.reject(new Error("provider exploded"));
        },
        abort: noop,
        dispose: noop,
      });
    const connection = new TerminalConnection(sink, {
      createAgent: factory,
      log: NOOP_LOGGER,
    });

    connection.handleLine(HELLO_LINE);
    connection.handleLine(aiRequestLine("r1"));
    await settle();

    expect(types(lines)).toEqual(["ack", "ai_delta", "ai_error"]);
    expect(lines[2]).toContain("provider exploded");
  });

  it("serializes concurrent requests FIFO on one conversation", async () => {
    const { sink, lines } = collectingSink();
    let releaseFirst: (() => void) | undefined;
    let asks = 0;
    const factory: AgentFactory = () =>
      Promise.resolve<KoshellAgent>({
        ask({ onDelta }: AskOptions): Promise<void> {
          asks += 1;
          const id = asks;
          onDelta(`answer-${String(id)}`);
          if (id === 1) {
            return new Promise((resolve) => {
              releaseFirst = resolve;
            });
          }
          return Promise.resolve();
        },
        abort: noop,
        dispose: noop,
      });
    const connection = new TerminalConnection(sink, {
      createAgent: factory,
      log: NOOP_LOGGER,
    });

    connection.handleLine(HELLO_LINE);
    connection.handleLine(aiRequestLine("r1"));
    connection.handleLine(aiRequestLine("r2"));
    await settle();

    // Both acks are immediate, but request 2 must not start streaming while
    // request 1 is still in flight.
    expect(types(lines)).toEqual(["ack", "ack", "ai_delta"]);
    expect(lines[2]).toContain("answer-1");

    releaseFirst?.();
    await settle();

    expect(types(lines)).toEqual([
      "ack",
      "ack",
      "ai_delta",
      "ai_response_end",
      "ai_delta",
      "ai_response_end",
    ]);
    expect(lines[4]).toContain("answer-2");
  });

  it("aborts the running request on ai_cancel and still ends it", async () => {
    const { sink, lines } = collectingSink();
    let aborts = 0;
    let finishAsk: (() => void) | undefined;
    const factory: AgentFactory = () =>
      Promise.resolve<KoshellAgent>({
        ask({ onDelta }: AskOptions): Promise<void> {
          onDelta("partial");
          // Like pi: abort() makes the in-flight prompt resolve early.
          return new Promise((resolve) => {
            finishAsk = resolve;
          });
        },
        abort(): void {
          aborts += 1;
          finishAsk?.();
        },
        dispose: noop,
      });
    const connection = new TerminalConnection(sink, {
      createAgent: factory,
      log: NOOP_LOGGER,
    });

    connection.handleLine(HELLO_LINE);
    connection.handleLine(aiRequestLine("r1"));
    await settle();
    connection.handleLine(
      JSON.stringify({ type: "ai_cancel", request_id: "r1" }),
    );
    await settle();

    expect(aborts).toBe(1);
    expect(types(lines)).toEqual(["ack", "ai_delta", "ai_response_end"]);
  });

  it("skips a queued request cancelled before it starts", async () => {
    const { sink, lines } = collectingSink();
    let releaseFirst: (() => void) | undefined;
    let asks = 0;
    const factory: AgentFactory = () =>
      Promise.resolve<KoshellAgent>({
        ask({ onDelta }: AskOptions): Promise<void> {
          asks += 1;
          onDelta(`answer-${String(asks)}`);
          if (asks === 1) {
            return new Promise((resolve) => {
              releaseFirst = resolve;
            });
          }
          return Promise.resolve();
        },
        abort: noop,
        dispose: noop,
      });
    const connection = new TerminalConnection(sink, {
      createAgent: factory,
      log: NOOP_LOGGER,
    });

    connection.handleLine(HELLO_LINE);
    connection.handleLine(aiRequestLine("r1"));
    connection.handleLine(aiRequestLine("r2"));
    await settle();
    // r2 sits behind r1 in the FIFO queue; cancelling it must skip its prompt
    // entirely when its turn comes, while still ending it for the terminal.
    connection.handleLine(
      JSON.stringify({ type: "ai_cancel", request_id: "r2" }),
    );
    releaseFirst?.();
    await settle();

    expect(asks).toBe(1);
    expect(types(lines)).toEqual([
      "ack",
      "ack",
      "ai_delta",
      "ai_response_end",
      "ai_response_end",
    ]);
  });

  it("disposes the agent on bye and drops late deltas", async () => {
    const { sink, lines } = collectingSink();
    let disposed = 0;
    let emitLate: (() => void) | undefined;
    const factory: AgentFactory = () =>
      Promise.resolve<KoshellAgent>({
        ask({ onDelta }: AskOptions): Promise<void> {
          onDelta("early");
          return new Promise(() => {
            emitLate = () => {
              onDelta("late");
            };
          });
        },
        abort: noop,
        dispose(): void {
          disposed += 1;
        },
      });
    const connection = new TerminalConnection(sink, {
      createAgent: factory,
      log: NOOP_LOGGER,
    });

    connection.handleLine(HELLO_LINE);
    connection.handleLine(aiRequestLine("r1"));
    await settle();
    connection.handleLine(
      JSON.stringify({ type: "bye", terminal_session_id: "koshell-42" }),
    );
    await settle();
    emitLate?.();
    connection.dispose();
    await settle();

    expect(types(lines)).toEqual(["ack", "ai_delta"]);
    expect(disposed).toBe(1);
  });

  it("refuses an ai_request that arrives before hello", async () => {
    const { sink, lines } = collectingSink();
    const factory: AgentFactory = () => {
      throw new Error("the agent must not be created without a handshake");
    };
    const connection = new TerminalConnection(sink, {
      createAgent: factory,
      log: NOOP_LOGGER,
    });

    connection.handleLine(aiRequestLine("r1"));
    await settle();

    expect(types(lines)).toEqual(["ack", "ai_error"]);
    expect(lines[1]).toContain("hello handshake");
  });

  it("refuses ai_requests after a protocol version mismatch, until a matching hello", async () => {
    const { sink, lines } = collectingSink();
    const factory: AgentFactory = () =>
      Promise.resolve<KoshellAgent>({
        ask({ onDelta }: AskOptions): Promise<void> {
          onDelta("ok");
          return Promise.resolve();
        },
        abort: noop,
        dispose: noop,
      });
    const connection = new TerminalConnection(sink, {
      createAgent: factory,
      log: NOOP_LOGGER,
    });

    connection.handleLine(helloLine(PROTOCOL_VERSION + 1));
    connection.handleLine(aiRequestLine("r1"));
    await settle();

    expect(types(lines)).toEqual(["ack", "ai_error"]);
    expect(lines[1]).toContain(`v${String(PROTOCOL_VERSION + 1)}`);
    expect(lines[1]).toContain(`v${String(PROTOCOL_VERSION)}`);

    // A matching hello on the same connection recovers it (e.g. a corrected
    // client retrying its handshake).
    connection.handleLine(HELLO_LINE);
    connection.handleLine(aiRequestLine("r2"));
    await settle();

    expect(types(lines)).toEqual([
      "ack",
      "ai_error",
      "ack",
      "ai_delta",
      "ai_response_end",
    ]);
  });

  it("answers status_request without a hello handshake", async () => {
    const { sink, lines } = collectingSink();
    const factory: AgentFactory = () => {
      throw new Error("status must not create an agent");
    };
    const connection = new TerminalConnection(sink, {
      createAgent: factory,
      log: NOOP_LOGGER,
      status: () => ({
        pid: 4321,
        version: "9.9.9",
        protocol_version: PROTOCOL_VERSION,
        uptime_ms: 1234,
        connections: 2,
      }),
    });

    connection.handleLine(JSON.stringify({ type: "status_request" }));
    await settle();

    expect(types(lines)).toEqual(["status"]);
    const status = JSON.parse(lines[0] ?? "{}") as Record<string, unknown>;
    expect(status.pid).toBe(4321);
    expect(status.version).toBe("9.9.9");
    expect(status.protocol_version).toBe(PROTOCOL_VERSION);
    expect(status.connections).toBe(2);
  });
});

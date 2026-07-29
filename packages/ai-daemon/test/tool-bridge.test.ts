import { describe, expect, it } from "bun:test";

import { createLogger, type Logger } from "../src/logging.ts";
import {
  CALL_TIMEOUT_MS,
  MAX_CALLS_PER_REQUEST,
  ToolBridge,
  type ToolOutcome,
} from "../src/tool-bridge.ts";

interface Sent {
  requestId: string;
  toolCallId: string;
  toolName: string;
  args: unknown;
}

function harness(): {
  bridge: ToolBridge;
  sent: Sent[];
  lines: string[];
  log: Logger;
} {
  const sent: Sent[] = [];
  const lines: string[] = [];
  const log = createLogger("debug", (line) => lines.push(line));
  const bridge = new ToolBridge({
    send: (call) => sent.push(call),
    log,
  });
  return { bridge, sent, lines, log };
}

// The id of the nth call the bridge sent. Throws rather than asserting non-null so a
// missing send fails with a readable message instead of a type escape hatch.
function callId(sent: Sent[], index = 0): string {
  const call = sent[index];
  if (call === undefined) {
    throw new Error(`no tool call was sent at index ${String(index)}`);
  }
  return call.toolCallId;
}

function expectFailure(outcome: ToolOutcome, code: string): void {
  if (outcome.ok) {
    throw new Error(
      `expected failure "${code}", got result ${JSON.stringify(outcome.result)}`,
    );
  }
  expect(outcome.error.code).toBe(code);
}

describe("ToolBridge", () => {
  it("resolves a call when the terminal answers it", async () => {
    const { bridge, sent } = harness();
    bridge.beginRequest("request-1");

    const pending = bridge.call("list_recent_commands", {});
    expect(sent).toHaveLength(1);
    expect(sent[0]?.requestId).toBe("request-1");
    expect(sent[0]?.toolName).toBe("list_recent_commands");

    bridge.settle("request-1", callId(sent), {
      ok: true,
      result: { commands: [] },
    });
    expect(await pending).toEqual({ ok: true, result: { commands: [] } });
  });

  it("passes a structured terminal failure through", async () => {
    const { bridge, sent } = harness();
    bridge.beginRequest("request-1");
    const pending = bridge.call("read_command_output", { commandId: "x" });
    bridge.settle("request-1", callId(sent), {
      ok: false,
      error: { code: "command_not_found", message: "no such command" },
    });

    expectFailure(await pending, "command_not_found");
  });

  // Exactly one response settles a call. A duplicate must not re-resolve a promise
  // the agent has already moved past.
  it("drops a duplicate response instead of re-settling", async () => {
    const { bridge, sent, lines } = harness();
    bridge.beginRequest("request-1");
    const pending = bridge.call("list_recent_commands", {});

    bridge.settle("request-1", callId(sent), { ok: true, result: 1 });
    bridge.settle("request-1", callId(sent), { ok: true, result: 2 });

    expect(await pending).toEqual({ ok: true, result: 1 });
    expect(lines.join("\n")).toContain("already-settled");
  });

  it("drops a response naming an unknown call", () => {
    const { bridge, lines } = harness();
    bridge.beginRequest("request-1");
    bridge.settle("request-1", "tool-999", { ok: true, result: null });
    expect(lines.join("\n")).toContain("unknown or already-settled");
  });

  // A response tagged with a stale request is by construction from a turn that ended.
  it("drops a response for a request that is no longer active", async () => {
    const { bridge, sent, lines } = harness();
    bridge.beginRequest("request-1");
    const pending = bridge.call("list_recent_commands", {});

    bridge.settle("request-2", callId(sent), {
      ok: true,
      result: "wrong turn",
    });
    expect(lines.join("\n")).toContain("is not active");

    // The real answer still settles it.
    bridge.settle("request-1", callId(sent), {
      ok: true,
      result: "right turn",
    });
    expect(await pending).toEqual({ ok: true, result: "right turn" });
  });

  it(
    "times out a call the terminal never answers",
    async () => {
      const { bridge } = harness();
      bridge.beginRequest("request-1");
      const started = Date.now();
      expectFailure(await bridge.call("list_recent_commands", {}), "timeout");
      expect(Date.now() - started).toBeGreaterThanOrEqual(
        CALL_TIMEOUT_MS - 100,
      );
    },
    CALL_TIMEOUT_MS + 5_000,
  );

  it("refuses a call outside any active request", async () => {
    const { bridge, sent } = harness();
    expectFailure(
      await bridge.call("list_recent_commands", {}),
      "request_inactive",
    );
    expect(sent).toHaveLength(0);
  });

  // Ending the request settles what is still outstanding: an unsettled call would
  // stall the agent turn, and a late answer has nowhere to go.
  it("rejects outstanding calls when the request ends", async () => {
    const { bridge } = harness();
    bridge.beginRequest("request-1");
    const pending = bridge.call("list_recent_commands", {});
    bridge.endRequest();
    expectFailure(await pending, "request_inactive");
  });

  it("rejects outstanding calls when the connection is disposed", async () => {
    const { bridge } = harness();
    bridge.beginRequest("request-1");
    const pending = bridge.call("list_recent_commands", {});
    bridge.dispose();

    expectFailure(await pending, "terminal_disconnected");
    // And later calls do not even go out.
    expectFailure(
      await bridge.call("list_recent_commands", {}),
      "terminal_disconnected",
    );
  });

  it("settles a call when the caller aborts", async () => {
    const { bridge } = harness();
    bridge.beginRequest("request-1");
    const controller = new AbortController();
    const pending = bridge.call("list_recent_commands", {}, controller.signal);
    controller.abort();
    expectFailure(await pending, "request_inactive");
  });

  it("refuses immediately when the signal is already aborted", async () => {
    const { bridge, sent } = harness();
    bridge.beginRequest("request-1");
    const controller = new AbortController();
    controller.abort();
    expectFailure(
      await bridge.call("list_recent_commands", {}, controller.signal),
      "request_inactive",
    );
    expect(sent).toHaveLength(0);
  });

  // One runaway agent turn must not page the whole retained megabyte into context.
  it("caps the calls one request may make", async () => {
    const { bridge, sent } = harness();
    bridge.beginRequest("request-1");
    for (let index = 0; index < MAX_CALLS_PER_REQUEST; index += 1) {
      const pending = bridge.call("list_recent_commands", {});
      bridge.settle("request-1", callId(sent, index), {
        ok: true,
        result: {},
      });
      await pending;
    }

    expectFailure(
      await bridge.call("list_recent_commands", {}),
      "budget_exceeded",
    );
    expect(sent).toHaveLength(MAX_CALLS_PER_REQUEST);
  });

  it("caps the serialized result characters one request may read", async () => {
    const { bridge, sent } = harness();
    bridge.beginRequest("request-1");

    const first = bridge.call("read_command_output", {});
    bridge.settle("request-1", callId(sent), {
      ok: true,
      result: "x".repeat(300_000),
    });
    await first;

    expectFailure(
      await bridge.call("read_command_output", {}),
      "budget_exceeded",
    );
  });

  it("resets the budgets for the next request", async () => {
    const { bridge, sent } = harness();
    bridge.beginRequest("request-1");
    const first = bridge.call("read_command_output", {});
    bridge.settle("request-1", callId(sent), {
      ok: true,
      result: "y".repeat(300_000),
    });
    await first;
    bridge.endRequest();

    bridge.beginRequest("request-2");
    const next = bridge.call("read_command_output", {});
    expect(sent).toHaveLength(2);
    bridge.settle("request-2", callId(sent, 1), { ok: true, result: "ok" });
    expect(await next).toEqual({ ok: true, result: "ok" });
  });
});

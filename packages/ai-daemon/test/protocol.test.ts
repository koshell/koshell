import { describe, expect, it } from "vitest";

import { handleMessage } from "../src/server.ts";
import { parseClientMessage, respondTo } from "../src/protocol.ts";

describe("parseClientMessage", () => {
  it("parses an ai_request with its context package", () => {
    const line = JSON.stringify({
      type: "ai_request",
      request_id: "koshell-req-1",
      question: "explain this output",
      trigger: "#?",
      context_package: { contractVersion: "koshell_ai_context_v1" },
    });
    const message = parseClientMessage(line);
    expect(message).toEqual({
      type: "ai_request",
      request_id: "koshell-req-1",
      question: "explain this output",
      trigger: "#?",
      context_package: { contractVersion: "koshell_ai_context_v1" },
    });
  });

  it("parses a hello handshake", () => {
    const line = JSON.stringify({
      type: "hello",
      protocol_version: 1,
      terminal_session_id: "koshell-42",
      cwd: "/tmp",
      shell: "/bin/zsh",
      rows: 24,
      cols: 80,
    });
    expect(parseClientMessage(line)?.type).toBe("hello");
  });

  it("rejects malformed input", () => {
    expect(parseClientMessage("not json")).toBeNull();
    expect(parseClientMessage(JSON.stringify({ type: "unknown" }))).toBeNull();
    expect(
      parseClientMessage(JSON.stringify({ type: "ai_request" })),
    ).toBeNull();
  });
});

describe("respondTo", () => {
  it("acks an ai_request and nothing else", () => {
    expect(
      respondTo({
        type: "ai_request",
        request_id: "r1",
        question: "q",
        trigger: "#?",
        context_package: null,
      }),
    ).toEqual({ type: "ack", request_id: "r1" });

    expect(
      respondTo({ type: "bye", terminal_session_id: "koshell-42" }),
    ).toBeNull();
  });
});

describe("handleMessage", () => {
  it("returns an ack line for an ai_request and logs the question", () => {
    const logs: string[] = [];
    const reply = handleMessage(
      {
        type: "ai_request",
        request_id: "r1",
        question: "why did this fail",
        trigger: "#?",
        context_package: null,
      },
      (message) => logs.push(message),
    );
    expect(reply).toBe(
      `${JSON.stringify({ type: "ack", request_id: "r1" })}\n`,
    );
    expect(logs.some((line) => line.includes("why did this fail"))).toBe(true);
  });
});

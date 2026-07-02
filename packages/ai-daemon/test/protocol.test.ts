import { describe, expect, it } from "vitest";

import { parseClientMessage, serializeServerMessage } from "../src/protocol.ts";

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

describe("serializeServerMessage", () => {
  // Exact wire lines, locked in step with the Rust proto tests
  // (crates/koshell-proto/src/lib.rs).
  it("produces the exact JSONL wire lines", () => {
    expect(serializeServerMessage({ type: "ack", request_id: "req-1" })).toBe(
      '{"type":"ack","request_id":"req-1"}\n',
    );
    expect(
      serializeServerMessage({
        type: "ai_delta",
        request_id: "req-1",
        delta: "Hello",
      }),
    ).toBe('{"type":"ai_delta","request_id":"req-1","delta":"Hello"}\n');
    expect(
      serializeServerMessage({
        type: "ai_response_end",
        request_id: "req-1",
      }),
    ).toBe('{"type":"ai_response_end","request_id":"req-1"}\n');
    expect(
      serializeServerMessage({
        type: "ai_error",
        request_id: "req-1",
        message: "no provider configured",
      }),
    ).toBe(
      '{"type":"ai_error","request_id":"req-1","message":"no provider configured"}\n',
    );
  });
});

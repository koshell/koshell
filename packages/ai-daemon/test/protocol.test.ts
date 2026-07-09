import { describe, expect, it } from "bun:test";

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

  it("parses an ai_cancel", () => {
    const line = JSON.stringify({
      type: "ai_cancel",
      request_id: "koshell-req-1",
    });
    expect(parseClientMessage(line)).toEqual({
      type: "ai_cancel",
      request_id: "koshell-req-1",
    });
  });

  it("parses auth_login and auth_logout", () => {
    expect(
      parseClientMessage(
        JSON.stringify({
          type: "auth_login",
          request_id: "auth-1",
          provider: "anthropic",
        }),
      ),
    ).toEqual({
      type: "auth_login",
      request_id: "auth-1",
      provider: "anthropic",
    });
    expect(
      parseClientMessage(
        JSON.stringify({
          type: "auth_logout",
          request_id: "auth-1",
          provider: "anthropic",
        }),
      ),
    ).toEqual({
      type: "auth_logout",
      request_id: "auth-1",
      provider: "anthropic",
    });
  });

  it("parses auth_status_request with and without a provider", () => {
    expect(
      parseClientMessage(
        JSON.stringify({ type: "auth_status_request", request_id: "auth-1" }),
      ),
    ).toEqual({ type: "auth_status_request", request_id: "auth-1" });
    expect(
      parseClientMessage(
        JSON.stringify({
          type: "auth_status_request",
          request_id: "auth-1",
          provider: "openai-codex",
        }),
      ),
    ).toEqual({
      type: "auth_status_request",
      request_id: "auth-1",
      provider: "openai-codex",
    });
    // Rust's Option<String> can serialize as an explicit null; treat it as absent.
    expect(
      parseClientMessage(
        JSON.stringify({
          type: "auth_status_request",
          request_id: "auth-1",
          provider: null,
        }),
      ),
    ).toEqual({ type: "auth_status_request", request_id: "auth-1" });
  });

  it("parses auth_prompt_response with a string or null value", () => {
    expect(
      parseClientMessage(
        JSON.stringify({
          type: "auth_prompt_response",
          request_id: "auth-1",
          prompt_id: "prompt-1",
          value: "code",
        }),
      ),
    ).toEqual({
      type: "auth_prompt_response",
      request_id: "auth-1",
      prompt_id: "prompt-1",
      value: "code",
    });
    expect(
      parseClientMessage(
        JSON.stringify({
          type: "auth_prompt_response",
          request_id: "auth-1",
          prompt_id: "prompt-1",
          value: null,
        }),
      ),
    ).toEqual({
      type: "auth_prompt_response",
      request_id: "auth-1",
      prompt_id: "prompt-1",
      value: null,
    });
  });

  it("parses reload_request with and without a session_id", () => {
    expect(
      parseClientMessage(JSON.stringify({ type: "reload_request" })),
    ).toEqual({ type: "reload_request" });
    expect(
      parseClientMessage(
        JSON.stringify({ type: "reload_request", session_id: "koshell-42" }),
      ),
    ).toEqual({ type: "reload_request", session_id: "koshell-42" });
  });

  it("parses instance_status_request", () => {
    expect(
      parseClientMessage(
        JSON.stringify({
          type: "instance_status_request",
          session_id: "koshell-42",
        }),
      ),
    ).toEqual({ type: "instance_status_request", session_id: "koshell-42" });
  });

  it("rejects malformed input", () => {
    expect(parseClientMessage("not json")).toBeNull();
    expect(parseClientMessage(JSON.stringify({ type: "unknown" }))).toBeNull();
    expect(
      parseClientMessage(
        JSON.stringify({ type: "reload_request", session_id: 42 }),
      ),
    ).toBeNull();
    expect(
      parseClientMessage(JSON.stringify({ type: "instance_status_request" })),
    ).toBeNull();
    expect(
      parseClientMessage(JSON.stringify({ type: "ai_request" })),
    ).toBeNull();
    expect(
      parseClientMessage(JSON.stringify({ type: "ai_cancel" })),
    ).toBeNull();
    expect(
      parseClientMessage(JSON.stringify({ type: "auth_login" })),
    ).toBeNull();
    expect(
      parseClientMessage(
        JSON.stringify({ type: "auth_status_request", provider: "anthropic" }),
      ),
    ).toBeNull();
    expect(
      parseClientMessage(
        JSON.stringify({
          type: "auth_prompt_response",
          request_id: "auth-1",
          prompt_id: "prompt-1",
        }),
      ),
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

  it("produces the exact auth JSONL wire lines", () => {
    expect(
      serializeServerMessage({
        type: "auth_url",
        request_id: "auth-1",
        url: "https://example.test/authorize",
      }),
    ).toBe(
      '{"type":"auth_url","request_id":"auth-1","url":"https://example.test/authorize"}\n',
    );
    expect(
      serializeServerMessage({
        type: "auth_device_code",
        request_id: "auth-1",
        user_code: "ABCD-1234",
        verification_uri: "https://example.test/device",
        interval_seconds: 5,
      }),
    ).toBe(
      '{"type":"auth_device_code","request_id":"auth-1","user_code":"ABCD-1234","verification_uri":"https://example.test/device","interval_seconds":5}\n',
    );
    expect(
      serializeServerMessage({
        type: "auth_progress",
        request_id: "auth-1",
        message: "Waiting for authorization...",
      }),
    ).toBe(
      '{"type":"auth_progress","request_id":"auth-1","message":"Waiting for authorization..."}\n',
    );
    expect(
      serializeServerMessage({
        type: "auth_prompt",
        request_id: "auth-1",
        prompt_id: "prompt-1",
        message: "Paste the authorization code",
        allow_empty: false,
      }),
    ).toBe(
      '{"type":"auth_prompt","request_id":"auth-1","prompt_id":"prompt-1","message":"Paste the authorization code","allow_empty":false}\n',
    );
    expect(
      serializeServerMessage({
        type: "auth_select",
        request_id: "auth-1",
        prompt_id: "prompt-2",
        message: "How do you want to sign in?",
        options: [{ id: "browser", label: "Open a browser" }],
      }),
    ).toBe(
      '{"type":"auth_select","request_id":"auth-1","prompt_id":"prompt-2","message":"How do you want to sign in?","options":[{"id":"browser","label":"Open a browser"}]}\n',
    );
    expect(
      serializeServerMessage({
        type: "auth_result",
        request_id: "auth-1",
        ok: true,
        message: "logged in",
      }),
    ).toBe(
      '{"type":"auth_result","request_id":"auth-1","ok":true,"message":"logged in"}\n',
    );
    expect(
      serializeServerMessage({
        type: "auth_status",
        request_id: "auth-1",
        entries: [
          {
            provider: "anthropic",
            name: "Anthropic (Claude Pro/Max)",
            oauth: true,
            configured: true,
            source: "environment",
            label: "ANTHROPIC_API_KEY",
          },
        ],
      }),
    ).toBe(
      '{"type":"auth_status","request_id":"auth-1","entries":[{"provider":"anthropic","name":"Anthropic (Claude Pro/Max)","oauth":true,"configured":true,"source":"environment","label":"ANTHROPIC_API_KEY"}]}\n',
    );
    expect(serializeServerMessage({ type: "reload", ok: true })).toBe(
      '{"type":"reload","ok":true}\n',
    );
    expect(
      serializeServerMessage({
        type: "reload",
        ok: false,
        message: "config invalid",
      }),
    ).toBe('{"type":"reload","ok":false,"message":"config invalid"}\n');
    expect(
      serializeServerMessage({
        type: "instance_status",
        known: true,
        session_id: "koshell-42",
        cwd: "/home/u/proj",
        shell: "/bin/zsh",
        model: "anthropic/claude-sonnet-4-5",
        conversation: true,
        daemon_pid: 1234,
        uptime_ms: 9000,
        version: "0.1.0",
        protocol_version: 1,
        connections: 2,
      }),
    ).toBe(
      '{"type":"instance_status","known":true,"session_id":"koshell-42","cwd":"/home/u/proj","shell":"/bin/zsh","model":"anthropic/claude-sonnet-4-5","conversation":true,"daemon_pid":1234,"uptime_ms":9000,"version":"0.1.0","protocol_version":1,"connections":2}\n',
    );
  });
});

// IPC protocol types shared with the Rust terminal-core (`crates/koshell-proto`).
// Newline-delimited JSON over a Unix domain socket, tagged by `type`.

export const PROTOCOL_VERSION = 1;

export interface HelloMessage {
  type: "hello";
  protocol_version: number;
  terminal_session_id: string;
  cwd: string;
  shell: string;
  rows: number;
  cols: number;
}

export interface AiRequestMessage {
  type: "ai_request";
  request_id: string;
  question: string;
  trigger: string;
  context_package: unknown;
}

export interface ByeMessage {
  type: "bye";
  terminal_session_id: string;
}

export type ClientMessage = HelloMessage | AiRequestMessage | ByeMessage;

// Per request, the daemon sends `ack` first (parsed and enqueued), then zero or
// more `ai_delta` chunks, then exactly one of `ai_response_end` or `ai_error`.
export interface AckMessage {
  type: "ack";
  request_id: string;
}

export interface AiDeltaMessage {
  type: "ai_delta";
  request_id: string;
  delta: string;
}

export interface AiResponseEndMessage {
  type: "ai_response_end";
  request_id: string;
}

export interface AiErrorMessage {
  type: "ai_error";
  request_id: string;
  message: string;
}

export type ServerMessage =
  AckMessage | AiDeltaMessage | AiResponseEndMessage | AiErrorMessage;

// Encodes one server message as a newline-terminated JSONL line.
export function serializeServerMessage(message: ServerMessage): string {
  return `${JSON.stringify(message)}\n`;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

// Parses one JSONL line into a known client message, or null if unrecognized.
export function parseClientMessage(line: string): ClientMessage | null {
  let value: unknown;
  try {
    value = JSON.parse(line);
  } catch {
    return null;
  }

  if (!isRecord(value) || typeof value.type !== "string") {
    return null;
  }

  switch (value.type) {
    case "hello":
      if (
        typeof value.protocol_version === "number" &&
        typeof value.terminal_session_id === "string" &&
        typeof value.cwd === "string" &&
        typeof value.shell === "string" &&
        typeof value.rows === "number" &&
        typeof value.cols === "number"
      ) {
        return {
          type: "hello",
          protocol_version: value.protocol_version,
          terminal_session_id: value.terminal_session_id,
          cwd: value.cwd,
          shell: value.shell,
          rows: value.rows,
          cols: value.cols,
        };
      }
      return null;
    case "ai_request":
      if (
        typeof value.request_id === "string" &&
        typeof value.question === "string" &&
        typeof value.trigger === "string"
      ) {
        return {
          type: "ai_request",
          request_id: value.request_id,
          question: value.question,
          trigger: value.trigger,
          context_package: value.context_package,
        };
      }
      return null;
    case "bye":
      if (typeof value.terminal_session_id === "string") {
        return {
          type: "bye",
          terminal_session_id: value.terminal_session_id,
        };
      }
      return null;
    default:
      return null;
  }
}

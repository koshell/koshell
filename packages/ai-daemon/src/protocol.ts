// IPC protocol types shared with the Rust terminal-core (`crates/koshell-proto`).
// Newline-delimited JSON over a Unix domain socket, tagged by `type`.

export const PROTOCOL_VERSION = 1;

// Capability name for the read-only completed-command tools. A terminal advertises
// it in its hello; the daemon registers those tools only for connections that did, so
// a new daemon with an old terminal stays push-only instead of issuing calls that can
// never be answered. Additive: no protocol version bump.
export const CAPABILITY_COMMAND_OUTPUT_TOOLS_V1 = "command_output_tools_v1";

export interface HelloMessage {
  type: "hello";
  protocol_version: number;
  terminal_session_id: string;
  cwd: string;
  shell: string;
  rows: number;
  cols: number;
  // Absent on an older terminal, which means "no capabilities".
  capabilities?: string[];
}

export interface AiRequestMessage {
  type: "ai_request";
  request_id: string;
  question: string;
  trigger: string;
  context_package: unknown;
}

// Best-effort withdrawal of an in-flight ai_request after a user interrupt
// (Ctrl+C). The terminal has already stopped rendering locally; this only stops
// generation and unblocks the FIFO queue. The request still terminates with its
// usual single end/error marker.
export interface AiCancelMessage {
  type: "ai_cancel";
  request_id: string;
}

// Structured tool failure, safe to hand to the model as a normal tool result.
export interface ToolErrorPayload {
  code: string;
  message: string;
  details?: unknown;
}

// Settles exactly one `ai_tool_call`. A success carries ok:true and one `result`; a
// failure carries ok:false and one `error`. Both are untrusted JSON validated here at
// the wire boundary. Unknown, duplicate, mismatched, and late responses are dropped.
export interface ToolResponseMessage {
  type: "tool_response";
  request_id: string;
  tool_call_id: string;
  ok: boolean;
  result?: unknown;
  error?: ToolErrorPayload;
}

export interface ByeMessage {
  type: "bye";
  terminal_session_id: string;
}

// Diagnostics for `koshell daemon status`. Answered with `status` regardless of
// the hello handshake — asking a version-mismatched daemon for its identity is
// exactly the point. Additive (design 0004): no protocol version bump.
export interface StatusRequestMessage {
  type: "status_request";
}

// Interactive OAuth login for `koshell auth login` (design 0014). The daemon
// replies `ack`, streams auth display/prompt events, and terminates with
// exactly one `auth_result`. Dropping the connection aborts the flow.
// Additive (design 0004): no protocol version bump.
export interface AuthLoginMessage {
  type: "auth_login";
  request_id: string;
  provider: string;
}

// Removes the stored credential for a provider (`koshell auth logout`).
// Answered with `ack` then one `auth_result`.
export interface AuthLogoutMessage {
  type: "auth_logout";
  request_id: string;
  provider: string;
}

// Per-provider credential status (`koshell auth status`); `provider` limits
// the report to one entry. Answered with `ack` then one `auth_status`.
export interface AuthStatusRequestMessage {
  type: "auth_status_request";
  request_id: string;
  provider?: string;
}

// Answers a daemon-initiated `auth_prompt` or `auth_select`. `value` is the
// typed text (prompt) or the chosen option id (select); null means the user
// declined (EOF).
export interface AuthPromptResponseMessage {
  type: "auth_prompt_response";
  request_id: string;
  prompt_id: string;
  value: string | null;
}

// Re-read koshell.toml and rebuild live sessions (`koshell reload`, design 0015).
// `session_id` targets one instance's conversation; omitted (the `--all` form)
// resets every active session. Answered with one `reload`, routed by the
// `session_id` in the message rather than the requester's own connection, and
// served regardless of the hello handshake. Additive: no protocol version bump.
export interface ReloadRequestMessage {
  type: "reload_request";
  session_id?: string;
}

// One instance's live state (`koshell status`, design 0015), routed by
// `session_id` (the wrapper's terminal_session_id). Answered with one
// `instance_status`, also without a hello handshake.
export interface InstanceStatusRequestMessage {
  type: "instance_status_request";
  session_id: string;
}

// Live model discovery and selection (design 0018). These requests use a hello
// handshake on their throwaway CLI connection and are acknowledged before the
// terminal response.
export interface ModelListMessage {
  type: "model_list";
  request_id: string;
  all: boolean;
  query?: string;
}

export interface ModelShowMessage {
  type: "model_show";
  request_id: string;
  session_id?: string;
}

export interface ModelSetMessage {
  type: "model_set";
  request_id: string;
  model: string;
  session_id?: string;
  session_only: boolean;
}

export type ClientMessage =
  | HelloMessage
  | AiRequestMessage
  | AiCancelMessage
  | ToolResponseMessage
  | ByeMessage
  | StatusRequestMessage
  | AuthLoginMessage
  | AuthLogoutMessage
  | AuthStatusRequestMessage
  | AuthPromptResponseMessage
  | ReloadRequestMessage
  | InstanceStatusRequestMessage
  | ModelListMessage
  | ModelShowMessage
  | ModelSetMessage;

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

// Reports what the AI is doing mid-answer so the user can watch and interrupt.
// Announced for every tool call, including ones the daemon serves itself
// (web_search never reaches the terminal), so this is the only message that makes the
// whole tool loop visible. `message` is ready-to-render display text authored here,
// so a new tool needs no matching terminal release.
export interface AiToolActivityMessage {
  type: "ai_tool_activity";
  request_id: string;
  tool_name: string;
  phase: "started" | "failed";
  message: string;
}

// Asks the terminal to run one read-only context tool against the session state it
// owns, settled by exactly one `tool_response` with the same tool_call_id. Only sent
// to connections that advertised the matching capability.
export interface AiToolCallMessage {
  type: "ai_tool_call";
  request_id: string;
  tool_call_id: string;
  tool_name: string;
  arguments: unknown;
}

// Reply to `status_request`. `version` is the daemon package version;
// `connections` is the live terminal count at reply time.
export interface StatusMessage {
  type: "status";
  pid: number;
  version: string;
  protocol_version: number;
  uptime_ms: number;
  connections: number;
}

// Login display event: "open this URL to authorize".
export interface AuthUrlMessage {
  type: "auth_url";
  request_id: string;
  url: string;
  instructions?: string;
}

// Login display event: enter `user_code` at `verification_uri` (device-code
// flows: github-copilot, openai-codex).
export interface AuthDeviceCodeMessage {
  type: "auth_device_code";
  request_id: string;
  user_code: string;
  verification_uri: string;
  interval_seconds?: number;
  expires_in_seconds?: number;
}

// Login display event: free-form progress line.
export interface AuthProgressMessage {
  type: "auth_progress";
  request_id: string;
  message: string;
}

// Daemon-initiated free-text prompt; answered by `auth_prompt_response` with
// the same `prompt_id`.
export interface AuthPromptMessage {
  type: "auth_prompt";
  request_id: string;
  prompt_id: string;
  message: string;
  placeholder?: string;
  allow_empty: boolean;
}

export interface AuthSelectOption {
  id: string;
  label: string;
}

// Daemon-initiated selection; answered by `auth_prompt_response` with the
// chosen option id.
export interface AuthSelectMessage {
  type: "auth_select";
  request_id: string;
  prompt_id: string;
  message: string;
  options: AuthSelectOption[];
}

// Terminal marker for `auth_login` and `auth_logout`: exactly one per request.
export interface AuthResultMessage {
  type: "auth_result";
  request_id: string;
  ok: boolean;
  message?: string;
}

// One provider row in `auth_status`. `source` is a koshell-defined label
// ("stored", "environment", "config"), free-form on the wire so a new label
// never breaks an older client.
export interface AuthStatusEntry {
  provider: string;
  name: string;
  oauth: boolean;
  configured: boolean;
  source?: string;
  label?: string;
}

// Terminal reply to `auth_status_request`.
export interface AuthStatusMessage {
  type: "auth_status";
  request_id: string;
  entries: AuthStatusEntry[];
}

// Reply to `reload_request`. `ok` is whether the new config validated and was
// applied; `message` is a human summary (applied-session count, or the error).
export interface ReloadMessage {
  type: "reload";
  ok: boolean;
  message?: string;
}

// Reply to `instance_status_request`. `known` is whether the daemon has a live
// connection for that session_id; per-connection fields are set only when
// known, while the daemon-global fields are always present.
// The user's AGENTS.md as the daemon sees it (design 0022). Reported by the daemon
// rather than resolved terminal-side because the two processes can disagree about
// XDG_CONFIG_HOME, and the path that matters is the one actually read. `path` is
// always present so a typo shows as "nothing at this path" instead of silence.
export interface InstructionsStatusData {
  path: string;
  /** Absent when the file is missing or blank — the same outcome either way. */
  bytes?: number;
  truncated: boolean;
  /** Set when the file exists but could not be read, carrying why. */
  error?: string;
  /**
   * Whether the live conversation was built from the file's current text. False
   * means it was edited afterwards, so it is on disk but not in effect until
   * `koshell reload`. Absent when there is no conversation to compare against.
   */
  current?: boolean;
}

export interface InstanceStatusMessage {
  type: "instance_status";
  known: boolean;
  session_id: string;
  cwd?: string;
  shell?: string;
  model?: string;
  conversation: boolean;
  instructions?: InstructionsStatusData;
  daemon_pid: number;
  uptime_ms: number;
  version: string;
  protocol_version: number;
  connections: number;
}

export interface ModelCatalogEntry {
  ref: string;
  provider: string;
  id: string;
  name: string;
  available: boolean;
  context_window: number;
  reasoning: boolean;
}

export interface ModelCatalogMessage {
  type: "model_catalog";
  request_id: string;
  configured_model?: string;
  entries: ModelCatalogEntry[];
}

export interface ModelStateMessage {
  type: "model_state";
  request_id: string;
  configured_model?: string;
  active_model?: string;
  session_known: boolean;
  conversation: boolean;
}

export interface ModelResultMessage {
  type: "model_result";
  request_id: string;
  ok: boolean;
  message: string;
  configured_model?: string;
  active_model?: string;
}

export type ServerMessage =
  | AckMessage
  | AiDeltaMessage
  | AiResponseEndMessage
  | AiErrorMessage
  | AiToolActivityMessage
  | AiToolCallMessage
  | StatusMessage
  | AuthUrlMessage
  | AuthDeviceCodeMessage
  | AuthProgressMessage
  | AuthPromptMessage
  | AuthSelectMessage
  | AuthResultMessage
  | AuthStatusMessage
  | ReloadMessage
  | InstanceStatusMessage
  | ModelCatalogMessage
  | ModelStateMessage
  | ModelResultMessage;

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
        const message: HelloMessage = {
          type: "hello",
          protocol_version: value.protocol_version,
          terminal_session_id: value.terminal_session_id,
          cwd: value.cwd,
          shell: value.shell,
          rows: value.rows,
          cols: value.cols,
        };
        // Unknown or malformed entries are dropped rather than rejecting the
        // handshake: the hello shape is frozen, so a capability list this daemon
        // does not understand must never cost the connection.
        if (Array.isArray(value.capabilities)) {
          message.capabilities = value.capabilities.filter(
            (entry): entry is string => typeof entry === "string",
          );
        }
        return message;
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
    case "ai_cancel":
      if (typeof value.request_id === "string") {
        return {
          type: "ai_cancel",
          request_id: value.request_id,
        };
      }
      return null;
    case "tool_response": {
      if (
        typeof value.request_id !== "string" ||
        typeof value.tool_call_id !== "string" ||
        typeof value.ok !== "boolean"
      ) {
        return null;
      }
      const message: ToolResponseMessage = {
        type: "tool_response",
        request_id: value.request_id,
        tool_call_id: value.tool_call_id,
        ok: value.ok,
      };
      if (value.ok) {
        message.result = value.result;
      } else {
        // A failure without a well-formed error is itself malformed: the caller
        // would otherwise settle the call with nothing to tell the model.
        if (
          !isRecord(value.error) ||
          typeof value.error.code !== "string" ||
          typeof value.error.message !== "string"
        ) {
          return null;
        }
        message.error = {
          code: value.error.code,
          message: value.error.message,
          details: value.error.details,
        };
      }
      return message;
    }
    case "bye":
      if (typeof value.terminal_session_id === "string") {
        return {
          type: "bye",
          terminal_session_id: value.terminal_session_id,
        };
      }
      return null;
    case "status_request":
      return { type: "status_request" };
    case "reload_request": {
      const message: ReloadRequestMessage = { type: "reload_request" };
      if (typeof value.session_id === "string") {
        message.session_id = value.session_id;
      } else if (value.session_id !== undefined && value.session_id !== null) {
        return null;
      }
      return message;
    }
    case "instance_status_request":
      if (typeof value.session_id === "string") {
        return {
          type: "instance_status_request",
          session_id: value.session_id,
        };
      }
      return null;
    case "auth_login":
    case "auth_logout":
      if (
        typeof value.request_id === "string" &&
        typeof value.provider === "string"
      ) {
        return {
          type: value.type,
          request_id: value.request_id,
          provider: value.provider,
        };
      }
      return null;
    case "auth_status_request":
      if (typeof value.request_id === "string") {
        const message: AuthStatusRequestMessage = {
          type: "auth_status_request",
          request_id: value.request_id,
        };
        if (typeof value.provider === "string") {
          message.provider = value.provider;
        } else if (value.provider !== undefined && value.provider !== null) {
          return null;
        }
        return message;
      }
      return null;
    case "auth_prompt_response":
      if (
        typeof value.request_id === "string" &&
        typeof value.prompt_id === "string" &&
        (typeof value.value === "string" || value.value === null)
      ) {
        return {
          type: "auth_prompt_response",
          request_id: value.request_id,
          prompt_id: value.prompt_id,
          value: value.value,
        };
      }
      return null;
    case "model_list":
      if (
        typeof value.request_id === "string" &&
        typeof value.all === "boolean"
      ) {
        const message: ModelListMessage = {
          type: "model_list",
          request_id: value.request_id,
          all: value.all,
        };
        if (typeof value.query === "string") {
          message.query = value.query;
        } else if (value.query !== undefined && value.query !== null) {
          return null;
        }
        return message;
      }
      return null;
    case "model_show":
      if (typeof value.request_id === "string") {
        const message: ModelShowMessage = {
          type: "model_show",
          request_id: value.request_id,
        };
        if (typeof value.session_id === "string") {
          message.session_id = value.session_id;
        } else if (
          value.session_id !== undefined &&
          value.session_id !== null
        ) {
          return null;
        }
        return message;
      }
      return null;
    case "model_set":
      if (
        typeof value.request_id === "string" &&
        typeof value.model === "string" &&
        typeof value.session_only === "boolean"
      ) {
        const message: ModelSetMessage = {
          type: "model_set",
          request_id: value.request_id,
          model: value.model,
          session_only: value.session_only,
        };
        if (typeof value.session_id === "string") {
          message.session_id = value.session_id;
        } else if (
          value.session_id !== undefined &&
          value.session_id !== null
        ) {
          return null;
        }
        return message;
      }
      return null;
    default:
      return null;
  }
}

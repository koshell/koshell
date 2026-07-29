// The daemon half of the terminal tool round trip.
//
// A pi custom tool is an ordinary async function, but the data these tools read lives
// in the Rust terminal process. This bridge is what turns one into the other: it sends
// an `ai_tool_call`, parks the promise, and settles it when the matching
// `tool_response` arrives on the same connection.
//
// Its real job is bounding an untrusted, remote, cancellable dependency. Every failure
// mode here — the terminal never answers, answers twice, answers something malformed,
// or disappears mid-turn — must settle the call, because an unsettled tool call stalls
// the agent turn and therefore the `#?` answer.
import type { ToolErrorPayload } from "./protocol.ts";

/// A tool call's outcome as the tool wrapper sees it.
export type ToolOutcome =
  { ok: true; result: unknown } | { ok: false; error: ToolErrorPayload };

// Per-request safety bounds. These are not retention bounds — the API stays pageable
// across later questions. They stop one runaway agent turn from paging the whole
// retained megabyte into the model's context by accident.
export const MAX_CALLS_PER_REQUEST = 32;
export const MAX_RESULT_CHARACTERS_PER_REQUEST = 256_000;

// One local round trip over a Unix socket. Generous for a memory read; short enough
// that a wedged terminal costs one answer, not the session.
export const CALL_TIMEOUT_MS = 5_000;

interface PendingCall {
  settle: (outcome: ToolOutcome) => void;
  timer: ReturnType<typeof setTimeout>;
}

export interface ToolBridgeOptions {
  /** Sends one `ai_tool_call` on the owning connection. */
  send: (call: {
    requestId: string;
    toolCallId: string;
    toolName: string;
    args: unknown;
  }) => void;
  log: { info: (m: string) => void; warn: (m: string) => void };
}

function budgetError(message: string): ToolOutcome {
  return { ok: false, error: { code: "budget_exceeded", message } };
}

/**
 * Per-connection pending-call registry.
 *
 * Calls are bound to the FIFO-active request id: one conversation serializes prompts,
 * so at most one request is generating at a time, and a response naming a different
 * request is stale by construction.
 */
export class ToolBridge {
  private readonly options: ToolBridgeOptions;
  private readonly pending = new Map<string, PendingCall>();
  private activeRequestId: string | undefined;
  private callCount = 0;
  private resultCharacters = 0;
  private nextCallId = 1;
  private disposed = false;

  constructor(options: ToolBridgeOptions) {
    this.options = options;
  }

  /** Opens the budget window for one `#?` request. */
  beginRequest(requestId: string): void {
    this.activeRequestId = requestId;
    this.callCount = 0;
    this.resultCharacters = 0;
  }

  /**
   * Closes the window and rejects anything still outstanding. Called when the
   * request ends, is cancelled, or the agent is reset — a late response after this
   * point has nowhere to go and is dropped by `settle`.
   */
  endRequest(): void {
    this.activeRequestId = undefined;
    this.rejectAll(
      "request_inactive",
      "the request ended before the tool answered",
    );
  }

  dispose(): void {
    this.disposed = true;
    this.activeRequestId = undefined;
    this.rejectAll(
      "terminal_disconnected",
      "the terminal disconnected before the tool answered",
    );
  }

  private rejectAll(code: string, message: string): void {
    for (const [, call] of this.pending) {
      clearTimeout(call.timer);
      call.settle({ ok: false, error: { code, message } });
    }
    this.pending.clear();
  }

  /**
   * Settles one outstanding call.
   *
   * Unknown, duplicate, and late `tool_call_id`s are dropped with a log line and no
   * presentation: exactly one response settles a call, and a second one must not
   * re-resolve a promise the agent already moved past.
   */
  settle(requestId: string, toolCallId: string, outcome: ToolOutcome): void {
    const call = this.pending.get(toolCallId);
    if (call === undefined) {
      this.options.log.warn(
        `dropped tool_response for unknown or already-settled call ${toolCallId}`,
      );
      return;
    }
    if (requestId !== this.activeRequestId) {
      this.options.log.warn(
        `dropped tool_response for ${toolCallId}: request ${requestId} is not active`,
      );
      return;
    }
    this.pending.delete(toolCallId);
    clearTimeout(call.timer);
    call.settle(outcome);
  }

  /**
   * Issues one tool call and resolves when the terminal answers, the call times out,
   * the caller aborts, or the connection dies. Never rejects: every path produces a
   * `ToolOutcome` the tool wrapper can hand the model as a normal result.
   */
  call(
    toolName: string,
    args: unknown,
    signal?: AbortSignal,
  ): Promise<ToolOutcome> {
    if (this.disposed) {
      return Promise.resolve({
        ok: false,
        error: {
          code: "terminal_disconnected",
          message: "the terminal is no longer connected",
        },
      });
    }
    const requestId = this.activeRequestId;
    if (requestId === undefined) {
      return Promise.resolve({
        ok: false,
        error: {
          code: "request_inactive",
          message: "no #? request is active for this conversation",
        },
      });
    }
    if (this.callCount >= MAX_CALLS_PER_REQUEST) {
      return Promise.resolve(
        budgetError(
          `this question already used its ${String(MAX_CALLS_PER_REQUEST)} terminal tool calls; answer with the evidence you have`,
        ),
      );
    }
    if (this.resultCharacters >= MAX_RESULT_CHARACTERS_PER_REQUEST) {
      return Promise.resolve(
        budgetError(
          `this question already read its ${String(MAX_RESULT_CHARACTERS_PER_REQUEST)}-character terminal tool budget; answer with the evidence you have`,
        ),
      );
    }
    if (signal?.aborted === true) {
      return Promise.resolve({
        ok: false,
        error: {
          code: "request_inactive",
          message: "the request was cancelled",
        },
      });
    }

    this.callCount += 1;
    const toolCallId = `tool-${String(this.nextCallId)}`;
    this.nextCallId += 1;

    return new Promise<ToolOutcome>((resolve) => {
      let settled = false;
      const settle = (outcome: ToolOutcome) => {
        if (settled) {
          return;
        }
        settled = true;
        if (outcome.ok) {
          this.resultCharacters += JSON.stringify(
            outcome.result ?? null,
          ).length;
        }
        signal?.removeEventListener("abort", onAbort);
        resolve(outcome);
      };
      const onAbort = () => {
        this.pending.delete(toolCallId);
        clearTimeout(timer);
        settle({
          ok: false,
          error: {
            code: "request_inactive",
            message: "the request was cancelled",
          },
        });
      };
      const timer = setTimeout(() => {
        this.pending.delete(toolCallId);
        this.options.log.warn(
          `tool call ${toolCallId} (${toolName}) timed out after ${String(CALL_TIMEOUT_MS)}ms`,
        );
        settle({
          ok: false,
          error: {
            code: "timeout",
            message: "the terminal did not answer the tool call in time",
          },
        });
      }, CALL_TIMEOUT_MS);

      this.pending.set(toolCallId, { settle, timer });
      signal?.addEventListener("abort", onAbort, { once: true });
      this.options.send({ requestId, toolCallId, toolName, args });
    });
  }
}

// The `koshell auth` operations, driven over IPC (design 0014).
//
// pi's login flows are pure async functions driven by callbacks — they never
// touch a TTY or open a browser — so each callback maps onto one IPC message:
// display events stream to the client, prompts round-trip through AuthFlowIo
// and block the flow until the client answers (or the connection drops).
import type { AuthStorage } from "@earendil-works/pi-coding-agent";
import {
  type OAuthLoginCallbacks,
  getOAuthProviders,
} from "@earendil-works/pi-ai/oauth";

import type { KoshellConfig } from "./config.ts";
import type {
  AuthDeviceCodeMessage,
  AuthPromptMessage,
  AuthSelectMessage,
  AuthStatusEntry,
  AuthUrlMessage,
  ServerMessage,
} from "./protocol.ts";

// How a login flow reaches the terminal. `send` fires display events;
// `prompt` sends a prompt/select and resolves with the client's answer, or
// null when the client declined or the connection dropped; `signal` aborts
// the flow (connection drop or the daemon-side login timeout).
export interface AuthFlowIo {
  send(message: ServerMessage): void;
  prompt(
    message: AuthPromptMessage | AuthSelectMessage,
  ): Promise<string | null>;
  signal: AbortSignal;
}

export interface AuthOutcome {
  ok: boolean;
  message: string;
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function persistenceFailure(errors: Error[]): string {
  return `saving the credential failed: ${errors.map((e) => e.message).join("; ")}`;
}

// Runs one interactive OAuth login and persists the credential. Never
// rejects: every failure (unknown provider, cancelled prompt, aborted flow,
// persistence error) resolves to an AuthOutcome for the single auth_result.
export async function runAuthLogin(
  storage: AuthStorage,
  provider: string,
  requestId: string,
  io: AuthFlowIo,
): Promise<AuthOutcome> {
  const known = getOAuthProviders();
  const match = known.find((p) => p.id === provider);
  if (match === undefined) {
    const ids = known
      .map((p) => p.id)
      .sort()
      .join(", ");
    return {
      ok: false,
      message:
        `"${provider}" is not an OAuth provider; providers with a login flow: ${ids}. ` +
        `API-key providers are configured via koshell.toml or their environment variable instead.`,
    };
  }

  let promptSeq = 0;
  const nextPromptId = (): string => {
    promptSeq += 1;
    return `prompt-${String(promptSeq)}`;
  };
  const callbacks: OAuthLoginCallbacks = {
    onAuth: (info) => {
      const message: AuthUrlMessage = {
        type: "auth_url",
        request_id: requestId,
        url: info.url,
      };
      if (info.instructions !== undefined) {
        message.instructions = info.instructions;
      }
      io.send(message);
    },
    onDeviceCode: (info) => {
      const message: AuthDeviceCodeMessage = {
        type: "auth_device_code",
        request_id: requestId,
        user_code: info.userCode,
        verification_uri: info.verificationUri,
      };
      if (info.intervalSeconds !== undefined) {
        message.interval_seconds = info.intervalSeconds;
      }
      if (info.expiresInSeconds !== undefined) {
        message.expires_in_seconds = info.expiresInSeconds;
      }
      io.send(message);
    },
    onProgress: (text) => {
      io.send({ type: "auth_progress", request_id: requestId, message: text });
    },
    onPrompt: async (prompt) => {
      const message: AuthPromptMessage = {
        type: "auth_prompt",
        request_id: requestId,
        prompt_id: nextPromptId(),
        message: prompt.message,
        allow_empty: prompt.allowEmpty ?? false,
      };
      if (prompt.placeholder !== undefined) {
        message.placeholder = prompt.placeholder;
      }
      const value = await io.prompt(message);
      if (value === null) {
        throw new Error("login cancelled");
      }
      return value;
    },
    onSelect: async (prompt) => {
      const value = await io.prompt({
        type: "auth_select",
        request_id: requestId,
        prompt_id: nextPromptId(),
        message: prompt.message,
        options: prompt.options.map((o) => ({ id: o.id, label: o.label })),
      });
      return value ?? undefined;
    },
    // onManualCodeInput is deliberately omitted: pi races it against the
    // loopback callback server, which would leave a single-threaded client
    // blocked on stdin when the browser callback wins. Without it, pi falls
    // back to an onPrompt paste when the callback server path is unavailable.
    signal: io.signal,
  };

  try {
    await storage.login(provider, callbacks);
  } catch (error) {
    return { ok: false, message: `login failed: ${errorText(error)}` };
  }
  const errors = storage.drainErrors();
  if (errors.length > 0) {
    return { ok: false, message: persistenceFailure(errors) };
  }
  return { ok: true, message: `logged in to ${match.name}` };
}

// Removes the stored credential. Idempotent: logging out of a provider with
// nothing stored succeeds with a note, mirroring `koshell daemon stop`.
export function runAuthLogout(
  storage: AuthStorage,
  provider: string,
): AuthOutcome {
  if (!storage.has(provider)) {
    return { ok: true, message: `no stored credentials for "${provider}"` };
  }
  storage.logout(provider);
  const errors = storage.drainErrors();
  if (errors.length > 0) {
    return { ok: false, message: persistenceFailure(errors) };
  }
  return { ok: true, message: `removed stored credentials for "${provider}"` };
}

// Builds the status report. pi's getAuthStatus only reports configured=true
// for a stored credential (an exported env var still comes back
// configured=false with source "environment"), so the usable-credential
// verdict is composed here: stored, then environment, then a config api_key.
export function buildAuthStatus(
  storage: AuthStorage,
  provider: string | undefined,
  config: KoshellConfig | undefined,
): AuthStatusEntry[] {
  const oauthNames = new Map(getOAuthProviders().map((p) => [p.id, p.name]));
  const configKeyed = new Set(
    Object.entries(config?.providers ?? {})
      .filter(([, p]) => p.api_key !== undefined)
      .map(([name]) => name),
  );
  const ids =
    provider !== undefined
      ? [provider]
      : [
          ...new Set([...oauthNames.keys(), ...storage.list(), ...configKeyed]),
        ].sort();

  return ids.map((id) => {
    const entry: AuthStatusEntry = {
      provider: id,
      name: oauthNames.get(id) ?? id,
      oauth: oauthNames.has(id),
      configured: false,
    };
    const status = storage.getAuthStatus(id);
    if (status.source === "stored") {
      entry.configured = true;
      entry.source = "stored";
    } else if (status.source === "environment") {
      entry.configured = true;
      entry.source = "environment";
      if (status.label !== undefined) {
        entry.label = status.label;
      }
    } else if (configKeyed.has(id)) {
      entry.configured = true;
      entry.source = "config";
    }
    return entry;
  });
}

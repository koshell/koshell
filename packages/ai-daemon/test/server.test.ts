import { afterEach, describe, expect, it } from "bun:test";

import { AuthStorage } from "@earendil-works/pi-coding-agent";
import {
  type OAuthLoginCallbacks,
  registerOAuthProvider,
  unregisterOAuthProvider,
} from "@earendil-works/pi-ai/oauth";

import type {
  AgentFactory,
  AskOptions,
  KoshellAgent,
} from "../src/agent-runtime.ts";
import type { KoshellConfig } from "../src/config.ts";
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

// The auth handlers run against a fake OAuth provider injected into pi-ai's
// global registry (the same registry AuthStorage.login consults), so a full
// login exchange runs without any network or real credentials.
const FAKE_PROVIDER_ID = "fake-oauth";

const NO_AGENT: AgentFactory = () => {
  throw new Error("auth handling must not create an agent");
};

function registerFakeProvider(): void {
  registerOAuthProvider({
    id: FAKE_PROVIDER_ID,
    name: "Fake Provider",
    async login(callbacks: OAuthLoginCallbacks) {
      callbacks.onAuth({
        url: "https://example.test/authorize",
        instructions: "authorize, then paste the code",
      });
      const code = await callbacks.onPrompt({ message: "Code" });
      return { refresh: "r", access: code, expires: Date.now() + 3_600_000 };
    },
    refreshToken(credentials) {
      return Promise.resolve(credentials);
    },
    getApiKey(credentials) {
      return credentials.access;
    },
  });
}

function authConnection(options?: {
  storage?: AuthStorage;
  config?: KoshellConfig;
}): { connection: TerminalConnection; lines: string[]; storage: AuthStorage } {
  const { sink, lines } = collectingSink();
  const storage = options?.storage ?? AuthStorage.inMemory();
  const config = options?.config;
  const connection = new TerminalConnection(sink, {
    createAgent: NO_AGENT,
    log: NOOP_LOGGER,
    openAuthStorage: () => storage,
    loadConfig: () => {
      if (config === undefined) {
        throw new Error("no config for this test");
      }
      return config;
    },
  });
  return { connection, lines, storage };
}

function parsed(lines: string[], index: number): Record<string, unknown> {
  return JSON.parse(lines[index] ?? "{}") as Record<string, unknown>;
}

describe("TerminalConnection auth", () => {
  afterEach(() => {
    unregisterOAuthProvider(FAKE_PROVIDER_ID);
  });

  it("runs a full login exchange and stores the credential", async () => {
    registerFakeProvider();
    const { connection, lines, storage } = authConnection();

    connection.handleLine(HELLO_LINE);
    connection.handleLine(
      JSON.stringify({
        type: "auth_login",
        request_id: "a1",
        provider: FAKE_PROVIDER_ID,
      }),
    );
    await settle();

    expect(types(lines)).toEqual(["ack", "auth_url", "auth_prompt"]);
    expect(parsed(lines, 1).url).toBe("https://example.test/authorize");
    const prompt = parsed(lines, 2);
    expect(prompt.message).toBe("Code");

    connection.handleLine(
      JSON.stringify({
        type: "auth_prompt_response",
        request_id: "a1",
        prompt_id: prompt.prompt_id,
        value: "the-code",
      }),
    );
    await settle();

    expect(types(lines)).toEqual([
      "ack",
      "auth_url",
      "auth_prompt",
      "auth_result",
    ]);
    const result = parsed(lines, 3);
    expect(result.ok).toBe(true);
    expect(result.message).toContain("Fake Provider");
    expect(storage.get(FAKE_PROVIDER_ID)).toMatchObject({
      type: "oauth",
      access: "the-code",
    });
  });

  it("refuses a second login while one is in progress on the connection", async () => {
    registerFakeProvider();
    const { connection, lines } = authConnection();

    connection.handleLine(HELLO_LINE);
    connection.handleLine(
      JSON.stringify({
        type: "auth_login",
        request_id: "a1",
        provider: FAKE_PROVIDER_ID,
      }),
    );
    await settle();
    connection.handleLine(
      JSON.stringify({
        type: "auth_login",
        request_id: "a2",
        provider: FAKE_PROVIDER_ID,
      }),
    );
    await settle();

    expect(types(lines)).toEqual([
      "ack",
      "auth_url",
      "auth_prompt",
      "ack",
      "auth_result",
    ]);
    const refusal = parsed(lines, 4);
    expect(refusal.request_id).toBe("a2");
    expect(refusal.ok).toBe(false);
    expect(refusal.message).toContain("already in progress");

    // Settle the first login so it cannot leak across tests.
    connection.handleLine(
      JSON.stringify({
        type: "auth_prompt_response",
        request_id: "a1",
        prompt_id: parsed(lines, 2).prompt_id,
        value: "the-code",
      }),
    );
    await settle();
  });

  it("aborts a login mid-prompt when the connection is disposed", async () => {
    registerFakeProvider();
    const { connection, lines, storage } = authConnection();

    connection.handleLine(HELLO_LINE);
    connection.handleLine(
      JSON.stringify({
        type: "auth_login",
        request_id: "a1",
        provider: FAKE_PROVIDER_ID,
      }),
    );
    await settle();
    expect(types(lines)).toEqual(["ack", "auth_url", "auth_prompt"]);

    connection.dispose();
    await settle();

    // The login settled as a failure, but the connection is closed so no
    // auth_result reaches the wire; nothing was stored.
    expect(types(lines)).toEqual(["ack", "auth_url", "auth_prompt"]);
    expect(storage.list()).toEqual([]);
  });

  it("fails the login when the client declines a prompt", async () => {
    registerFakeProvider();
    const { connection, lines } = authConnection();

    connection.handleLine(HELLO_LINE);
    connection.handleLine(
      JSON.stringify({
        type: "auth_login",
        request_id: "a1",
        provider: FAKE_PROVIDER_ID,
      }),
    );
    await settle();
    connection.handleLine(
      JSON.stringify({
        type: "auth_prompt_response",
        request_id: "a1",
        prompt_id: parsed(lines, 2).prompt_id,
        value: null,
      }),
    );
    await settle();

    const result = parsed(lines, 3);
    expect(result.type).toBe("auth_result");
    expect(result.ok).toBe(false);
    expect(result.message).toContain("cancelled");
  });

  it("rejects a login for a provider without an OAuth flow", async () => {
    const { connection, lines } = authConnection();

    connection.handleLine(HELLO_LINE);
    connection.handleLine(
      JSON.stringify({
        type: "auth_login",
        request_id: "a1",
        provider: "not-a-provider",
      }),
    );
    await settle();

    expect(types(lines)).toEqual(["ack", "auth_result"]);
    const result = parsed(lines, 1);
    expect(result.ok).toBe(false);
    // The provider list derives from pi's live OAuth registry.
    expect(result.message).toContain("anthropic");
    expect(result.message).toContain("github-copilot");
    expect(result.message).toContain("openai-codex");
  });

  it("refuses auth requests before a hello handshake", async () => {
    const { connection, lines } = authConnection();

    connection.handleLine(
      JSON.stringify({
        type: "auth_login",
        request_id: "a1",
        provider: FAKE_PROVIDER_ID,
      }),
    );
    await settle();

    expect(types(lines)).toEqual(["ack", "auth_result"]);
    const result = parsed(lines, 1);
    expect(result.ok).toBe(false);
    expect(result.message).toContain("hello handshake");
  });

  it("logs out a stored credential and stays idempotent", async () => {
    const storage = AuthStorage.inMemory();
    storage.set(FAKE_PROVIDER_ID, {
      type: "oauth",
      refresh: "r",
      access: "a",
      expires: Date.now() + 3_600_000,
    });
    const { connection, lines } = authConnection({ storage });

    connection.handleLine(HELLO_LINE);
    connection.handleLine(
      JSON.stringify({
        type: "auth_logout",
        request_id: "a1",
        provider: FAKE_PROVIDER_ID,
      }),
    );
    await settle();
    connection.handleLine(
      JSON.stringify({
        type: "auth_logout",
        request_id: "a2",
        provider: FAKE_PROVIDER_ID,
      }),
    );
    await settle();

    expect(types(lines)).toEqual(["ack", "auth_result", "ack", "auth_result"]);
    expect(parsed(lines, 1)).toMatchObject({ ok: true });
    expect(parsed(lines, 1).message).toContain("removed");
    expect(parsed(lines, 3)).toMatchObject({ ok: true });
    expect(parsed(lines, 3).message).toContain("no stored credentials");
    expect(storage.has(FAKE_PROVIDER_ID)).toBe(false);
  });

  it("reports status with stored, environment, and config sources", async () => {
    const storage = AuthStorage.inMemory();
    storage.set("openai-codex", {
      type: "oauth",
      refresh: "r",
      access: "a",
      expires: Date.now() + 3_600_000,
    });
    const config: KoshellConfig = {
      model: "groq/some-model",
      providers: {
        groq: { api_key: "gsk-config" },
        mistral: { api_key: "sk-config" },
      },
    };
    const savedGroq = process.env.GROQ_API_KEY;
    const savedMistral = process.env.MISTRAL_API_KEY;
    delete process.env.GROQ_API_KEY;
    process.env.MISTRAL_API_KEY = "sk-env";
    try {
      const { connection, lines } = authConnection({ storage, config });

      connection.handleLine(HELLO_LINE);
      connection.handleLine(
        JSON.stringify({ type: "auth_status_request", request_id: "a1" }),
      );
      await settle();

      expect(types(lines)).toEqual(["ack", "auth_status"]);
      const entries = parsed(lines, 1).entries as Record<string, unknown>[];
      const byProvider = new Map(entries.map((e) => [e.provider, e]));

      // Stored credential wins outright.
      expect(byProvider.get("openai-codex")).toMatchObject({
        oauth: true,
        configured: true,
        source: "stored",
      });
      // A set conventional env var counts as configured, and outranks the
      // config api_key in the report.
      expect(byProvider.get("mistral")).toMatchObject({
        configured: true,
        source: "environment",
        label: "MISTRAL_API_KEY",
      });
      // A config api_key counts as configured (groq's env var is unset).
      expect(byProvider.get("groq")).toMatchObject({
        oauth: false,
        configured: true,
        source: "config",
      });
      // OAuth providers appear even with nothing configured.
      expect(byProvider.get("github-copilot")).toMatchObject({ oauth: true });
    } finally {
      if (savedGroq === undefined) {
        delete process.env.GROQ_API_KEY;
      } else {
        process.env.GROQ_API_KEY = savedGroq;
      }
      if (savedMistral === undefined) {
        delete process.env.MISTRAL_API_KEY;
      } else {
        process.env.MISTRAL_API_KEY = savedMistral;
      }
    }
  });

  it("limits status to one provider when asked", async () => {
    const { connection, lines } = authConnection();

    connection.handleLine(HELLO_LINE);
    connection.handleLine(
      JSON.stringify({
        type: "auth_status_request",
        request_id: "a1",
        provider: "openai-codex",
      }),
    );
    await settle();

    expect(types(lines)).toEqual(["ack", "auth_status"]);
    const entries = parsed(lines, 1).entries as Record<string, unknown>[];
    expect(entries).toHaveLength(1);
    expect(entries[0]).toMatchObject({
      provider: "openai-codex",
      oauth: true,
      configured: false,
    });
  });
});

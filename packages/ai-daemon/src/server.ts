import net from "node:net";
import process from "node:process";

import type { AuthStorage } from "@earendil-works/pi-coding-agent";

import type { AgentFactory, KoshellAgent } from "./agent-runtime.ts";
import {
  type AuthFlowIo,
  buildAuthStatus,
  runAuthLogin,
  runAuthLogout,
} from "./auth-flow.ts";
import { openAuthStorage } from "./auth-store.ts";
import { type KoshellConfig, loadConfig } from "./config.ts";
import { NdjsonDecoder } from "./framing.ts";
import type { Logger } from "./logging.ts";
import { buildUserPrompt } from "./prompt.ts";
import { resolveProvider } from "./provider.ts";
import {
  type AiRequestMessage,
  type AuthLoginMessage,
  type HelloMessage,
  PROTOCOL_VERSION,
  type ServerMessage,
  parseClientMessage,
  serializeServerMessage,
} from "./protocol.ts";

// A snapshot of the daemon's identity and load, for `status_request`.
export interface DaemonStatus {
  pid: number;
  version: string;
  protocol_version: number;
  uptime_ms: number;
  connections: number;
}

// Outcome of a `reload_request`: whether the new config validated and was
// applied, and a human summary for the client to print (design 0015).
export interface ReloadOutcome {
  ok: boolean;
  message: string;
}

// One instance's live state, for `instance_status_request`. `known` is whether
// the daemon has a live connection for that session_id; the per-connection
// fields are set only when known, while the daemon-global fields are always
// present (design 0015).
export interface InstanceStatusData {
  known: boolean;
  session_id: string;
  cwd?: string;
  shell?: string;
  model?: string;
  conversation: boolean;
  daemon_pid: number;
  uptime_ms: number;
  version: string;
  protocol_version: number;
  connections: number;
}

// What one TerminalConnection needs. `status` is injected by startDaemon (which
// owns the connection counter and the start time); its absence just makes
// status_request a no-op, which keeps unit tests that never exercise status
// free of the plumbing. `openAuthStorage` and `loadConfig` are injection seams
// for the `koshell auth` handlers, defaulting to the real file-backed store and
// config loader; the defaults are resolved lazily so tests that never send auth
// messages stay off the filesystem.
export interface ConnectionOptions {
  createAgent: AgentFactory;
  log: Logger;
  status?: () => DaemonStatus;
  openAuthStorage?: () => AuthStorage;
  loadConfig?: () => KoshellConfig;
  // The `koshell reload` / `koshell status` seams, injected by startDaemon
  // which owns the session registry. `reload`/`instanceStatus` route by the
  // session_id in the request (not this connection). `registerSession` /
  // `unregisterSession` let a connection add itself to the registry when its
  // hello arrives and remove itself on close. Absent seams make the handlers
  // no-ops, keeping unit tests that never exercise them free of plumbing.
  reload?: (sessionId?: string) => ReloadOutcome;
  instanceStatus?: (sessionId: string) => InstanceStatusData;
  registerSession?: (id: string, connection: TerminalConnection) => void;
  unregisterSession?: (id: string, connection: TerminalConnection) => void;
}

// An interactive login flow can sit in pi's polling loops for as long as the
// user takes to authorize; cap it so a wedged client cannot hold the
// connection (and the daemon's idle timer) open forever.
const LOGIN_TIMEOUT_MS = 15 * 60 * 1000;

// What index.ts passes to startDaemon. `version` is the daemon package version
// reported by status; the idle knobs let a terminal-less daemon exit itself.
export interface DaemonOptions {
  createAgent: AgentFactory;
  log: Logger;
  version: string;
  idleTimeoutMs?: number;
  onIdle?: () => void;
}

// Where a connection's server messages are written. Abstracted from net.Socket so
// tests can drive a TerminalConnection with an array-collecting sink.
export interface MessageSink {
  write(line: string): void;
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

// One terminal connection: holds the hello metadata and one lazy persistent agent
// conversation, and serializes ai_requests FIFO (pi forbids concurrent prompts on
// one session). The conversation is discarded when the terminal disconnects; a
// reconnecting terminal gets a fresh conversation.
//
// The hello handshake is enforced: ai_requests are only served after a hello whose
// protocol_version matches this daemon's. Anything else (no hello yet, or a
// mismatched version) is answered with an explicit ai_error so the terminal shows
// the reason inline instead of failing on a message-shape mismatch later.
export class TerminalConnection {
  private readonly sink: MessageSink;
  private readonly options: ConnectionOptions;
  private hello: HelloMessage | undefined;
  // Why ai_requests cannot be served yet; cleared by a version-matching hello.
  private helloRejection: string | undefined =
    "the terminal did not complete the hello handshake on this connection";
  private agent: Promise<KoshellAgent> | undefined;
  // The resolved model id, cached synchronously once the agent resolves so
  // `koshell status` can read it without awaiting; cleared with the agent.
  private modelId: string | undefined;
  // This connection's terminal_session_id, learned from hello; the key it is
  // registered under so it can unregister itself on close.
  private sessionId: string | undefined;
  private queue: Promise<void> = Promise.resolve();
  // Requests withdrawn by ai_cancel; consumed when run() reaches them (queued
  // requests are skipped without prompting) and cleared when the request ends,
  // so a cancel that raced past its request's completion cannot linger.
  private readonly cancelled = new Set<string>();
  private runningRequestId: string | undefined;
  // The one in-flight `koshell auth login` on this connection, with the
  // resolvers of prompts the client has not answered yet. Aborting the
  // controller resolves every pending prompt with null (see handleAuthLogin),
  // which settles the login promise.
  private activeLogin:
    | {
        requestId: string;
        controller: AbortController;
        pending: Map<string, (value: string | null) => void>;
      }
    | undefined;
  private closed = false;

  constructor(sink: MessageSink, options: ConnectionOptions) {
    this.sink = sink;
    this.options = options;
  }

  // Parses and dispatches one JSONL line from the terminal.
  handleLine(line: string): void {
    const message = parseClientMessage(line);
    if (message === null) {
      this.options.log.warn(`ignored invalid message: ${line}`);
      return;
    }
    switch (message.type) {
      case "hello":
        if (message.protocol_version !== PROTOCOL_VERSION) {
          this.hello = undefined;
          this.helloRejection =
            `protocol version mismatch: the terminal speaks v${String(message.protocol_version)}, ` +
            `this daemon speaks v${String(PROTOCOL_VERSION)}. Upgrade the older side, ` +
            `restart the daemon, and reopen this terminal window.`;
          this.options.log.warn(
            `rejected hello from ${message.terminal_session_id}: ${this.helloRejection}`,
          );
          break;
        }
        this.hello = message;
        this.helloRejection = undefined;
        // Register under the terminal_session_id so `koshell reload`/`status`,
        // arriving on their own throwaway connections, can route to this one.
        if (this.sessionId !== message.terminal_session_id) {
          if (this.sessionId !== undefined) {
            this.options.unregisterSession?.(this.sessionId, this);
          }
          this.sessionId = message.terminal_session_id;
          this.options.registerSession?.(this.sessionId, this);
        }
        this.options.log.info(
          `hello from ${message.terminal_session_id} (${message.shell}, ${String(message.cols)}x${String(message.rows)}) cwd=${message.cwd}`,
        );
        break;
      case "ai_request":
        if (this.helloRejection !== undefined) {
          // Keep the per-request contract (ack, then exactly one terminal
          // marker) so the terminal's pending-request handling stays uniform.
          this.options.log.warn(
            `refused #? [${message.request_id}]: ${this.helloRejection}`,
          );
          this.send({ type: "ack", request_id: message.request_id });
          this.send({
            type: "ai_error",
            request_id: message.request_id,
            message: this.helloRejection,
          });
          break;
        }
        this.options.log.info(`#? [${message.request_id}] ${message.question}`);
        this.options.log.debug(
          `#? [${message.request_id}] context_package: ${JSON.stringify(message.context_package)}`,
        );
        this.send({ type: "ack", request_id: message.request_id });
        this.queue = this.queue.then(() => this.run(message));
        break;
      case "ai_cancel":
        // Best-effort: the terminal already stopped rendering and suppresses
        // late messages, so this only stops generation and unblocks the queue.
        this.options.log.info(
          `#? [${message.request_id}] cancelled by the terminal`,
        );
        this.cancelled.add(message.request_id);
        if (this.runningRequestId === message.request_id) {
          void this.agent
            ?.then((agent) => {
              agent.abort();
            })
            .catch(() => undefined);
        }
        break;
      case "bye":
        this.options.log.info(`bye from ${message.terminal_session_id}`);
        this.dispose();
        break;
      case "status_request": {
        const status = this.options.status?.();
        if (status !== undefined) {
          this.send({ type: "status", ...status });
        }
        break;
      }
      case "reload_request": {
        // Daemon-global, routed by session_id; served without a hello gate,
        // like status_request. Absent seam => no-op (older-daemon behaviour).
        const outcome = this.options.reload?.(message.session_id);
        if (outcome !== undefined) {
          const reply: ServerMessage = { type: "reload", ok: outcome.ok };
          if (outcome.message.length > 0) {
            reply.message = outcome.message;
          }
          this.send(reply);
        }
        break;
      }
      case "instance_status_request": {
        const data = this.options.instanceStatus?.(message.session_id);
        if (data !== undefined) {
          this.send({ type: "instance_status", ...data });
        }
        break;
      }
      case "auth_login":
        this.handleAuthLogin(message);
        break;
      case "auth_logout": {
        this.send({ type: "ack", request_id: message.request_id });
        if (this.helloRejection !== undefined) {
          this.sendAuthResult(message.request_id, {
            ok: false,
            message: this.helloRejection,
          });
          break;
        }
        this.options.log.info(
          `auth logout [${message.request_id}] provider=${message.provider}`,
        );
        try {
          const storage = this.openAuthStorage();
          this.sendAuthResult(
            message.request_id,
            runAuthLogout(storage, message.provider),
          );
        } catch (error) {
          this.sendAuthResult(message.request_id, {
            ok: false,
            message: errorText(error),
          });
        }
        break;
      }
      case "auth_status_request": {
        this.send({ type: "ack", request_id: message.request_id });
        if (this.helloRejection !== undefined) {
          this.sendAuthResult(message.request_id, {
            ok: false,
            message: this.helloRejection,
          });
          break;
        }
        try {
          const storage = this.openAuthStorage();
          this.send({
            type: "auth_status",
            request_id: message.request_id,
            entries: buildAuthStatus(
              storage,
              message.provider,
              this.loadConfigForStatus(),
            ),
          });
        } catch (error) {
          this.sendAuthResult(message.request_id, {
            ok: false,
            message: errorText(error),
          });
        }
        break;
      }
      case "auth_prompt_response": {
        const login = this.activeLogin;
        const resolve = login?.pending.get(message.prompt_id);
        if (
          login === undefined ||
          resolve === undefined ||
          login.requestId !== message.request_id
        ) {
          // A response racing a finished (or aborted) login is expected; drop it.
          this.options.log.warn(
            `dropped auth_prompt_response for unknown prompt ${message.prompt_id}`,
          );
          break;
        }
        login.pending.delete(message.prompt_id);
        resolve(message.value);
        break;
      }
    }
  }

  // Runs one interactive login. The single auth_result is sent when the flow
  // settles; prompts stay pending until the client answers, the connection
  // drops, or the login timeout aborts the flow.
  private handleAuthLogin(message: AuthLoginMessage): void {
    this.send({ type: "ack", request_id: message.request_id });
    if (this.helloRejection !== undefined) {
      this.sendAuthResult(message.request_id, {
        ok: false,
        message: this.helloRejection,
      });
      return;
    }
    if (this.activeLogin !== undefined) {
      this.sendAuthResult(message.request_id, {
        ok: false,
        message: "another login is already in progress on this connection",
      });
      return;
    }
    let storage: AuthStorage;
    try {
      storage = this.openAuthStorage();
    } catch (error) {
      this.sendAuthResult(message.request_id, {
        ok: false,
        message: errorText(error),
      });
      return;
    }

    const login = {
      requestId: message.request_id,
      controller: new AbortController(),
      pending: new Map<string, (value: string | null) => void>(),
    };
    this.activeLogin = login;
    const signal = AbortSignal.any([
      login.controller.signal,
      AbortSignal.timeout(LOGIN_TIMEOUT_MS),
    ]);
    // An abort must settle the login even while pi awaits a prompt answer:
    // resolving the pending prompts with null makes onPrompt throw, which
    // rejects pi's flow and lets runAuthLogin return its failure outcome.
    signal.addEventListener("abort", () => {
      for (const resolve of login.pending.values()) {
        resolve(null);
      }
      login.pending.clear();
    });
    const io: AuthFlowIo = {
      send: (event) => {
        this.send(event);
      },
      prompt: (prompt) =>
        new Promise((resolve) => {
          if (signal.aborted) {
            resolve(null);
            return;
          }
          login.pending.set(prompt.prompt_id, resolve);
          this.send(prompt);
        }),
      signal,
    };
    this.options.log.info(
      `auth login [${message.request_id}] provider=${message.provider}`,
    );
    void runAuthLogin(storage, message.provider, message.request_id, io).then(
      (outcome) => {
        for (const resolve of login.pending.values()) {
          resolve(null);
        }
        login.pending.clear();
        if (this.activeLogin === login) {
          this.activeLogin = undefined;
        }
        this.options.log.info(
          `auth login [${message.request_id}] ${outcome.ok ? "succeeded" : "failed"}: ${outcome.message}`,
        );
        this.sendAuthResult(message.request_id, outcome);
      },
    );
  }

  private sendAuthResult(
    requestId: string,
    outcome: { ok: boolean; message: string },
  ): void {
    this.send({
      type: "auth_result",
      request_id: requestId,
      ok: outcome.ok,
      message: outcome.message,
    });
  }

  private openAuthStorage(): AuthStorage {
    return (this.options.openAuthStorage ?? openAuthStorage)();
  }

  // Best-effort config for the status report: an absent or invalid config
  // must not break `koshell auth status` (its whole point is diagnosing an
  // incomplete setup).
  private loadConfigForStatus(): KoshellConfig | undefined {
    try {
      return (this.options.loadConfig ?? loadConfig)();
    } catch (error) {
      this.options.log.warn(
        `auth status: config unavailable: ${errorText(error)}`,
      );
      return undefined;
    }
  }

  // Idempotent; called on bye and on socket close. An in-flight request keeps
  // running but its sends become no-ops and its errors are swallowed by run().
  // An in-flight login is aborted: dropping the connection is the cancel
  // gesture (Ctrl-C in `koshell auth login` simply exits the client).
  dispose(): void {
    if (this.closed) {
      return;
    }
    this.closed = true;
    if (this.sessionId !== undefined) {
      this.options.unregisterSession?.(this.sessionId, this);
    }
    this.activeLogin?.controller.abort();
    void this.agent
      ?.then((agent) => {
        agent.dispose();
      })
      .catch(() => undefined);
    this.agent = undefined;
    this.modelId = undefined;
  }

  // `koshell reload` seam: drop the memoized agent so the next ai_request
  // rebuilds it from the current config, WITHOUT closing the connection. The
  // teardown is deferred onto the FIFO queue so an in-flight request finishes
  // on its old session first; the next request re-enters getAgent() and picks
  // up the new config (its conversation starts fresh — history is discarded).
  // Returns whether an agent was live, so the caller can count applied sessions.
  resetAgent(): boolean {
    if (this.closed) {
      return false;
    }
    const had = this.agent !== undefined;
    const reset = async (): Promise<void> => {
      const pending = this.agent;
      this.agent = undefined;
      this.modelId = undefined;
      if (pending !== undefined) {
        try {
          (await pending).dispose();
        } catch {
          // A never-built agent's creation error is already surfaced elsewhere.
        }
      }
    };
    this.queue = this.queue.then(reset, reset);
    return had;
  }

  // Per-instance snapshot for `koshell status`. `model` is present only once
  // the agent has been built and resolved; `conversation` is whether a live
  // agent exists on this connection.
  instanceSnapshot(): {
    cwd?: string;
    shell?: string;
    model?: string;
    conversation: boolean;
  } {
    const snapshot: {
      cwd?: string;
      shell?: string;
      model?: string;
      conversation: boolean;
    } = { conversation: this.agent !== undefined };
    if (this.hello?.cwd !== undefined) {
      snapshot.cwd = this.hello.cwd;
    }
    if (this.hello?.shell !== undefined) {
      snapshot.shell = this.hello.shell;
    }
    if (this.modelId !== undefined) {
      snapshot.model = this.modelId;
    }
    return snapshot;
  }

  // Never rejects: exactly one of ai_response_end or ai_error per request.
  // A cancelled request keeps that contract: skipped before prompting when the
  // cancel arrived while it was queued (or while the agent was being created),
  // and ended normally when abort() cut the prompt short mid-generation.
  private async run(message: AiRequestMessage): Promise<void> {
    if (this.cancelled.has(message.request_id)) {
      this.options.log.info(`#? [${message.request_id}] skipped (cancelled)`);
      this.cancelled.delete(message.request_id);
      this.send({ type: "ai_response_end", request_id: message.request_id });
      return;
    }
    this.runningRequestId = message.request_id;
    try {
      const agent = await this.getAgent();
      if (this.cancelled.has(message.request_id)) {
        this.options.log.info(`#? [${message.request_id}] skipped (cancelled)`);
        this.send({ type: "ai_response_end", request_id: message.request_id });
        return;
      }
      await agent.ask({
        prompt: buildUserPrompt(message, this.hello),
        onDelta: (delta) => {
          this.send({
            type: "ai_delta",
            request_id: message.request_id,
            delta,
          });
        },
      });
      this.send({ type: "ai_response_end", request_id: message.request_id });
    } catch (error) {
      this.options.log.error(
        `#? [${message.request_id}] failed: ${errorText(error)}`,
      );
      this.send({
        type: "ai_error",
        request_id: message.request_id,
        message: errorText(error),
      });
    } finally {
      this.cancelled.delete(message.request_id);
      this.runningRequestId = undefined;
    }
  }

  private getAgent(): Promise<KoshellAgent> {
    this.agent ??= this.options
      .createAgent({
        cwd: this.hello?.cwd ?? process.cwd(),
        log: this.options.log,
      })
      .then((agent) => {
        // Cache the resolved model id so `koshell status` reads it synchronously.
        this.modelId = agent.modelId;
        return agent;
      })
      .catch((error: unknown) => {
        // A failed creation must not poison the connection: clear the memoized
        // promise so the next request retries (the user may have configured a
        // provider key in the meantime).
        this.agent = undefined;
        this.modelId = undefined;
        throw error;
      });
    return this.agent;
  }

  private send(message: ServerMessage): void {
    if (this.closed) {
      return;
    }
    this.sink.write(serializeServerMessage(message));
  }
}

// Starts the JSONL Unix-socket daemon on the given path. Tracks the live
// connection count so `status` can report it and so a terminal-less daemon can
// exit itself after `idleTimeoutMs` — the idle timer is armed at listen time
// too, so a daemon whose terminal died before connecting does not linger.
export function startDaemon(
  socketPath: string,
  options: DaemonOptions,
): net.Server {
  const startedAt = Date.now();
  let connections = 0;
  let idleTimer: ReturnType<typeof setTimeout> | undefined;

  const armIdle = (): void => {
    if (options.idleTimeoutMs === undefined || options.onIdle === undefined) {
      return;
    }
    // Never arm while a terminal is attached. This guards the listen-time arm
    // against a connection that raced in before the listen callback ran: without
    // it, the timer would arm despite the live connection and exit the daemon
    // out from under a terminal it is serving.
    if (connections > 0) {
      return;
    }
    if (idleTimer !== undefined) {
      clearTimeout(idleTimer);
    }
    idleTimer = setTimeout(() => {
      options.onIdle?.();
    }, options.idleTimeoutMs);
  };
  const cancelIdle = (): void => {
    if (idleTimer !== undefined) {
      clearTimeout(idleTimer);
      idleTimer = undefined;
    }
  };

  const status = (): DaemonStatus => ({
    pid: process.pid,
    version: options.version,
    protocol_version: PROTOCOL_VERSION,
    uptime_ms: Date.now() - startedAt,
    connections,
  });

  // Session registry: maps each connection's terminal_session_id to its
  // TerminalConnection so `koshell reload`/`status`, arriving on their own
  // throwaway connections, can route to the right instance (design 0015).
  const sessions = new Map<string, TerminalConnection>();

  // `koshell reload`: validate the new config once (a faithful dry run of what
  // createAgent does), and only if it holds, reset the target session(s) so
  // their next #? rebuilds from it. A broken config leaves every session on its
  // previous working config. sessionId === undefined is the `--all` form.
  const reload = (sessionId?: string): ReloadOutcome => {
    try {
      resolveProvider(loadConfig());
    } catch (error) {
      return {
        ok: false,
        message: `config invalid: ${errorText(error)}; sessions keep the previous configuration`,
      };
    }
    const targets =
      sessionId === undefined
        ? [...sessions.values()]
        : [sessions.get(sessionId)].filter(
            (c): c is TerminalConnection => c !== undefined,
          );
    let applied = 0;
    for (const connection of targets) {
      if (connection.resetAgent()) {
        applied += 1;
      }
    }
    const scope = sessionId === undefined ? "" : " for this instance";
    const note =
      applied === 0
        ? ` no active conversation${scope} to reset; config validated`
        : ` ${String(applied)} session(s) will use it on the next #?`;
    return { ok: true, message: `configuration reloaded;${note}` };
  };

  // `koshell status`: one instance's live state (per-connection if the session
  // is registered) plus the daemon-global facts (always present).
  const instanceStatus = (sessionId: string): InstanceStatusData => {
    const connection = sessions.get(sessionId);
    const snapshot = connection?.instanceSnapshot();
    return {
      known: connection !== undefined,
      session_id: sessionId,
      conversation: snapshot?.conversation ?? false,
      daemon_pid: process.pid,
      uptime_ms: Date.now() - startedAt,
      version: options.version,
      protocol_version: PROTOCOL_VERSION,
      connections,
      ...(snapshot?.cwd !== undefined ? { cwd: snapshot.cwd } : {}),
      ...(snapshot?.shell !== undefined ? { shell: snapshot.shell } : {}),
      ...(snapshot?.model !== undefined ? { model: snapshot.model } : {}),
    };
  };

  const connectionOptions: ConnectionOptions = {
    createAgent: options.createAgent,
    log: options.log,
    status,
    reload,
    instanceStatus,
    registerSession: (id, connection) => sessions.set(id, connection),
    unregisterSession: (id, connection) => {
      // Only drop the entry if it still points at the connection unregistering:
      // a reconnect under the same id (same wrapper pid) may have replaced it,
      // and the old connection's close must not evict the new one.
      if (sessions.get(id) === connection) {
        sessions.delete(id);
      }
    },
  };

  const server = net.createServer((socket) => {
    connections += 1;
    cancelIdle();
    const decoder = new NdjsonDecoder();
    options.log.info("terminal connected");

    const sink: MessageSink = {
      write(line: string): void {
        if (!socket.destroyed && !socket.writableEnded) {
          socket.write(line);
        }
      },
    };
    const connection = new TerminalConnection(sink, connectionOptions);

    socket.on("data", (chunk: Buffer) => {
      for (const line of decoder.push(chunk.toString("utf8"))) {
        connection.handleLine(line);
      }
    });

    socket.on("error", () => {
      // A terminal disconnecting mid-write is expected; ignore.
    });
    socket.on("close", () => {
      connection.dispose();
      connections -= 1;
      if (connections === 0) {
        armIdle();
      }
      options.log.info("terminal disconnected");
    });
  });

  server.listen(socketPath, () => {
    options.log.info(`listening on ${socketPath}`);
    armIdle();
  });

  return server;
}

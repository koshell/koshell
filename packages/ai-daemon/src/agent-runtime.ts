// pi-backed agent runtime: the only module that imports pi. One KoshellAgent wraps
// one persistent pi AgentSession, holding the conversation for one terminal session.
//
// Provider, model, and auth resolution are Koshell-owned: the config is read and
// validated (config.ts) and adapted into pi's in-memory auth/model objects
// (provider.ts) when the conversation is created, then the single resolved model is
// passed to the pi session factory. pi's own defaults (~/.pi/agent, provider env
// vars) are not consulted, except that a builtin provider without an api_key in the
// config falls back to its provider env var. Compaction is disabled, so a very long
// conversation can outgrow the model context; the conversation dies with the
// terminal session, which bounds the damage for the prototype.
import {
  createAgentSession,
  createExtensionRuntime,
  SessionManager,
  SettingsManager,
  type ResourceLoader,
} from "@earendil-works/pi-coding-agent";

import { loadConfig } from "./config.ts";
import type { Logger } from "./logging.ts";
import { SYSTEM_PROMPT } from "./prompt.ts";
import { resolveProvider } from "./provider.ts";

export interface AskOptions {
  prompt: string;
  onDelta: (delta: string) => void;
}

// One persistent conversation for one terminal session.
export interface KoshellAgent {
  // The resolved model id in use, e.g. "anthropic/claude-sonnet-4-5". Reported
  // by `koshell status`; fixed for the life of this conversation.
  readonly modelId: string;
  // Resolves when the response is complete; rejects with the provider/setup error.
  ask(options: AskOptions): Promise<void>;
  // Interrupts the in-flight ask (user Ctrl+C); a no-op when nothing is running.
  // The session survives and serves later asks.
  abort(): void;
  dispose(): void;
}

export interface AgentFactoryOptions {
  cwd: string;
  log: Logger;
}

export type AgentFactory = (
  options: AgentFactoryOptions,
) => Promise<KoshellAgent>;

function createResourceLoader(): ResourceLoader {
  return {
    getExtensions: () => ({
      extensions: [],
      errors: [],
      runtime: createExtensionRuntime(),
    }),
    getSkills: () => ({ skills: [], diagnostics: [] }),
    getPrompts: () => ({ prompts: [], diagnostics: [] }),
    getThemes: () => ({ themes: [], diagnostics: [] }),
    getAgentsFiles: () => ({ agentsFiles: [] }),
    getSystemPrompt: () => SYSTEM_PROMPT,
    getAppendSystemPrompt: () => [],
    extendResources: () => undefined,
    reload: () => Promise.resolve(),
  };
}

// Creates the production factory. Kept behind the AgentFactory seam so the server
// is testable with a fake agent.
export function createPiAgentFactory(): AgentFactory {
  return async ({ cwd, log }) => {
    // Read the config per conversation, so "edit the config, start a new
    // conversation" applies without a reload. A ConfigError (missing file,
    // invalid schema, unknown model, missing key) propagates as the #? failure,
    // which the terminal shows inline as setup guidance.
    const { authStorage, modelRegistry, model, thinkingLevel } =
      resolveProvider(loadConfig());

    const { session } = await createAgentSession({
      cwd,
      resourceLoader: createResourceLoader(),
      noTools: "all",
      sessionManager: SessionManager.inMemory(cwd),
      settingsManager: SettingsManager.inMemory({
        compaction: { enabled: false },
      }),
      authStorage,
      modelRegistry,
      model,
      // exactOptionalPropertyTypes: only pass thinkingLevel when configured.
      ...(thinkingLevel !== undefined ? { thinkingLevel } : {}),
    });

    if (session.model === undefined) {
      // The resolved model was rejected by the session factory; treat as a setup
      // failure rather than a crash.
      session.dispose();
      throw new Error(
        `AI model "${model.provider}/${model.id}" is unavailable`,
      );
    }
    log.info(`agent session created (model: ${session.model.id})`);

    // Holder object: the subscribe closure mutates it during the awaited prompt,
    // which TypeScript's narrowing does not track for plain locals.
    const streaming: {
      onDelta?: (delta: string) => void;
      errorMessage?: string;
    } = {};
    session.subscribe((event) => {
      if (event.type !== "message_update") {
        return;
      }
      const assistantEvent = event.assistantMessageEvent;
      if (assistantEvent.type === "text_delta") {
        streaming.onDelta?.(assistantEvent.delta);
      } else if (assistantEvent.type === "error") {
        streaming.errorMessage =
          assistantEvent.error.errorMessage ??
          `provider error (${assistantEvent.reason})`;
      }
    });

    // Reads (and clears) the streamed error through a function boundary, so the
    // compiler does not narrow the check to the pre-prompt value.
    const takeError = (): string | undefined => {
      const message = streaming.errorMessage;
      delete streaming.errorMessage;
      return message;
    };

    return {
      modelId: `${model.provider}/${model.id}`,
      async ask(options: AskOptions): Promise<void> {
        streaming.onDelta = options.onDelta;
        delete streaming.errorMessage;
        try {
          await session.prompt(options.prompt);
        } finally {
          delete streaming.onDelta;
        }
        const failure = takeError();
        if (failure !== undefined) {
          throw new Error(failure);
        }
      },
      abort(): void {
        void session.abort().catch(() => undefined);
      },
      dispose(): void {
        void session.abort().catch(() => undefined);
        session.dispose();
      },
    };
  };
}

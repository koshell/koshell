// pi-backed agent runtime: the only module that imports pi. One KoshellAgent wraps
// one persistent pi AgentSession, holding the conversation for one terminal session.
//
// Provider, model, and auth resolution are delegated to pi's own defaults
// (~/.pi/agent/auth.json, then provider environment variables such as
// ANTHROPIC_API_KEY). Koshell-owned XDG/TOML provider configuration replaces this
// in a later stage. Compaction is disabled, so a very long conversation can outgrow
// the model context; the conversation dies with the terminal session, which bounds
// the damage for the prototype.
import {
  createAgentSession,
  createExtensionRuntime,
  SessionManager,
  SettingsManager,
  type ResourceLoader,
} from "@earendil-works/pi-coding-agent";

import type { Logger } from "./logging.ts";
import { SYSTEM_PROMPT } from "./prompt.ts";

export interface AskOptions {
  prompt: string;
  onDelta: (delta: string) => void;
}

// One persistent conversation for one terminal session.
export interface KoshellAgent {
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
    const { session } = await createAgentSession({
      cwd,
      resourceLoader: createResourceLoader(),
      noTools: "all",
      sessionManager: SessionManager.inMemory(cwd),
      settingsManager: SettingsManager.inMemory({
        compaction: { enabled: false },
      }),
    });

    if (session.model === undefined) {
      session.dispose();
      throw new Error(
        "no AI provider configured (set a provider API key such as ANTHROPIC_API_KEY, or configure pi)",
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

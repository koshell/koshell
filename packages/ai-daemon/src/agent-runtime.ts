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

import { type KoshellConfig, loadConfig } from "./config.ts";
import type { Logger } from "./logging.ts";
import { buildSystemPrompt } from "./prompt.ts";
import { resolveModel, resolveProvider } from "./provider.ts";
import type { ToolBridge } from "./tool-bridge.ts";
import { type AnnounceTool, createCustomTools } from "./tools.ts";

export interface AskOptions {
  prompt: string;
  onDelta: (delta: string) => void;
}

// One persistent conversation for one terminal session.
export interface KoshellAgent {
  // The active model id, e.g. "anthropic/claude-sonnet-4-5". Reported by
  // `koshell status` and updated after an in-place switch.
  readonly modelId: string;
  // Stable serialization of provider/thinking construction inputs, excluding the
  // root default model. `koshell reload` uses it to prove a change is model-only.
  readonly configurationFingerprint?: string;
  // Switches this AgentSession in place, preserving its transcript. Optional only
  // for lightweight injected test agents; the production pi agent implements it.
  setModel?(modelId: string): Promise<void>;
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
  /**
   * The terminal tool round trip for this connection, present only when the terminal
   * advertised `command_output_tools_v1`. Absent keeps the conversation push-only.
   */
  bridge?: ToolBridge | undefined;
  /**
   * Reports each tool call to the user's terminal so the work is visible while it
   * happens. Absent leaves the tool loop silent (tests, non-terminal callers).
   */
  announce?: AnnounceTool | undefined;
}

export type AgentFactory = (
  options: AgentFactoryOptions,
) => Promise<KoshellAgent>;

const MODEL_SWITCH_RESERVE_TOKENS = 16_384;

export function assertModelSwitchCapacity(
  modelId: string,
  contextWindow: number,
  maxTokens: number,
  retainedTokens: number | null,
): void {
  if (retainedTokens === null) {
    throw new Error(
      `cannot safely switch to "${modelId}" because retained context usage is unknown`,
    );
  }
  const reserveTokens = Math.min(MODEL_SWITCH_RESERVE_TOKENS, maxTokens);
  if (retainedTokens + reserveTokens > contextWindow) {
    throw new Error(
      `cannot switch to "${modelId}": the retained conversation needs about ${String(retainedTokens)} tokens plus a ${String(reserveTokens)}-token response reserve, but the model context window is ${String(contextWindow)}. Start a new conversation or choose a larger-context model.`,
    );
  }
}

export function configurationFingerprint(config: KoshellConfig): string {
  return JSON.stringify({
    thinking_level: config.thinking_level ?? null,
    providers: config.providers,
    // The tool catalog and the system prompt are both built from `[search]` at
    // session creation, and neither can be swapped in place the way a model can. A
    // change here must therefore rebuild the conversation, not switch it.
    search: config.search ?? null,
  });
}

function createResourceLoader(systemPrompt: string): ResourceLoader {
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
    getSystemPrompt: () => systemPrompt,
    getAppendSystemPrompt: () => [],
    extendResources: () => undefined,
    reload: () => Promise.resolve(),
  };
}

// Creates the production factory. Kept behind the AgentFactory seam so the server
// is testable with a fake agent.
export function createPiAgentFactory(): AgentFactory {
  return async ({ cwd, log, bridge, announce }) => {
    // Read the config when constructing a conversation. `koshell model` can
    // later switch this session in place; other config changes rebuild it via
    // reload. A ConfigError propagates as the #? failure shown inline.
    const config = loadConfig();
    const { authStorage, modelRegistry, model, thinkingLevel } =
      resolveProvider(config);

    // Custom tools are assembled from what is actually configured. With none, the
    // session keeps `noTools: "all"` (pi's builtins plus custom tools all disabled),
    // which is the historical push-only behavior. With some, `noTools: "builtin"`
    // enables exactly these while pi's file, shell, edit, and write tools stay off —
    // Koshell's observe-only boundary is what this array contains.
    const customTools = createCustomTools({
      config,
      env: process.env,
      log,
      bridge,
      announce,
    });
    if (customTools.length > 0) {
      log.info(
        `custom tools active: ${customTools.map((tool) => tool.name).join(", ")}`,
      );
    }
    const toolNames = new Set(customTools.map((tool) => tool.name));

    const { session } = await createAgentSession({
      cwd,
      resourceLoader: createResourceLoader(
        buildSystemPrompt({
          webSearch: toolNames.has("web_search"),
          commandOutput: toolNames.has("read_command_output"),
        }),
      ),
      noTools: customTools.length > 0 ? "builtin" : "all",
      ...(customTools.length > 0 ? { customTools } : {}),
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

    let currentModelId = `${model.provider}/${model.id}`;
    return {
      get modelId(): string {
        return currentModelId;
      },
      configurationFingerprint: configurationFingerprint(config),
      async setModel(modelId: string): Promise<void> {
        // `koshell auth login/logout` can mutate the shared credential file after
        // this conversation was created. Refresh it before validating the target.
        authStorage.reload();
        const target = resolveModel(modelRegistry, modelId);
        const targetId = `${target.provider}/${target.id}`;
        if (targetId === currentModelId) {
          return;
        }
        const usage = session.getContextUsage();
        assertModelSwitchCapacity(
          targetId,
          target.contextWindow,
          target.maxTokens,
          usage === undefined ? 0 : usage.tokens,
        );
        await session.setModel(target);
        currentModelId = targetId;
        log.info(`agent model switched in place (model: ${targetId})`);
      },
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

import type { AgentTool, AgentToolResult } from "@earendil-works/pi-agent-core";
import { Type, validateToolArguments } from "@earendil-works/pi-ai";
import type {
  CacheRetention,
  Static,
  TSchema,
  ToolCall,
} from "@earendil-works/pi-ai";
import {
  buildTerminalContext,
  type TerminalContext,
  type TerminalContextOptions,
  type TerminalContextTimelineSource,
  type TerminalScreenChange,
} from "./terminal-context.ts";
import type {
  ScreenSnapshotDiff,
  ScreenSnapshotEntry,
  TerminalEvent,
  TimelineEntry,
} from "./timeline.ts";

export const AI_CONTEXT_CONTRACT_VERSION = "koshell_ai_context_v1";
export const AI_CONTEXT_STABLE_PREFIX_VERSION = "koshell_ai_stable_prefix_v1";
export const AI_CONTEXT_TOOL_CATALOG_VERSION = "koshell_context_tools_v1";

export type AiContextPrimarySource = TerminalContext["primarySource"];

export type KoshellContextToolName =
  | "koshell_get_current_context"
  | "koshell_get_screen_snapshot"
  | "koshell_diff_screen_snapshots"
  | "koshell_list_recent_screen_changes"
  | "koshell_get_recent_timeline_events";

export type KoshellContextTool =
  | ReturnType<typeof createGetCurrentContextTool>
  | ReturnType<typeof createGetScreenSnapshotTool>
  | ReturnType<typeof createDiffScreenSnapshotsTool>
  | ReturnType<typeof createListRecentScreenChangesTool>
  | ReturnType<typeof createGetRecentTimelineEventsTool>;

export interface AiContextCachePolicy {
  stablePrefixVersion: string;
  toolCatalogVersion: string;
  dynamicContextPlacement: "suffix";
  keepToolCatalogStable: true;
  appendRuntimeStateOnly: true;
  cacheRetentionHint: CacheRetention;
}

export interface AiContextFollowup {
  toolName: KoshellContextToolName;
  reason: string;
  input: Record<string, unknown>;
}

export interface AiContextNotableEvent {
  type:
    | "large_screen_change"
    | "alternate_screen_active"
    | "recent_human_input"
    | "visible_output_available"
    | "current_screen_available";
  summary: string;
  snapshotId?: string;
  previousSnapshotId?: string;
}

export interface AiTerminalContextPackage {
  contractVersion: string;
  cachePolicy: AiContextCachePolicy;
  dynamicContext: {
    primaryText: string;
    primarySource: AiContextPrimarySource;
    recentInput: string;
    recentPtyOutput: string;
    recentVisibleOutput: string;
    currentScreen: string | null;
    currentSnapshotId: string | null;
    altScreen: boolean;
    screenRows: number | null;
    screenColumns: number | null;
    recentScreenChanges: TerminalScreenChange[];
    notableEvents: AiContextNotableEvent[];
    availableFollowups: AiContextFollowup[];
  };
  budget: Required<AiContextBudget>;
}

export interface AiContextBudget extends TerminalContextOptions {
  primaryTextMaxCharacters?: number;
  recentTimelineEventsLimit?: number;
}

export interface AiContextTimelineSource extends TerminalContextTimelineSource {
  listEntries(): TimelineEntry[];
  getScreenSnapshot(snapshotId: string): ScreenSnapshotEntry | undefined;
  diffScreenSnapshots(
    fromSnapshotId: string,
    toSnapshotId: string,
  ): ScreenSnapshotDiff;
}

export interface AiCurrentContextToolDetails {
  contextPackage: AiTerminalContextPackage;
}

export interface AiScreenSnapshotToolDetails {
  snapshot: AiScreenSnapshotToolResult;
}

export interface AiScreenSnapshotDiffToolDetails {
  diff: ScreenSnapshotDiff;
}

export interface AiScreenChangesToolDetails {
  changes: TerminalScreenChange[];
}

export interface AiTimelineEventsToolDetails {
  events: AiTimelineEventSummary[];
}

export interface AiScreenSnapshotToolResult {
  snapshotId: string;
  rows: number;
  columns: number;
  altScreen: boolean;
  screen: string | null;
}

export interface AiTimelineEventSummary {
  id: string;
  ts: number;
  type: TerminalEvent["type"];
  summary: string;
}

const AiContextBudgetSchema = Type.Object(
  {
    primaryTextMaxCharacters: Type.Optional(Type.Integer({ minimum: 0 })),
    recentInputMaxCharacters: Type.Optional(Type.Integer({ minimum: 0 })),
    recentPtyOutputMaxCharacters: Type.Optional(Type.Integer({ minimum: 0 })),
    recentVisibleOutputMaxCharacters: Type.Optional(
      Type.Integer({ minimum: 0 }),
    ),
    currentScreenMaxCharacters: Type.Optional(Type.Integer({ minimum: 0 })),
    recentScreenChangesLimit: Type.Optional(Type.Integer({ minimum: 0 })),
    recentTimelineEventsLimit: Type.Optional(Type.Integer({ minimum: 0 })),
  },
  { additionalProperties: false },
);

const GetCurrentContextParameters = Type.Object(
  {
    budget: Type.Optional(AiContextBudgetSchema),
  },
  { additionalProperties: false },
);

const GetScreenSnapshotParameters = Type.Object(
  {
    snapshotId: Type.String(),
  },
  { additionalProperties: false },
);

const DiffScreenSnapshotsParameters = Type.Object(
  {
    fromSnapshotId: Type.String(),
    toSnapshotId: Type.String(),
  },
  { additionalProperties: false },
);

const ListRecentScreenChangesParameters = Type.Object(
  {
    limit: Type.Optional(Type.Integer({ minimum: 0 })),
  },
  { additionalProperties: false },
);

const GetRecentTimelineEventsParameters = Type.Object(
  {
    limit: Type.Optional(Type.Integer({ minimum: 0 })),
  },
  { additionalProperties: false },
);

type GetCurrentContextInput = Static<typeof GetCurrentContextParameters>;
type GetScreenSnapshotInput = Static<typeof GetScreenSnapshotParameters>;
type DiffScreenSnapshotsInput = Static<typeof DiffScreenSnapshotsParameters>;
type ListRecentScreenChangesInput = Static<
  typeof ListRecentScreenChangesParameters
>;
type GetRecentTimelineEventsInput = Static<
  typeof GetRecentTimelineEventsParameters
>;

const KOSHELL_CONTEXT_TOOL_NAMES: readonly KoshellContextToolName[] = [
  "koshell_get_current_context",
  "koshell_get_screen_snapshot",
  "koshell_diff_screen_snapshots",
  "koshell_list_recent_screen_changes",
  "koshell_get_recent_timeline_events",
];

const DEFAULT_AI_CONTEXT_BUDGET: Required<AiContextBudget> = {
  primaryTextMaxCharacters: 8_000,
  recentInputMaxCharacters: 2_000,
  recentPtyOutputMaxCharacters: 8_000,
  recentVisibleOutputMaxCharacters: 8_000,
  currentScreenMaxCharacters: 8_000,
  recentScreenChangesLimit: 20,
  recentTimelineEventsLimit: 50,
};

export function getAiContextCachePolicy(): AiContextCachePolicy {
  return {
    stablePrefixVersion: AI_CONTEXT_STABLE_PREFIX_VERSION,
    toolCatalogVersion: AI_CONTEXT_TOOL_CATALOG_VERSION,
    dynamicContextPlacement: "suffix",
    keepToolCatalogStable: true,
    appendRuntimeStateOnly: true,
    cacheRetentionHint: "short",
  };
}

export function createKoshellContextTools(
  timeline: AiContextTimelineSource,
): KoshellContextTool[] {
  return [
    createGetCurrentContextTool(timeline),
    createGetScreenSnapshotTool(timeline),
    createDiffScreenSnapshotsTool(timeline),
    createListRecentScreenChangesTool(timeline),
    createGetRecentTimelineEventsTool(timeline),
  ];
}

export function getKoshellContextToolNames(): KoshellContextToolName[] {
  return [...KOSHELL_CONTEXT_TOOL_NAMES];
}

export async function executeKoshellContextTool(
  timeline: AiContextTimelineSource,
  toolName: KoshellContextToolName,
  input: Record<string, unknown> = {},
  signal?: AbortSignal,
): Promise<AgentToolResult<unknown>> {
  switch (toolName) {
    case "koshell_get_current_context":
      return executeValidatedTool(
        createGetCurrentContextTool(timeline),
        toolName,
        input,
        signal,
      );
    case "koshell_get_screen_snapshot":
      return executeValidatedTool(
        createGetScreenSnapshotTool(timeline),
        toolName,
        input,
        signal,
      );
    case "koshell_diff_screen_snapshots":
      return executeValidatedTool(
        createDiffScreenSnapshotsTool(timeline),
        toolName,
        input,
        signal,
      );
    case "koshell_list_recent_screen_changes":
      return executeValidatedTool(
        createListRecentScreenChangesTool(timeline),
        toolName,
        input,
        signal,
      );
    case "koshell_get_recent_timeline_events":
      return executeValidatedTool(
        createGetRecentTimelineEventsTool(timeline),
        toolName,
        input,
        signal,
      );
  }
}

export function buildAiTerminalContextPackage(
  timeline: AiContextTimelineSource,
  budget: AiContextBudget = {},
): AiTerminalContextPackage {
  const resolvedBudget = resolveBudget(budget);
  const terminalContext = buildTerminalContext(timeline, resolvedBudget);
  const latestSnapshot = timeline.getLatestScreenSnapshot()?.event;
  const primaryText = trimStartToMaxCharacters(
    terminalContext.primaryText,
    resolvedBudget.primaryTextMaxCharacters,
  );

  return {
    contractVersion: AI_CONTEXT_CONTRACT_VERSION,
    cachePolicy: getAiContextCachePolicy(),
    dynamicContext: {
      primaryText,
      primarySource: terminalContext.primarySource,
      recentInput: terminalContext.recentInput,
      recentPtyOutput: terminalContext.recentPtyOutput,
      recentVisibleOutput: terminalContext.recentVisibleOutput,
      currentScreen: terminalContext.currentScreen ?? null,
      currentSnapshotId: latestSnapshot?.snapshotId ?? null,
      altScreen: terminalContext.altScreen,
      screenRows: terminalContext.screenRows ?? null,
      screenColumns: terminalContext.screenColumns ?? null,
      recentScreenChanges: terminalContext.recentScreenChanges,
      notableEvents: getNotableEvents(terminalContext, latestSnapshot),
      availableFollowups: getAvailableFollowups(
        terminalContext,
        latestSnapshot?.snapshotId,
      ),
    },
    budget: resolvedBudget,
  };
}

async function executeValidatedTool<TParameters extends TSchema, TDetails>(
  tool: AgentTool<TParameters, TDetails>,
  toolName: KoshellContextToolName,
  input: Record<string, unknown>,
  signal?: AbortSignal,
): Promise<AgentToolResult<TDetails>> {
  const toolCall: ToolCall = {
    type: "toolCall",
    id: "koshell-context-tool-call",
    name: toolName,
    arguments: input,
  };
  const params = validateToolArguments(tool, toolCall) as Static<TParameters>;

  return tool.execute(toolCall.id, params, signal);
}

function createGetCurrentContextTool(
  timeline: AiContextTimelineSource,
): AgentTool<
  typeof GetCurrentContextParameters,
  AiCurrentContextToolDetails
> & {
  name: "koshell_get_current_context";
} {
  return {
    name: "koshell_get_current_context",
    label: "Get Current Terminal Context",
    description:
      "Return the current Koshell terminal context package using the stable cache-aware contract. Use this when the initial context is stale or when a tool-call turn needs a refreshed suffix payload. The response keeps the same shape across runtime states and places changing terminal data in the dynamic context section.",
    parameters: GetCurrentContextParameters,
    execute(_toolCallId, params: GetCurrentContextInput) {
      const contextPackage = buildAiTerminalContextPackage(
        timeline,
        params.budget,
      );

      return Promise.resolve({
        content: [
          {
            type: "text" as const,
            text: formatCurrentContextForTool(contextPackage),
          },
        ],
        details: { contextPackage },
      });
    },
  };
}

function createGetScreenSnapshotTool(
  timeline: AiContextTimelineSource,
): AgentTool<
  typeof GetScreenSnapshotParameters,
  AiScreenSnapshotToolDetails
> & {
  name: "koshell_get_screen_snapshot";
} {
  return {
    name: "koshell_get_screen_snapshot",
    label: "Get Screen Snapshot",
    description:
      "Fetch one terminal screen snapshot by its stable snapshot id. Use this only when lightweight context summaries indicate that full screen contents are needed. The tool returns screen text, dimensions, and alternate-screen status for that exact snapshot.",
    parameters: GetScreenSnapshotParameters,
    execute(_toolCallId, params: GetScreenSnapshotInput) {
      const snapshot = getRequiredScreenSnapshot(timeline, params.snapshotId);
      const result: AiScreenSnapshotToolResult = {
        snapshotId: snapshot.event.snapshotId,
        rows: snapshot.event.rows,
        columns: snapshot.event.columns,
        altScreen: snapshot.event.altScreen,
        screen: snapshot.event.screen ?? null,
      };

      return Promise.resolve({
        content: [
          {
            type: "text" as const,
            text: `Screen snapshot ${result.snapshotId}: ${String(result.rows)} rows x ${String(result.columns)} columns, altScreen=${String(result.altScreen)}.\n${result.screen ?? ""}`,
          },
        ],
        details: { snapshot: result },
      });
    },
  };
}

function createDiffScreenSnapshotsTool(
  timeline: AiContextTimelineSource,
): AgentTool<
  typeof DiffScreenSnapshotsParameters,
  AiScreenSnapshotDiffToolDetails
> & {
  name: "koshell_diff_screen_snapshots";
} {
  return {
    name: "koshell_diff_screen_snapshots",
    label: "Diff Screen Snapshots",
    description:
      "Compare two terminal screen snapshots by id and return detailed line-level hunks. Use this after recent screen-change summaries identify a relevant transition. Prefer this over requesting full snapshots when the question is about what changed between two moments.",
    parameters: DiffScreenSnapshotsParameters,
    execute(_toolCallId, params: DiffScreenSnapshotsInput) {
      const diff = timeline.diffScreenSnapshots(
        params.fromSnapshotId,
        params.toSnapshotId,
      );

      return Promise.resolve({
        content: [
          {
            type: "text" as const,
            text: `Screen diff ${diff.fromSnapshotId} -> ${diff.toSnapshotId}: +${String(diff.addedLines)}, -${String(diff.removedLines)}.`,
          },
        ],
        details: { diff },
      });
    },
  };
}

function createListRecentScreenChangesTool(
  timeline: AiContextTimelineSource,
): AgentTool<
  typeof ListRecentScreenChangesParameters,
  AiScreenChangesToolDetails
> & {
  name: "koshell_list_recent_screen_changes";
} {
  return {
    name: "koshell_list_recent_screen_changes",
    label: "List Recent Screen Changes",
    description:
      "List recent lightweight screen-change summaries, including added and removed line counts and large-change flags. Use this to orient around terminal movement without reading full screen contents. The optional limit bounds the number of returned summaries.",
    parameters: ListRecentScreenChangesParameters,
    execute(_toolCallId, params: ListRecentScreenChangesInput) {
      const context = buildTerminalContext(timeline, {
        recentScreenChangesLimit:
          params.limit ?? DEFAULT_AI_CONTEXT_BUDGET.recentScreenChangesLimit,
      });

      return Promise.resolve({
        content: [
          {
            type: "text" as const,
            text: formatScreenChangesForTool(context.recentScreenChanges),
          },
        ],
        details: { changes: context.recentScreenChanges },
      });
    },
  };
}

function createGetRecentTimelineEventsTool(
  timeline: AiContextTimelineSource,
): AgentTool<
  typeof GetRecentTimelineEventsParameters,
  AiTimelineEventsToolDetails
> & {
  name: "koshell_get_recent_timeline_events";
} {
  return {
    name: "koshell_get_recent_timeline_events",
    label: "Get Recent Timeline Events",
    description:
      "Return compact summaries of recent timeline events. Use this for orientation around user input, PTY output, visible output, screen snapshots, and future AI request or response events. The response is intentionally summarized to avoid flooding the model context with raw terminal data.",
    parameters: GetRecentTimelineEventsParameters,
    execute(_toolCallId, params: GetRecentTimelineEventsInput) {
      const events = getRecentTimelineEventSummaries(
        timeline,
        params.limit ?? DEFAULT_AI_CONTEXT_BUDGET.recentTimelineEventsLimit,
      );

      return Promise.resolve({
        content: [
          {
            type: "text" as const,
            text: formatTimelineEventsForTool(events),
          },
        ],
        details: { events },
      });
    },
  };
}

function getAvailableFollowups(
  context: TerminalContext,
  currentSnapshotId: string | undefined,
): AiContextFollowup[] {
  const followups: AiContextFollowup[] = [
    {
      toolName: "koshell_list_recent_screen_changes",
      reason: "Inspect recent screen transitions without loading full screens.",
      input: { limit: context.recentScreenChanges.length },
    },
    {
      toolName: "koshell_get_recent_timeline_events",
      reason: "Inspect compact recent timeline event summaries.",
      input: { limit: DEFAULT_AI_CONTEXT_BUDGET.recentTimelineEventsLimit },
    },
  ];

  if (currentSnapshotId !== undefined) {
    followups.push({
      toolName: "koshell_get_screen_snapshot",
      reason:
        "Fetch the current full screen snapshot if the summary is insufficient.",
      input: { snapshotId: currentSnapshotId },
    });
  }

  for (const change of context.recentScreenChanges) {
    if (change.previousSnapshotId === undefined) {
      continue;
    }

    followups.push({
      toolName: "koshell_diff_screen_snapshots",
      reason: change.largeChange
        ? "Inspect the detailed line-level diff for a large screen change."
        : "Inspect the detailed line-level diff for this screen change if needed.",
      input: {
        fromSnapshotId: change.previousSnapshotId,
        toSnapshotId: change.snapshotId,
      },
    });
  }

  return followups;
}

function getNotableEvents(
  context: TerminalContext,
  latestSnapshot: ScreenSnapshotEntry["event"] | undefined,
): AiContextNotableEvent[] {
  const events: AiContextNotableEvent[] = [];

  for (const change of context.recentScreenChanges) {
    if (!change.largeChange) {
      continue;
    }

    const event: AiContextNotableEvent = {
      type: "large_screen_change",
      summary: `Screen changed by ${change.summary}.`,
      snapshotId: change.snapshotId,
    };

    if (change.previousSnapshotId !== undefined) {
      event.previousSnapshotId = change.previousSnapshotId;
    }

    events.push(event);
  }

  if (context.altScreen && latestSnapshot) {
    events.push({
      type: "alternate_screen_active",
      summary: "The terminal is currently using the alternate screen buffer.",
      snapshotId: latestSnapshot.snapshotId,
    });
  }

  if (context.recentInput.length > 0) {
    events.push({
      type: "recent_human_input",
      summary: "Recent visible human input is available in dynamic context.",
    });
  }

  if (context.recentVisibleOutput.length > 0) {
    events.push({
      type: "visible_output_available",
      summary: "Recent cleaned visible output is available in dynamic context.",
    });
  }

  if (context.currentScreen !== undefined) {
    const event: AiContextNotableEvent = {
      type: "current_screen_available",
      summary: "A current terminal screen snapshot is available.",
    };

    if (latestSnapshot?.snapshotId !== undefined) {
      event.snapshotId = latestSnapshot.snapshotId;
    }

    events.push(event);
  }

  return events;
}

function getRecentTimelineEventSummaries(
  timeline: AiContextTimelineSource,
  limit: number,
): AiTimelineEventSummary[] {
  assertNonNegativeSafeInteger(limit, "limit");

  if (limit === 0) {
    return [];
  }

  return timeline
    .listEntries()
    .slice(-limit)
    .map((entry) => ({
      id: entry.id,
      ts: entry.event.ts,
      type: entry.event.type,
      summary: summarizeTimelineEvent(entry.event),
    }));
}

function summarizeTimelineEvent(event: TerminalEvent): string {
  switch (event.type) {
    case "human_input":
      return event.visible === false
        ? "Hidden human input was recorded."
        : `Human input: ${truncateForSummary(event.data)}`;
    case "pty_output":
      return `PTY output: ${truncateForSummary(event.data)}`;
    case "visible_output":
      return `Visible output: ${truncateForSummary(event.text)}`;
    case "command_start":
      return `Command started: ${event.command}`;
    case "command_end":
      return `Command ended: ${event.command}`;
    case "screen_snapshot":
      return `Screen snapshot ${event.snapshotId}${event.altScreen ? " in alternate screen" : ""}.`;
    case "ai_request":
      return `AI request ${event.requestId}: ${truncateForSummary(event.question)}`;
    case "ai_response":
      return `AI response ${event.requestId}: ${truncateForSummary(event.text)}`;
    case "control_mode_change":
      return `Control mode changed from ${event.from} to ${event.to}: ${event.reason}`;
  }
}

function resolveBudget(budget: AiContextBudget): Required<AiContextBudget> {
  const resolved = {
    primaryTextMaxCharacters:
      budget.primaryTextMaxCharacters ??
      DEFAULT_AI_CONTEXT_BUDGET.primaryTextMaxCharacters,
    recentInputMaxCharacters:
      budget.recentInputMaxCharacters ??
      DEFAULT_AI_CONTEXT_BUDGET.recentInputMaxCharacters,
    recentPtyOutputMaxCharacters:
      budget.recentPtyOutputMaxCharacters ??
      DEFAULT_AI_CONTEXT_BUDGET.recentPtyOutputMaxCharacters,
    recentVisibleOutputMaxCharacters:
      budget.recentVisibleOutputMaxCharacters ??
      DEFAULT_AI_CONTEXT_BUDGET.recentVisibleOutputMaxCharacters,
    currentScreenMaxCharacters:
      budget.currentScreenMaxCharacters ??
      DEFAULT_AI_CONTEXT_BUDGET.currentScreenMaxCharacters,
    recentScreenChangesLimit:
      budget.recentScreenChangesLimit ??
      DEFAULT_AI_CONTEXT_BUDGET.recentScreenChangesLimit,
    recentTimelineEventsLimit:
      budget.recentTimelineEventsLimit ??
      DEFAULT_AI_CONTEXT_BUDGET.recentTimelineEventsLimit,
  };

  for (const [name, value] of Object.entries(resolved)) {
    assertNonNegativeSafeInteger(value, name);
  }

  return resolved;
}

function getRequiredScreenSnapshot(
  timeline: AiContextTimelineSource,
  snapshotId: string,
): ScreenSnapshotEntry {
  const snapshot = timeline.getScreenSnapshot(snapshotId);

  if (!snapshot) {
    throw new Error(
      `Screen snapshot ${JSON.stringify(snapshotId)} was not found.`,
    );
  }

  return snapshot;
}

function formatCurrentContextForTool(
  contextPackage: AiTerminalContextPackage,
): string {
  const context = contextPackage.dynamicContext;
  const lines = [
    `Koshell context ${contextPackage.contractVersion}`,
    `Primary source: ${context.primarySource}`,
    `Alternate screen: ${String(context.altScreen)}`,
    `Current snapshot: ${context.currentSnapshotId ?? "none"}`,
    `Recent screen changes: ${String(context.recentScreenChanges.length)}`,
    `Notable events: ${String(context.notableEvents.length)}`,
    "",
    context.primaryText,
  ];

  return lines.join("\n").trimEnd();
}

function formatScreenChangesForTool(changes: TerminalScreenChange[]): string {
  if (changes.length === 0) {
    return "No recent screen changes.";
  }

  return changes
    .map(
      (change) =>
        `${change.previousSnapshotId ?? "unknown"} -> ${change.snapshotId}: ${change.summary}${change.largeChange ? " (large)" : ""}`,
    )
    .join("\n");
}

function formatTimelineEventsForTool(events: AiTimelineEventSummary[]): string {
  if (events.length === 0) {
    return "No recent timeline events.";
  }

  return events
    .map(
      (event) =>
        `${event.id} @ ${String(event.ts)} ${event.type}: ${event.summary}`,
    )
    .join("\n");
}

function trimStartToMaxCharacters(text: string, maxCharacters: number): string {
  assertNonNegativeSafeInteger(maxCharacters, "maxCharacters");

  if (text.length <= maxCharacters) {
    return text;
  }

  return text.slice(text.length - maxCharacters);
}

function truncateForSummary(text: string): string {
  const normalized = text.replaceAll("\r", "\\r").replaceAll("\n", "\\n");

  if (normalized.length <= 120) {
    return normalized;
  }

  return `${normalized.slice(0, 117)}...`;
}

function assertNonNegativeSafeInteger(
  value: unknown,
  name: string,
): asserts value is number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) {
    throw new Error(`${name} must be a non-negative safe integer.`);
  }
}

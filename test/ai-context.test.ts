import { describe, expect, it } from "vitest";
import {
  AI_CONTEXT_CONTRACT_VERSION,
  AI_CONTEXT_TOOL_CATALOG_VERSION,
  buildAiTerminalContextPackage,
  createKoshellContextTools,
  executeKoshellContextTool,
  getAiContextCachePolicy,
  getKoshellContextToolNames,
} from "../src/ai-context.ts";
import { InMemoryTimelineStore } from "../src/timeline.ts";

function createTimeline(): InMemoryTimelineStore {
  const timeline = new InMemoryTimelineStore({ now: () => 1 });

  timeline.record({ type: "human_input", data: "vim file.txt\r" });
  timeline.record({ type: "pty_output", data: "one\ntwo" });
  timeline.record({ type: "visible_output", text: "visible text" });
  timeline.record({
    type: "screen_snapshot",
    snapshotId: "snapshot-1",
    rows: 4,
    columns: 80,
    altScreen: false,
    screen: "one",
  });
  timeline.record({
    type: "screen_snapshot",
    snapshotId: "snapshot-2",
    rows: 4,
    columns: 80,
    altScreen: true,
    screen: "one\ntwo\nthree",
    previousSnapshotId: "snapshot-1",
    diff: { addedLines: 2, removedLines: 0, changedLines: 2 },
  });

  return timeline;
}

describe("AI context contract", () => {
  it("exposes a stable cache policy and AgentTool catalog", () => {
    const timeline = createTimeline();
    const tools = createKoshellContextTools(timeline);

    expect(getAiContextCachePolicy()).toEqual({
      stablePrefixVersion: "koshell_ai_stable_prefix_v1",
      toolCatalogVersion: AI_CONTEXT_TOOL_CATALOG_VERSION,
      dynamicContextPlacement: "suffix",
      keepToolCatalogStable: true,
      appendRuntimeStateOnly: true,
      cacheRetentionHint: "short",
    });
    expect(getKoshellContextToolNames()).toEqual([
      "koshell_get_current_context",
      "koshell_get_screen_snapshot",
      "koshell_diff_screen_snapshots",
      "koshell_list_recent_screen_changes",
      "koshell_get_recent_timeline_events",
    ]);
    expect(tools.map((tool) => tool.name)).toEqual(
      getKoshellContextToolNames(),
    );
    expect(
      tools.map((tool) => ({
        name: tool.name,
        label: tool.label,
        parameterType: tool.parameters.type,
      })),
    ).toEqual([
      {
        name: "koshell_get_current_context",
        label: "Get Current Terminal Context",
        parameterType: "object",
      },
      {
        name: "koshell_get_screen_snapshot",
        label: "Get Screen Snapshot",
        parameterType: "object",
      },
      {
        name: "koshell_diff_screen_snapshots",
        label: "Diff Screen Snapshots",
        parameterType: "object",
      },
      {
        name: "koshell_list_recent_screen_changes",
        label: "List Recent Screen Changes",
        parameterType: "object",
      },
      {
        name: "koshell_get_recent_timeline_events",
        label: "Get Recent Timeline Events",
        parameterType: "object",
      },
    ]);
  });

  it("keeps the dynamic context shape stable between empty and active terminals", () => {
    const emptyPackage = buildAiTerminalContextPackage(
      new InMemoryTimelineStore({ now: () => 1 }),
    );
    const activePackage = buildAiTerminalContextPackage(createTimeline());

    expect(emptyPackage.contractVersion).toBe(AI_CONTEXT_CONTRACT_VERSION);
    expect(Object.keys(activePackage.dynamicContext)).toEqual(
      Object.keys(emptyPackage.dynamicContext),
    );
    expect(emptyPackage.dynamicContext).toMatchObject({
      primaryText: "",
      primarySource: "empty",
      currentScreen: null,
      currentSnapshotId: null,
      altScreen: false,
      screenRows: null,
      screenColumns: null,
      recentScreenChanges: [],
      notableEvents: [],
    });
    expect(activePackage.dynamicContext).toMatchObject({
      primaryText: "one\ntwo\nthree",
      primarySource: "screen_snapshot",
      currentScreen: "one\ntwo\nthree",
      currentSnapshotId: "snapshot-2",
      altScreen: true,
      screenRows: 4,
      screenColumns: 80,
    });
  });

  it("exposes notable events and follow-up tool calls without changing the tool catalog", () => {
    const contextPackage = buildAiTerminalContextPackage(createTimeline());

    expect(contextPackage.dynamicContext.notableEvents).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          type: "large_screen_change",
          snapshotId: "snapshot-2",
          previousSnapshotId: "snapshot-1",
        }),
        expect.objectContaining({
          type: "alternate_screen_active",
          snapshotId: "snapshot-2",
        }),
      ]),
    );
    expect(contextPackage.dynamicContext.availableFollowups).toEqual(
      expect.arrayContaining([
        {
          toolName: "koshell_get_screen_snapshot",
          reason:
            "Fetch the current full screen snapshot if the summary is insufficient.",
          input: { snapshotId: "snapshot-2" },
        },
        {
          toolName: "koshell_diff_screen_snapshots",
          reason:
            "Inspect the detailed line-level diff for a large screen change.",
          input: {
            fromSnapshotId: "snapshot-1",
            toSnapshotId: "snapshot-2",
          },
        },
      ]),
    );
  });

  it("honors context budgets", () => {
    const contextPackage = buildAiTerminalContextPackage(createTimeline(), {
      primaryTextMaxCharacters: 5,
      recentInputMaxCharacters: 3,
      recentPtyOutputMaxCharacters: 4,
      recentVisibleOutputMaxCharacters: 7,
      currentScreenMaxCharacters: 5,
      recentScreenChangesLimit: 0,
    });

    expect(contextPackage.dynamicContext).toMatchObject({
      primaryText: "three",
      recentInput: "xt\r",
      recentPtyOutput: "\ntwo",
      recentVisibleOutput: "le text",
      currentScreen: "three",
      recentScreenChanges: [],
    });
  });
});

describe("executeKoshellContextTool", () => {
  it("executes context tools and returns AgentToolResult details", async () => {
    const timeline = createTimeline();

    await expect(
      executeKoshellContextTool(timeline, "koshell_get_screen_snapshot", {
        snapshotId: "snapshot-2",
      }),
    ).resolves.toMatchObject({
      content: [{ type: "text" }],
      details: {
        snapshot: {
          snapshotId: "snapshot-2",
          rows: 4,
          columns: 80,
          altScreen: true,
          screen: "one\ntwo\nthree",
        },
      },
    });
    await expect(
      executeKoshellContextTool(timeline, "koshell_diff_screen_snapshots", {
        fromSnapshotId: "snapshot-1",
        toSnapshotId: "snapshot-2",
      }),
    ).resolves.toMatchObject({
      content: [{ type: "text" }],
      details: {
        diff: {
          fromSnapshotId: "snapshot-1",
          toSnapshotId: "snapshot-2",
          addedLines: 2,
          removedLines: 0,
          changedLines: 2,
        },
      },
    });
    await expect(
      executeKoshellContextTool(
        timeline,
        "koshell_list_recent_screen_changes",
        { limit: 1 },
      ),
    ).resolves.toMatchObject({
      content: [{ type: "text" }],
      details: {
        changes: [{ snapshotId: "snapshot-2", summary: "+2, -0" }],
      },
    });
    await expect(
      executeKoshellContextTool(
        timeline,
        "koshell_get_recent_timeline_events",
        {
          limit: 2,
        },
      ),
    ).resolves.toMatchObject({
      content: [{ type: "text" }],
      details: {
        events: [
          {
            type: "screen_snapshot",
            summary: "Screen snapshot snapshot-1.",
          },
          {
            type: "screen_snapshot",
            summary: "Screen snapshot snapshot-2 in alternate screen.",
          },
        ],
      },
    });
  });

  it("validates tool input at the agent-runtime boundary", async () => {
    const timeline = createTimeline();

    await expect(
      executeKoshellContextTool(timeline, "koshell_get_screen_snapshot", {
        snapshotId: {},
      }),
    ).rejects.toThrow(/snapshotId|Invalid|object/);
    await expect(
      executeKoshellContextTool(
        timeline,
        "koshell_get_recent_timeline_events",
        { limit: -1 },
      ),
    ).rejects.toThrow(/limit/);
    await expect(
      executeKoshellContextTool(timeline, "koshell_get_current_context", {
        unsupported: true,
      }),
    ).rejects.toThrow(/unsupported|Unexpected|additional/i);
  });
});

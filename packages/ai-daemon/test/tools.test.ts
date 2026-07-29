import { describe, expect, it } from "bun:test";

import type { KoshellConfig } from "../src/config.ts";
import { createLogger, type Logger } from "../src/logging.ts";
import { ToolBridge } from "../src/tool-bridge.ts";
import {
  type AnnounceTool,
  createCommandTools,
  createCustomTools,
  createWebSearchTool,
} from "../src/tools.ts";

interface Announced {
  toolName: string;
  phase: "started" | "failed";
  message: string;
}

function recorder(): { announce: AnnounceTool; announced: Announced[] } {
  const announced: Announced[] = [];
  return { announce: (activity) => announced.push(activity), announced };
}

function collectingLogger(): { log: Logger; lines: string[] } {
  const lines: string[] = [];
  return {
    log: createLogger("debug", (line) => lines.push(line)),
    lines,
  };
}

function config(search?: KoshellConfig["search"]): KoshellConfig {
  return {
    model: "anthropic/claude-sonnet-4-5",
    providers: {},
    ...(search !== undefined ? { search } : {}),
  };
}

describe("createWebSearchTool", () => {
  // The capability boundary: absent config means the tool never exists, so the
  // agent is never told it can search.
  it("registers nothing when [search] is absent", () => {
    const { log } = collectingLogger();
    expect(createWebSearchTool(config(), {}, log)).toBeUndefined();
  });

  it("registers nothing and warns when the credential is missing", () => {
    const { log, lines } = collectingLogger();
    const tool = createWebSearchTool(
      config({ provider: "exa", max_results: 5 }),
      {},
      log,
    );
    expect(tool).toBeUndefined();
    expect(lines.join("\n")).toContain("web search disabled");
  });

  it("registers web_search when configured with a key", () => {
    const { log } = collectingLogger();
    const tool = createWebSearchTool(
      config({ provider: "exa", api_key: "k", max_results: 5 }),
      {},
      log,
    );
    expect(tool?.name).toBe("web_search");
  });

  it("accepts the credential from the environment", () => {
    const { log } = collectingLogger();
    const tool = createWebSearchTool(
      config({ provider: "exa", max_results: 5 }),
      { EXA_API_KEY: "exa-key" },
      log,
    );
    expect(tool?.name).toBe("web_search");
  });

  // A search failure must reach the model as ordinary evidence, so it can still
  // answer from the terminal context instead of aborting the turn.
  it("returns a failed search as a normal result carrying a stable code", async () => {
    const { log } = collectingLogger();
    const tool = createWebSearchTool(
      config({ provider: "exa", api_key: "k", max_results: 5 }),
      {},
      log,
    );
    if (tool === undefined) {
      throw new Error("web_search should be registered with a key present");
    }

    const originalFetch = globalThis.fetch;
    globalThis.fetch = (() =>
      Promise.resolve(
        new Response("{}", { status: 401 }),
      )) as unknown as typeof fetch;
    try {
      const result = await tool.execute(
        "call-1",
        { query: "anything" },
        undefined,
        undefined,
        // The web_search tool never touches the extension context.
        undefined as never,
      );
      const text = result.content
        .map((part) => ("text" in part ? part.text : ""))
        .join("");
      expect(text).toContain("search_unauthorized");
      expect(result.details).toMatchObject({ code: "search_unauthorized" });
    } finally {
      globalThis.fetch = originalFetch;
    }
  });
});

// web_search runs entirely inside the daemon and never reaches the terminal, so
// without these announcements the user sees nothing at all while it runs.
describe("tool activity announcements", () => {
  it("announces the search query before running it", async () => {
    const { log } = collectingLogger();
    const { announce, announced } = recorder();
    const tool = createWebSearchTool(
      config({ provider: "exa", api_key: "k", max_results: 5 }),
      {},
      log,
      announce,
    );
    if (tool === undefined) {
      throw new Error("web_search should be registered");
    }

    const originalFetch = globalThis.fetch;
    globalThis.fetch = (() =>
      Promise.resolve(
        new Response(JSON.stringify({ results: [] }), {
          status: 200,
          headers: { "content-type": "application/json" },
        }),
      )) as unknown as typeof fetch;
    try {
      await tool.execute(
        "call-1",
        { query: "brew shallow clone error" },
        undefined,
        undefined,
        undefined as never,
      );
    } finally {
      globalThis.fetch = originalFetch;
    }

    expect(announced).toHaveLength(1);
    expect(announced[0]).toMatchObject({
      toolName: "web_search",
      phase: "started",
    });
    // The query is the one thing leaving the machine; showing it is the point.
    expect(announced[0]?.message).toContain("brew shallow clone error");
    expect(announced[0]?.message).toContain("exa");
  });

  it("bounds a long query rather than flooding the terminal", async () => {
    const { log } = collectingLogger();
    const { announce, announced } = recorder();
    const tool = createWebSearchTool(
      config({ provider: "exa", api_key: "k", max_results: 5 }),
      {},
      log,
      announce,
    );
    if (tool === undefined) {
      throw new Error("web_search should be registered");
    }

    const originalFetch = globalThis.fetch;
    globalThis.fetch = (() =>
      Promise.resolve(
        new Response(JSON.stringify({ results: [] }), {
          status: 200,
          headers: { "content-type": "application/json" },
        }),
      )) as unknown as typeof fetch;
    try {
      await tool.execute(
        "call-1",
        { query: "x".repeat(500) },
        undefined,
        undefined,
        undefined as never,
      );
    } finally {
      globalThis.fetch = originalFetch;
    }

    expect(announced[0]?.message.length).toBeLessThan(200);
    expect(announced[0]?.message).toContain("…");
  });

  it("announces a failed search so silence never reads as success", async () => {
    const { log } = collectingLogger();
    const { announce, announced } = recorder();
    const tool = createWebSearchTool(
      config({ provider: "exa", api_key: "k", max_results: 5 }),
      {},
      log,
      announce,
    );
    if (tool === undefined) {
      throw new Error("web_search should be registered");
    }

    const originalFetch = globalThis.fetch;
    globalThis.fetch = (() =>
      Promise.resolve(
        new Response("{}", { status: 429 }),
      )) as unknown as typeof fetch;
    try {
      await tool.execute(
        "call-1",
        { query: "q" },
        undefined,
        undefined,
        undefined as never,
      );
    } finally {
      globalThis.fetch = originalFetch;
    }

    expect(announced.map((entry) => entry.phase)).toEqual([
      "started",
      "failed",
    ]);
    expect(announced[1]?.message).toContain("search_rate_limited");
  });

  it("announces each command read with the id and page it is fetching", async () => {
    const { log } = collectingLogger();
    const { announce, announced } = recorder();
    const bridge = new ToolBridge({ send: () => undefined, log });
    const [list, read] = createCommandTools(bridge, log, announce);
    if (list === undefined || read === undefined) {
      throw new Error("both command tools should be created");
    }

    // No active request, so the bridge settles immediately with request_inactive —
    // enough to exercise both the started and failed announcements.
    await list.execute("c1", {}, undefined, undefined, undefined as never);
    await read.execute(
      "c2",
      { commandId: "command-3", offset: 8000 },
      undefined,
      undefined,
      undefined as never,
    );

    const started = announced.filter((entry) => entry.phase === "started");
    expect(started[0]?.message).toContain("recent commands");
    expect(started[1]?.message).toContain("command-3");
    expect(started[1]?.message).toContain("8000");

    // Every settled failure is reported, including ones the bridge synthesizes.
    const failed = announced.filter((entry) => entry.phase === "failed");
    expect(failed).toHaveLength(2);
    expect(failed[0]?.message).toContain("request_inactive");
  });

  it("stays silent when no announcer is wired", async () => {
    const { log } = collectingLogger();
    const bridge = new ToolBridge({ send: () => undefined, log });
    const [list] = createCommandTools(bridge, log);
    if (list === undefined) {
      throw new Error("the list tool should be created");
    }
    // Must not throw on the optional-call path.
    await list.execute("c1", {}, undefined, undefined, undefined as never);
  });
});

describe("createCustomTools", () => {
  it("is empty without configured capabilities", () => {
    const { log } = collectingLogger();
    expect(createCustomTools({ config: config(), env: {}, log })).toEqual([]);
  });

  it("collects the configured tools", () => {
    const { log } = collectingLogger();
    const tools = createCustomTools({
      config: config({ provider: "exa", api_key: "k", max_results: 5 }),
      env: {},
      log,
    });
    expect(tools.map((tool) => tool.name)).toEqual(["web_search"]);
  });

  // Capability negotiation: without a bridge the terminal cannot serve a tool call,
  // so the command readers must not exist for that conversation.
  it("omits the command tools when the terminal advertised no capability", () => {
    const { log } = collectingLogger();
    const tools = createCustomTools({ config: config(), env: {}, log });
    expect(tools.map((tool) => tool.name)).not.toContain(
      "list_recent_commands",
    );
  });

  it("adds the command tools when a bridge is present", () => {
    const { log } = collectingLogger();
    const bridge = new ToolBridge({
      send: () => undefined,
      log,
    });
    const tools = createCustomTools({ config: config(), env: {}, log, bridge });
    expect(tools.map((tool) => tool.name)).toEqual([
      "list_recent_commands",
      "read_command_output",
    ]);
  });
});

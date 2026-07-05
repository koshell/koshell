import { describe, expect, it } from "bun:test";

import { createLogger, resolveLogLevel } from "../src/logging.ts";

describe("resolveLogLevel", () => {
  it("prefers the --log-level argument over the environment", () => {
    expect(
      resolveLogLevel(["--log-level", "debug"], { KOSHELL_LOG: "warn" }),
    ).toBe("debug");
    expect(
      resolveLogLevel(["--log-level=error"], { KOSHELL_LOG: "warn" }),
    ).toBe("error");
  });

  it("falls back to KOSHELL_LOG and then to info", () => {
    expect(resolveLogLevel([], { KOSHELL_LOG: "debug" })).toBe("debug");
    expect(resolveLogLevel([], {})).toBe("info");
    expect(resolveLogLevel([], { KOSHELL_LOG: "not-a-level" })).toBe("info");
  });
});

describe("createLogger", () => {
  it("drops messages below the configured level and tags the rest", () => {
    const lines: string[] = [];
    const log = createLogger("warn", (line) => lines.push(line));
    log.error("boom");
    log.warn("careful");
    log.info("hello");
    log.debug("details");
    expect(lines).toEqual(["error: boom", "warn: careful"]);
  });

  it("silences everything at level off", () => {
    const lines: string[] = [];
    const log = createLogger("off", (line) => lines.push(line));
    log.error("boom");
    expect(lines).toEqual([]);
  });
});

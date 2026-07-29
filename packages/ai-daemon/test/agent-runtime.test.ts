import { describe, expect, it } from "bun:test";

import {
  assertModelSwitchCapacity,
  configurationFingerprint,
} from "../src/agent-runtime.ts";

describe("model switch safety", () => {
  it("accepts retained context with the bounded response reserve", () => {
    expect(() => {
      assertModelSwitchCapacity("test/large", 128_000, 32_000, 100_000);
    }).not.toThrow();
  });

  it("rejects a smaller target before mutating the session", () => {
    expect(() => {
      assertModelSwitchCapacity("test/small", 64_000, 16_000, 50_000);
    }).toThrow(/retained conversation needs about 50000 tokens/);
  });

  it("rejects unknown retained usage instead of assuming it fits", () => {
    expect(() => {
      assertModelSwitchCapacity("test/unknown", 128_000, 4_096, null);
    }).toThrow(/usage is unknown/);
  });

  it("excludes only the root model from the reload construction fingerprint", () => {
    const providers = { test: { api_key: "x" } };
    expect(
      configurationFingerprint({
        model: "test/one",
        thinking_level: "high",
        providers,
      }),
    ).toBe(
      configurationFingerprint({
        model: "test/two",
        thinking_level: "high",
        providers,
      }),
    );
    expect(
      configurationFingerprint({
        model: "test/two",
        thinking_level: "low",
        providers,
      }),
    ).not.toBe(
      configurationFingerprint({
        model: "test/two",
        thinking_level: "high",
        providers,
      }),
    );
  });

  // AGENTS.md is baked into the system prompt at session creation, so `koshell reload`
  // has to rebuild after an edit rather than switch in place — otherwise the file looks
  // live but the running conversation keeps the text it started with.
  it("treats edited user instructions as a construction change", () => {
    const config = {
      model: "test/one",
      thinking_level: "high" as const,
      providers: {},
    };
    const instructions = {
      path: "/config/AGENTS.md",
      text: "Be blunt.",
      truncated: false,
    };
    expect(configurationFingerprint(config, instructions)).not.toBe(
      configurationFingerprint(config, undefined),
    );
    expect(configurationFingerprint(config, instructions)).not.toBe(
      configurationFingerprint(config, { ...instructions, text: "Be gentle." }),
    );
    // Rewriting the file to identical bytes is not worth losing a transcript over,
    // which is why the text is compared rather than an mtime.
    expect(configurationFingerprint(config, instructions)).toBe(
      configurationFingerprint(config, {
        ...instructions,
        path: "/elsewhere/AGENTS.md",
      }),
    );
  });
});

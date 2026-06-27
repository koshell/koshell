import { chmodSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import {
  assertNotNestedKoshell,
  createPtyEnv,
  isNestedKoshell,
  resolveShell,
} from "../src/shell.ts";

describe("resolveShell", () => {
  it("uses an executable absolute SHELL value", () => {
    const directory = mkdtempSync(join(tmpdir(), "koshell-shell-"));
    const shellPath = join(directory, "shell");

    try {
      writeFileSync(shellPath, "#!/bin/sh\nexit 0\n");
      chmodSync(shellPath, 0o755);

      expect(resolveShell({ SHELL: shellPath, PATH: "" })).toBe(shellPath);
    } finally {
      rmSync(directory, { force: true, recursive: true });
    }
  });

  it("resolves a shell name through PATH", () => {
    const directory = mkdtempSync(join(tmpdir(), "koshell-shell-"));
    const shellPath = join(directory, "mock-sh");

    try {
      writeFileSync(shellPath, "#!/bin/sh\nexit 0\n");
      chmodSync(shellPath, 0o755);

      expect(resolveShell({ SHELL: "mock-sh", PATH: directory })).toBe(
        shellPath,
      );
    } finally {
      rmSync(directory, { force: true, recursive: true });
    }
  });
});

describe("nested koshell detection", () => {
  it("detects the child-shell marker", () => {
    expect(isNestedKoshell({ KOSHELL: "1" })).toBe(true);
    expect(isNestedKoshell({ KOSHELL: "" })).toBe(false);
    expect(isNestedKoshell({})).toBe(false);
  });

  it("throws before starting inside an existing koshell", () => {
    expect(() => {
      assertNotNestedKoshell({ KOSHELL: "1" });
    }).toThrow("koshell is already running");
  });
});

describe("createPtyEnv", () => {
  it("keeps only string environment values and marks the child as koshell", () => {
    const env = createPtyEnv({
      EMPTY: "",
      KEEP: "yes",
      PATH: "",
      DROP: undefined,
    });

    expect(env.KEEP).toBe("yes");
    expect(env.EMPTY).toBe("");
    expect(env.DROP).toBeUndefined();
    expect(env.KOSHELL).toBe("1");
    expect(env.PATH).not.toBe("");
  });
});

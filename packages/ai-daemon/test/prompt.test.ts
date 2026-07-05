import { describe, expect, it } from "bun:test";

import { buildUserPrompt } from "../src/prompt.ts";

const SESSION = {
  cwd: "/home/user/project",
  shell: "/bin/zsh",
  rows: 24,
  cols: 80,
};

const FULL_PACKAGE = {
  contractVersion: "koshell_ai_context_v1",
  question: "why did ls fail",
  trigger: {
    form: "inline",
    completion: "marker",
    stillRunning: false,
    exitCode: 1,
  },
  dynamicContext: {
    primaryText: "ls: /nope: No such file or directory",
    primarySource: "visible_output",
    currentScreen: "$ ls /nope\nls: /nope: No such file or directory\n$",
    screenRows: 24,
    screenColumns: 80,
    altScreen: false,
    recentInput: "ls /nope\r",
  },
};

describe("buildUserPrompt", () => {
  it("renders the full package with all sections", () => {
    const prompt = buildUserPrompt(
      { question: "why did ls fail", context_package: FULL_PACKAGE },
      SESSION,
    );
    expect(prompt).toContain("Question: why did ls fail");
    expect(prompt).toContain("- form: inline");
    expect(prompt).toContain("- completion confidence: marker");
    expect(prompt).toContain("- command still running: no");
    expect(prompt).toContain("- exit code: 1");
    expect(prompt).toContain("- cwd: /home/user/project");
    expect(prompt).toContain("- size: 80x24");
    expect(prompt).toContain(
      "Primary terminal context (source: visible_output",
    );
    expect(prompt).toContain("ls: /nope: No such file or directory");
    expect(prompt).toContain("Current screen (80x24):");
    expect(prompt).toContain("Recent typed input");
  });

  it("annotates a still-running command and a missing exit code", () => {
    const prompt = buildUserPrompt(
      {
        question: "what is it doing",
        context_package: {
          trigger: { form: "inline", stillRunning: true },
          dynamicContext: {
            primaryText: "compiling...",
            primarySource: "pty_output",
          },
        },
      },
      SESSION,
    );
    expect(prompt).toContain(
      "- command still running: yes — the output below may be incomplete and still growing",
    );
    expect(prompt).toContain("- exit code: not captured");
  });

  it("omits empty sections instead of rendering them blank", () => {
    const prompt = buildUserPrompt(
      {
        question: "q",
        context_package: {
          trigger: {},
          dynamicContext: { primaryText: "", recentInput: "" },
        },
      },
      undefined,
    );
    expect(prompt).not.toContain("Primary terminal context");
    expect(prompt).not.toContain("Current screen");
    expect(prompt).not.toContain("Recent typed input");
    expect(prompt).not.toContain("Terminal session:");
  });

  it("marks the alternate screen", () => {
    const prompt = buildUserPrompt(
      {
        question: "q",
        context_package: {
          dynamicContext: {
            currentScreen: "vim buffer",
            screenRows: 24,
            screenColumns: 80,
            altScreen: true,
          },
        },
      },
      undefined,
    );
    expect(prompt).toContain("Current screen (80x24, alternate screen):");
  });

  it("degrades gracefully on a malformed package", () => {
    for (const broken of [null, undefined, 42, "text"]) {
      const prompt = buildUserPrompt(
        { question: "why", context_package: broken },
        SESSION,
      );
      expect(prompt).toContain("Question: why");
      expect(prompt).toContain(
        "(terminal context was not available for this request)",
      );
    }
  });

  it("substitutes a default diagnose phrasing for an empty question", () => {
    const prompt = buildUserPrompt(
      { question: "", context_package: FULL_PACKAGE },
      SESSION,
    );
    expect(prompt).toContain(
      "Question: (no explicit question — diagnose what just happened in this terminal)",
    );
  });
});

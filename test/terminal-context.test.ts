import { describe, expect, it } from "vitest";
import { buildTerminalContext } from "../src/terminal-context.ts";
import { InMemoryTimelineStore } from "../src/timeline.ts";

describe("buildTerminalContext", () => {
  it("uses recent PTY output as the primary text in a normal shell context", () => {
    const timeline = new InMemoryTimelineStore({ now: () => 1 });

    timeline.record({ type: "human_input", data: "echo hello\r" });
    timeline.record({ type: "pty_output", data: "hello\r\n" });
    timeline.record({
      type: "screen_snapshot",
      snapshotId: "snapshot-1",
      rows: 24,
      columns: 80,
      altScreen: false,
      screen: "hello",
    });

    expect(buildTerminalContext(timeline)).toMatchObject({
      recentInput: "echo hello\r",
      recentPtyOutput: "hello\r\n",
      recentVisibleOutput: "",
      currentScreen: "hello",
      altScreen: false,
      primaryText: "hello\r\n",
      primarySource: "pty_output",
      screenRows: 24,
      screenColumns: 80,
    });
  });

  it("prefers visible output over raw PTY output when visible output exists", () => {
    const timeline = new InMemoryTimelineStore({ now: () => 1 });

    timeline.record({ type: "pty_output", data: "\u001B[31mred\u001B[0m" });
    timeline.record({ type: "visible_output", text: "red" });

    expect(buildTerminalContext(timeline)).toMatchObject({
      recentPtyOutput: "\u001B[31mred\u001B[0m",
      recentVisibleOutput: "red",
      primaryText: "red",
      primarySource: "visible_output",
    });
  });

  it("prefers current screen snapshots in alternate screen mode", () => {
    const timeline = new InMemoryTimelineStore({ now: () => 1 });

    timeline.record({ type: "human_input", data: "vim file.txt\r" });
    timeline.record({
      type: "pty_output",
      data: "\u001B[?1049h\u001B[2Jfile contents\u001B[?25l",
    });
    timeline.record({
      type: "screen_snapshot",
      snapshotId: "snapshot-1",
      rows: 24,
      columns: 80,
      altScreen: true,
      screen: "file contents",
    });

    expect(buildTerminalContext(timeline)).toMatchObject({
      recentInput: "vim file.txt\r",
      altScreen: true,
      currentScreen: "file contents",
      primaryText: "file contents",
      primarySource: "screen_snapshot",
    });
  });

  it("does not expose hidden human input in recent input", () => {
    const timeline = new InMemoryTimelineStore({ now: () => 1 });

    timeline.record({ type: "human_input", data: "visible\r" });
    timeline.record({ type: "human_input", data: "secret", visible: false });

    expect(buildTerminalContext(timeline).recentInput).toBe("visible\r");
  });

  it("honors character limits", () => {
    const timeline = new InMemoryTimelineStore({ now: () => 1 });

    timeline.record({ type: "human_input", data: "abcdef" });
    timeline.record({ type: "pty_output", data: "123456" });
    timeline.record({ type: "visible_output", text: "uvwxyz" });
    timeline.record({
      type: "screen_snapshot",
      snapshotId: "snapshot-1",
      rows: 24,
      columns: 80,
      altScreen: true,
      screen: "screen-text",
    });

    expect(
      buildTerminalContext(timeline, {
        recentInputMaxCharacters: 3,
        recentPtyOutputMaxCharacters: 2,
        recentVisibleOutputMaxCharacters: 4,
        currentScreenMaxCharacters: 4,
      }),
    ).toMatchObject({
      recentInput: "def",
      recentPtyOutput: "56",
      recentVisibleOutput: "wxyz",
      currentScreen: "text",
      primaryText: "text",
    });
  });
});

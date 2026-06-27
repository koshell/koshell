import { describe, expect, it } from "vitest";
import { InMemoryTimelineStore } from "../src/timeline.ts";

describe("InMemoryTimelineStore", () => {
  it("records events with deterministic ids and timestamps", () => {
    let currentTime = 1_000;
    const timeline = new InMemoryTimelineStore({
      now: () => currentTime,
    });

    const first = timeline.record({ type: "human_input", data: "ls\r" });
    currentTime = 1_500;
    const second = timeline.record({
      type: "pty_output",
      data: "file.txt\r\n",
    });

    expect(first).toEqual({
      id: "event-1",
      event: { type: "human_input", ts: 1_000, data: "ls\r" },
    });
    expect(second).toEqual({
      id: "event-2",
      event: { type: "pty_output", ts: 1_500, data: "file.txt\r\n" },
    });
    expect(timeline.listEvents()).toEqual([first.event, second.event]);
  });

  it("returns recent terminal text and PTY output with character limits", () => {
    const timeline = new InMemoryTimelineStore({ now: () => 1 });

    timeline.record({ type: "human_input", data: "echo hello\r" });
    timeline.record({ type: "pty_output", data: "hello\r\n" });
    timeline.record({ type: "visible_output", text: "visible" });
    timeline.record({ type: "human_input", data: "secret", visible: false });

    expect(timeline.getRecentText()).toBe("echo hello\rhello\r\nvisible");
    expect(timeline.getRecentText(5)).toBe("sible");
    expect(timeline.getRecentPtyOutput()).toBe("hello\r\n");
  });

  it("tracks the latest screen snapshot and can reset entries", () => {
    const timeline = new InMemoryTimelineStore({ now: () => 1 });

    timeline.record({
      type: "screen_snapshot",
      snapshotId: "snapshot-1",
      rows: 24,
      columns: 80,
      altScreen: false,
      screen: "first",
    });
    timeline.record({
      type: "screen_snapshot",
      snapshotId: "snapshot-2",
      rows: 30,
      columns: 100,
      altScreen: false,
      screen: "second",
    });

    expect(timeline.getLatestScreenSnapshot()?.event.snapshotId).toBe(
      "snapshot-2",
    );

    timeline.reset();

    expect(timeline.listEntries()).toEqual([]);
    expect(timeline.getLatestScreenSnapshot()).toBeUndefined();
  });

  it("rejects invalid recent-text limits", () => {
    const timeline = new InMemoryTimelineStore();

    expect(() => timeline.getRecentText(-1)).toThrow(
      "maxCharacters must be a non-negative safe integer",
    );
  });
});

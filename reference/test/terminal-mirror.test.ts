import { describe, expect, it } from "vitest";
import { TerminalMirror } from "../src/terminal-mirror.ts";

describe("TerminalMirror", () => {
  it("keeps an xterm-backed copy of PTY output", async () => {
    const mirror = new TerminalMirror(20, 5);

    try {
      await mirror.write("hello\r\nworld");

      const snapshot = mirror.getSnapshot();

      expect(snapshot.columns).toBe(20);
      expect(snapshot.rows).toBe(5);
      expect(snapshot.altScreen).toBe(false);
      expect(snapshot.screen).toContain("hello");
      expect(snapshot.screen).toContain("world");
      expect(mirror.serialize()).toContain("hello");
    } finally {
      mirror.dispose();
    }
  });

  it("tracks alternate screen snapshots", async () => {
    const mirror = new TerminalMirror(20, 5);

    try {
      await mirror.write("normal");
      expect(mirror.getSnapshot()).toMatchObject({
        altScreen: false,
        screen: "normal",
      });

      await mirror.write("\u001B[?1049h\u001B[2J\u001B[Halternate");
      expect(mirror.getSnapshot()).toMatchObject({
        altScreen: true,
        screen: "alternate",
      });

      await mirror.write("\u001B[?1049l");
      expect(mirror.getSnapshot()).toMatchObject({
        altScreen: false,
        screen: "normal",
      });
    } finally {
      mirror.dispose();
    }
  });

  it("resizes the mirrored terminal", () => {
    const mirror = new TerminalMirror(20, 5);

    try {
      mirror.resize(100, 30);

      expect(mirror.getSnapshot()).toMatchObject({
        columns: 100,
        rows: 30,
      });
    } finally {
      mirror.dispose();
    }
  });
});

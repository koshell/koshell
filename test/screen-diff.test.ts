import { describe, expect, it } from "vitest";
import { diffScreenText, summarizeScreenDiff } from "../src/screen-diff.ts";

describe("screen diff", () => {
  it("reports no changes for identical screens", () => {
    expect(summarizeScreenDiff("one\ntwo", "one\ntwo")).toEqual({
      addedLines: 0,
      removedLines: 0,
      changedLines: 0,
    });
    expect(diffScreenText("one\ntwo", "one\ntwo").hunks).toEqual([]);
  });

  it("counts added and removed lines", () => {
    expect(
      summarizeScreenDiff("one\ntwo\nthree", "zero\none\nthree\nfour"),
    ).toEqual({
      addedLines: 2,
      removedLines: 1,
      changedLines: 3,
    });
  });

  it("returns detailed hunks for on-demand snapshot diffs", () => {
    expect(diffScreenText("one\ntwo\nthree", "one\nTWO\nthree\nfour")).toEqual({
      addedLines: 2,
      removedLines: 1,
      changedLines: 3,
      hunks: [
        {
          oldStart: 2,
          oldLines: 1,
          newStart: 2,
          newLines: 1,
          lines: [
            { type: "removed", line: "two", oldLineNumber: 2 },
            { type: "added", line: "TWO", newLineNumber: 2 },
          ],
        },
        {
          oldStart: 4,
          oldLines: 0,
          newStart: 4,
          newLines: 1,
          lines: [{ type: "added", line: "four", newLineNumber: 4 }],
        },
      ],
    });
  });

  it("treats an empty screen as no lines", () => {
    expect(diffScreenText("", "first")).toMatchObject({
      addedLines: 1,
      removedLines: 0,
      changedLines: 1,
    });
    expect(diffScreenText("first", "")).toMatchObject({
      addedLines: 0,
      removedLines: 1,
      changedLines: 1,
    });
  });
});

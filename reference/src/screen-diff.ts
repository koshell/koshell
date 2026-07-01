import { diffArrays } from "diff";
import type { ChangeObject } from "diff";

export interface ScreenDiffSummary {
  addedLines: number;
  removedLines: number;
  changedLines: number;
}

export interface ScreenDiffHunkLine {
  type: "added" | "removed";
  line: string;
  oldLineNumber?: number;
  newLineNumber?: number;
}

export interface ScreenDiffHunk {
  oldStart: number;
  oldLines: number;
  newStart: number;
  newLines: number;
  lines: ScreenDiffHunkLine[];
}

export interface ScreenTextDiff extends ScreenDiffSummary {
  hunks: ScreenDiffHunk[];
}

type LineChange = ChangeObject<string[]>;

export function summarizeScreenDiff(
  before: string,
  after: string,
): ScreenDiffSummary {
  const diff = diffScreenText(before, after);

  return {
    addedLines: diff.addedLines,
    removedLines: diff.removedLines,
    changedLines: diff.changedLines,
  };
}

export function diffScreenText(before: string, after: string): ScreenTextDiff {
  const changes = diffArrays(splitScreenLines(before), splitScreenLines(after));
  const addedLines = countChangedLines(changes, "added");
  const removedLines = countChangedLines(changes, "removed");

  return {
    addedLines,
    removedLines,
    changedLines: addedLines + removedLines,
    hunks: buildHunks(changes),
  };
}

function splitScreenLines(screen: string): string[] {
  return screen.length === 0 ? [] : screen.split("\n");
}

function countChangedLines(
  changes: LineChange[],
  type: "added" | "removed",
): number {
  return changes
    .filter((change) => isChangeType(change, type))
    .reduce((total, change) => total + change.value.length, 0);
}

function buildHunks(changes: LineChange[]): ScreenDiffHunk[] {
  const hunks: ScreenDiffHunk[] = [];
  let pendingLines: ScreenDiffHunkLine[] = [];
  let oldStart: number | undefined;
  let newStart: number | undefined;
  let oldLines = 0;
  let newLines = 0;
  let oldLineNumber = 1;
  let newLineNumber = 1;

  const flush = (): void => {
    if (pendingLines.length === 0) {
      return;
    }

    hunks.push({
      oldStart: oldStart ?? oldLineNumber,
      oldLines,
      newStart: newStart ?? newLineNumber,
      newLines,
      lines: pendingLines,
    });
    pendingLines = [];
    oldStart = undefined;
    newStart = undefined;
    oldLines = 0;
    newLines = 0;
  };

  for (const change of changes) {
    if (change.added) {
      oldStart ??= oldLineNumber;
      newStart ??= newLineNumber;

      for (const line of change.value) {
        pendingLines.push({
          type: "added",
          line,
          newLineNumber,
        });
        newLineNumber += 1;
        newLines += 1;
      }
      continue;
    }

    if (change.removed) {
      oldStart ??= oldLineNumber;
      newStart ??= newLineNumber;

      for (const line of change.value) {
        pendingLines.push({
          type: "removed",
          line,
          oldLineNumber,
        });
        oldLineNumber += 1;
        oldLines += 1;
      }
      continue;
    }

    flush();
    oldLineNumber += change.value.length;
    newLineNumber += change.value.length;
  }

  flush();

  return hunks;
}

function isChangeType(change: LineChange, type: "added" | "removed"): boolean {
  return type === "added" ? change.added : change.removed;
}

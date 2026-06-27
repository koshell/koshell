import type { ScreenSnapshotEntry, TerminalEvent } from "./timeline.ts";

export type TerminalContextPrimarySource =
  | "screen_snapshot"
  | "visible_output"
  | "pty_output"
  | "empty";

export interface TerminalScreenChange {
  snapshotId: string;
  previousSnapshotId?: string;
  altScreen: boolean;
  rows: number;
  addedLines: number;
  removedLines: number;
  changedLines: number;
  largeChange: boolean;
  summary: string;
}

export interface TerminalContext {
  recentInput: string;
  recentPtyOutput: string;
  recentVisibleOutput: string;
  recentScreenChanges: TerminalScreenChange[];
  altScreen: boolean;
  primaryText: string;
  primarySource: TerminalContextPrimarySource;
  currentScreen?: string;
  screenRows?: number;
  screenColumns?: number;
}

export interface TerminalContextOptions {
  recentInputMaxCharacters?: number;
  recentPtyOutputMaxCharacters?: number;
  recentVisibleOutputMaxCharacters?: number;
  currentScreenMaxCharacters?: number;
  recentScreenChangesLimit?: number;
}

export interface TerminalContextTimelineSource {
  listEvents(): TerminalEvent[];
  listScreenSnapshots(): ScreenSnapshotEntry[];
  getRecentPtyOutput(maxCharacters?: number): string;
  getLatestScreenSnapshot(): ScreenSnapshotEntry | undefined;
}

const DEFAULT_RECENT_INPUT_MAX_CHARACTERS = 2_000;
const DEFAULT_RECENT_PTY_OUTPUT_MAX_CHARACTERS = 8_000;
const DEFAULT_RECENT_VISIBLE_OUTPUT_MAX_CHARACTERS = 8_000;
const DEFAULT_CURRENT_SCREEN_MAX_CHARACTERS = 8_000;
const DEFAULT_RECENT_SCREEN_CHANGES_LIMIT = 20;

export function buildTerminalContext(
  timeline: TerminalContextTimelineSource,
  options: TerminalContextOptions = {},
): TerminalContext {
  const events = timeline.listEvents();
  const latestSnapshot = timeline.getLatestScreenSnapshot()?.event;
  const recentInput = getRecentInput(
    events,
    options.recentInputMaxCharacters ?? DEFAULT_RECENT_INPUT_MAX_CHARACTERS,
  );
  const recentVisibleOutput = getRecentVisibleOutput(
    events,
    options.recentVisibleOutputMaxCharacters ??
      DEFAULT_RECENT_VISIBLE_OUTPUT_MAX_CHARACTERS,
  );
  const recentPtyOutput = timeline.getRecentPtyOutput(
    options.recentPtyOutputMaxCharacters ??
      DEFAULT_RECENT_PTY_OUTPUT_MAX_CHARACTERS,
  );
  const currentScreen = latestSnapshot?.screen
    ? trimStartToMaxCharacters(
        latestSnapshot.screen,
        options.currentScreenMaxCharacters ??
          DEFAULT_CURRENT_SCREEN_MAX_CHARACTERS,
      )
    : undefined;
  const altScreen = latestSnapshot?.altScreen ?? false;
  const recentScreenChanges = getRecentScreenChanges(
    timeline.listScreenSnapshots(),
    options.recentScreenChangesLimit ?? DEFAULT_RECENT_SCREEN_CHANGES_LIMIT,
  );
  const primary = choosePrimaryText({
    altScreen,
    currentScreen,
    recentVisibleOutput,
    recentPtyOutput,
  });
  const context: TerminalContext = {
    recentInput,
    recentPtyOutput,
    recentVisibleOutput,
    recentScreenChanges,
    altScreen,
    primaryText: primary.text,
    primarySource: primary.source,
  };

  if (currentScreen !== undefined) {
    context.currentScreen = currentScreen;
  }

  if (latestSnapshot) {
    context.screenRows = latestSnapshot.rows;
    context.screenColumns = latestSnapshot.columns;
  }

  return context;
}

function getRecentInput(
  events: TerminalEvent[],
  maxCharacters: number,
): string {
  const text = events
    .map((event) =>
      event.type === "human_input" && event.visible !== false ? event.data : "",
    )
    .join("");

  return trimStartToMaxCharacters(text, maxCharacters);
}

function getRecentVisibleOutput(
  events: TerminalEvent[],
  maxCharacters: number,
): string {
  const text = events
    .map((event) => (event.type === "visible_output" ? event.text : ""))
    .join("");

  return trimStartToMaxCharacters(text, maxCharacters);
}

function getRecentScreenChanges(
  snapshots: ScreenSnapshotEntry[],
  limit: number,
): TerminalScreenChange[] {
  if (!Number.isSafeInteger(limit) || limit < 0) {
    throw new Error(
      "recentScreenChangesLimit must be a non-negative safe integer.",
    );
  }

  return snapshots
    .filter((snapshot) => snapshot.event.diff !== undefined)
    .slice(-limit)
    .map((snapshot) => {
      const diff = snapshot.event.diff;

      if (!diff) {
        throw new Error("Screen snapshot diff was unexpectedly missing.");
      }

      const change: TerminalScreenChange = {
        snapshotId: snapshot.event.snapshotId,
        altScreen: snapshot.event.altScreen,
        rows: snapshot.event.rows,
        addedLines: diff.addedLines,
        removedLines: diff.removedLines,
        changedLines: diff.changedLines,
        largeChange: diff.changedLines >= Math.ceil(snapshot.event.rows / 2),
        summary: `+${String(diff.addedLines)}, -${String(diff.removedLines)}`,
      };

      if (snapshot.event.previousSnapshotId !== undefined) {
        change.previousSnapshotId = snapshot.event.previousSnapshotId;
      }

      return change;
    });
}

function choosePrimaryText(input: {
  altScreen: boolean;
  currentScreen: string | undefined;
  recentVisibleOutput: string;
  recentPtyOutput: string;
}): { text: string; source: TerminalContextPrimarySource } {
  if (input.altScreen && hasText(input.currentScreen)) {
    return { text: input.currentScreen, source: "screen_snapshot" };
  }

  if (hasText(input.recentVisibleOutput)) {
    return { text: input.recentVisibleOutput, source: "visible_output" };
  }

  if (!input.altScreen && hasText(input.recentPtyOutput)) {
    return { text: input.recentPtyOutput, source: "pty_output" };
  }

  if (hasText(input.currentScreen)) {
    return { text: input.currentScreen, source: "screen_snapshot" };
  }

  return { text: "", source: "empty" };
}

function hasText(value: string | undefined): value is string {
  return value !== undefined && value.trim().length > 0;
}

function trimStartToMaxCharacters(text: string, maxCharacters: number): string {
  if (!Number.isSafeInteger(maxCharacters) || maxCharacters < 0) {
    throw new Error("maxCharacters must be a non-negative safe integer.");
  }

  if (text.length <= maxCharacters) {
    return text;
  }

  return text.slice(text.length - maxCharacters);
}

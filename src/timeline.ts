import { diffScreenText } from "./screen-diff.ts";
import type { ScreenDiffSummary, ScreenTextDiff } from "./screen-diff.ts";

export type ControlMode = "human" | "shared" | "ai";

export type TerminalEvent =
  | { type: "human_input"; ts: number; data: string; visible?: boolean }
  | { type: "pty_output"; ts: number; data: string }
  | { type: "visible_output"; ts: number; text: string }
  | {
      type: "command_start";
      ts: number;
      commandId: string;
      command: string;
      cwd?: string;
    }
  | {
      type: "command_end";
      ts: number;
      commandId: string;
      command: string;
      exitCode?: number;
      durationMs?: number;
    }
  | {
      type: "screen_snapshot";
      ts: number;
      snapshotId: string;
      rows: number;
      columns: number;
      altScreen: boolean;
      screen?: string;
      previousSnapshotId?: string;
      diff?: ScreenDiffSummary;
    }
  | {
      type: "ai_request";
      ts: number;
      requestId: string;
      question: string;
      trigger: "#?";
    }
  | { type: "ai_response"; ts: number; requestId: string; text: string }
  | {
      type: "control_mode_change";
      ts: number;
      from: ControlMode;
      to: ControlMode;
      reason: string;
    };

export type UntimedTerminalEvent = TerminalEvent extends infer Event
  ? Event extends { ts: number }
    ? Omit<Event, "ts"> & { ts?: number }
    : never
  : never;

export interface TimelineEntry {
  id: string;
  event: TerminalEvent;
}

export type ScreenSnapshotEntry = TimelineEntry & {
  event: Extract<TerminalEvent, { type: "screen_snapshot" }>;
};

export interface TimelineRecorder {
  record(event: UntimedTerminalEvent): TimelineEntry;
}

export interface ScreenSnapshotDiff extends ScreenTextDiff {
  fromSnapshotId: string;
  toSnapshotId: string;
}

export interface TimelineStoreOptions {
  now?: () => number;
  createId?: () => string;
}

export class InMemoryTimelineStore implements TimelineRecorder {
  private readonly entries: TimelineEntry[] = [];
  private readonly now: () => number;
  private readonly createId: () => string;
  private nextId = 1;

  constructor(options: TimelineStoreOptions = {}) {
    this.now = options.now ?? Date.now;
    this.createId =
      options.createId ??
      (() => {
        const id = `event-${String(this.nextId)}`;
        this.nextId += 1;
        return id;
      });
  }

  append(event: TerminalEvent): TimelineEntry {
    const entry = { id: this.createId(), event };
    this.entries.push(entry);
    return entry;
  }

  record(event: UntimedTerminalEvent): TimelineEntry {
    return this.append({
      ...event,
      ts: event.ts ?? this.now(),
    });
  }

  listEntries(): TimelineEntry[] {
    return [...this.entries];
  }

  listEvents(): TerminalEvent[] {
    return this.entries.map((entry) => entry.event);
  }

  getRecentText(maxCharacters = 8_000): string {
    const text = this.entries
      .map((entry) => eventToText(entry.event))
      .filter((textChunk) => textChunk.length > 0)
      .join("");

    return trimStartToMaxCharacters(text, maxCharacters);
  }

  getRecentPtyOutput(maxCharacters = 8_000): string {
    const text = this.entries
      .map((entry) =>
        entry.event.type === "pty_output" ? entry.event.data : "",
      )
      .join("");

    return trimStartToMaxCharacters(text, maxCharacters);
  }

  listScreenSnapshots(): ScreenSnapshotEntry[] {
    return this.entries.filter(isScreenSnapshotEntry);
  }

  getScreenSnapshot(snapshotId: string): ScreenSnapshotEntry | undefined {
    return this.listScreenSnapshots().find(
      (entry) => entry.event.snapshotId === snapshotId,
    );
  }

  getLatestScreenSnapshot(): ScreenSnapshotEntry | undefined {
    return findLatestScreenSnapshot(this.entries, () => true);
  }

  getLatestAlternateScreenSnapshot(): ScreenSnapshotEntry | undefined {
    return findLatestScreenSnapshot(
      this.entries,
      (entry) => entry.event.altScreen,
    );
  }

  diffScreenSnapshots(
    fromSnapshotId: string,
    toSnapshotId: string,
  ): ScreenSnapshotDiff {
    const fromSnapshot = this.getRequiredScreenSnapshot(fromSnapshotId);
    const toSnapshot = this.getRequiredScreenSnapshot(toSnapshotId);

    return {
      fromSnapshotId,
      toSnapshotId,
      ...diffScreenText(
        fromSnapshot.event.screen ?? "",
        toSnapshot.event.screen ?? "",
      ),
    };
  }

  reset(): void {
    this.entries.length = 0;
  }

  private getRequiredScreenSnapshot(snapshotId: string): ScreenSnapshotEntry {
    const snapshot = this.getScreenSnapshot(snapshotId);

    if (!snapshot) {
      throw new Error(
        `Screen snapshot ${JSON.stringify(snapshotId)} was not found.`,
      );
    }

    return snapshot;
  }
}

function findLatestScreenSnapshot(
  entries: TimelineEntry[],
  predicate: (entry: ScreenSnapshotEntry) => boolean,
): ScreenSnapshotEntry | undefined {
  for (let index = entries.length - 1; index >= 0; index -= 1) {
    const entry = entries[index];

    if (entry && isScreenSnapshotEntry(entry) && predicate(entry)) {
      return entry;
    }
  }

  return undefined;
}

function eventToText(event: TerminalEvent): string {
  switch (event.type) {
    case "human_input":
      return event.visible === false ? "" : event.data;
    case "pty_output":
      return event.data;
    case "visible_output":
      return event.text;
    case "screen_snapshot":
      return event.screen ?? "";
    case "ai_request":
      return event.question;
    case "ai_response":
      return event.text;
    case "command_start":
    case "command_end":
    case "control_mode_change":
      return "";
  }
}

function isScreenSnapshotEntry(
  entry: TimelineEntry,
): entry is ScreenSnapshotEntry {
  return entry.event.type === "screen_snapshot";
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

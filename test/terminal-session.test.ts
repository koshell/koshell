import { describe, expect, it } from "vitest";
import type { TerminalSnapshot } from "../src/terminal-mirror.ts";
import type {
  Disposable,
  PtyExitEvent,
  PtyProcess,
  TerminalMirrorLike,
} from "../src/terminal-session.ts";
import { TerminalSession } from "../src/terminal-session.ts";
import { InMemoryTimelineStore } from "../src/timeline.ts";

class FakePtyProcess implements PtyProcess {
  readonly writes: string[] = [];
  readonly resizes: { columns: number; rows: number }[] = [];
  readonly kills: (string | undefined)[] = [];
  private readonly dataHandlers = new Set<(data: string) => void>();
  private readonly exitHandlers = new Set<(event: PtyExitEvent) => void>();

  onData(callback: (data: string) => void): Disposable {
    this.dataHandlers.add(callback);

    return {
      dispose: () => {
        this.dataHandlers.delete(callback);
      },
    };
  }

  onExit(callback: (event: PtyExitEvent) => void): Disposable {
    this.exitHandlers.add(callback);

    return {
      dispose: () => {
        this.exitHandlers.delete(callback);
      },
    };
  }

  write(data: string): void {
    this.writes.push(data);
  }

  resize(columns: number, rows: number): void {
    this.resizes.push({ columns, rows });
  }

  kill(signal?: string): void {
    this.kills.push(signal);
  }

  emitData(data: string): void {
    for (const handler of this.dataHandlers) {
      handler(data);
    }
  }

  emitExit(event: PtyExitEvent): void {
    for (const handler of this.exitHandlers) {
      handler(event);
    }
  }
}

class FakeTerminalMirror implements TerminalMirrorLike {
  readonly writes: string[] = [];
  readonly resizes: { columns: number; rows: number }[] = [];
  private columns = 80;
  private rows = 24;
  disposed = false;

  write(data: string): Promise<void> {
    this.writes.push(data);
    return Promise.resolve();
  }

  resize(columns: number, rows: number): void {
    this.columns = columns;
    this.rows = rows;
    this.resizes.push({ columns, rows });
  }

  getSnapshot(): TerminalSnapshot {
    return {
      timestamp: "2026-06-27T12:55:50.000Z",
      rows: this.rows,
      columns: this.columns,
      cursorX: 0,
      cursorY: 0,
      altScreen: false,
      screen: this.writes.join(""),
    };
  }

  dispose(): void {
    this.disposed = true;
  }
}

class SequencedTerminalMirror implements TerminalMirrorLike {
  readonly writes: string[] = [];
  private readonly pendingWrites: {
    resolve: () => void;
    reject: (error: unknown) => void;
  }[] = [];

  get pendingWriteCount(): number {
    return this.pendingWrites.length;
  }

  write(data: string): Promise<void> {
    this.writes.push(data);

    return new Promise((resolve, reject) => {
      this.pendingWrites.push({ resolve, reject });
    });
  }

  resize(): void {
    return;
  }

  getSnapshot(): TerminalSnapshot {
    return {
      timestamp: "2026-06-27T12:55:50.000Z",
      rows: 24,
      columns: 80,
      cursorX: 0,
      cursorY: 0,
      altScreen: false,
      screen: this.writes.join(""),
    };
  }

  dispose(): void {
    return;
  }

  resolveNextWrite(): void {
    const pendingWrite = this.pendingWrites.shift();

    if (!pendingWrite) {
      throw new Error("No pending write to resolve.");
    }

    pendingWrite.resolve();
  }

  rejectNextWrite(error: unknown): void {
    const pendingWrite = this.pendingWrites.shift();

    if (!pendingWrite) {
      throw new Error("No pending write to reject.");
    }

    pendingWrite.reject(error);
  }
}

async function waitForPendingWrites(
  mirror: SequencedTerminalMirror,
  expectedCount: number,
): Promise<void> {
  for (let attempt = 0; attempt < 10; attempt += 1) {
    if (mirror.pendingWriteCount === expectedCount) {
      return;
    }

    await Promise.resolve();
  }

  expect(mirror.pendingWriteCount).toBe(expectedCount);
}

describe("TerminalSession", () => {
  it("mirrors PTY output while forwarding it to the real output writer", async () => {
    const ptyProcess = new FakePtyProcess();
    const mirror = new FakeTerminalMirror();
    const outputChunks: string[] = [];
    const session = new TerminalSession({
      ptyProcess,
      mirror,
      output: {
        write: (data) => {
          outputChunks.push(data);
        },
      },
    });

    session.start();
    ptyProcess.emitData("hello");
    await session.flushOutput();

    expect(mirror.writes).toEqual(["hello"]);
    expect(outputChunks).toEqual(["hello"]);
  });

  it("forwards input, resize, kill, exit, and dispose operations", () => {
    const ptyProcess = new FakePtyProcess();
    const mirror = new FakeTerminalMirror();
    const exitEvents: PtyExitEvent[] = [];
    const session = new TerminalSession({
      ptyProcess,
      mirror,
      output: { write: () => undefined },
    });

    session.start((event) => {
      exitEvents.push(event);
    });
    session.writeInput("ls\r");
    session.resize(100, 30);
    session.kill("SIGTERM");
    ptyProcess.emitExit({ exitCode: 7 });
    session.dispose();

    expect(ptyProcess.writes).toEqual(["ls\r"]);
    expect(ptyProcess.resizes).toEqual([{ columns: 100, rows: 30 }]);
    expect(mirror.resizes).toEqual([{ columns: 100, rows: 30 }]);
    expect(ptyProcess.kills).toEqual(["SIGTERM"]);
    expect(exitEvents).toEqual([{ exitCode: 7 }]);
    expect(mirror.disposed).toBe(true);
  });

  it("records human input and PTY output in the timeline", async () => {
    const ptyProcess = new FakePtyProcess();
    const mirror = new FakeTerminalMirror();
    const timeline = new InMemoryTimelineStore({ now: () => 123 });
    const session = new TerminalSession({
      ptyProcess,
      mirror,
      output: { write: () => undefined },
      timeline,
    });

    session.start();
    session.writeInput("pwd\r");
    ptyProcess.emitData("/tmp\r\n");
    await session.flushOutput();

    expect(timeline.listEvents()).toEqual([
      { type: "human_input", ts: 123, data: "pwd\r" },
      { type: "pty_output", ts: 123, data: "/tmp\r\n" },
      {
        type: "screen_snapshot",
        ts: 123,
        snapshotId: "snapshot-1",
        rows: 24,
        columns: 80,
        altScreen: false,
        screen: "/tmp\r\n",
      },
    ]);
  });

  it("serializes mirror writes and reports mirror failures", async () => {
    const ptyProcess = new FakePtyProcess();
    const mirror = new SequencedTerminalMirror();
    const outputChunks: string[] = [];
    const outputErrors: unknown[] = [];
    const session = new TerminalSession({
      ptyProcess,
      mirror,
      output: {
        write: (data) => {
          outputChunks.push(data);
        },
      },
      onOutputError: (error) => {
        outputErrors.push(error);
      },
    });

    session.start();
    ptyProcess.emitData("first");
    ptyProcess.emitData("second");
    await waitForPendingWrites(mirror, 1);

    expect(outputChunks).toEqual(["first", "second"]);

    mirror.resolveNextWrite();
    await waitForPendingWrites(mirror, 1);

    const failure = new Error("mirror failed");
    mirror.rejectNextWrite(failure);
    await session.flushOutput();

    expect(mirror.writes).toEqual(["first", "second"]);
    expect(outputErrors).toEqual([failure]);
  });

  it("removes PTY listeners when disposed", async () => {
    const ptyProcess = new FakePtyProcess();
    const mirror = new FakeTerminalMirror();
    const outputChunks: string[] = [];
    const session = new TerminalSession({
      ptyProcess,
      mirror,
      output: {
        write: (data) => {
          outputChunks.push(data);
        },
      },
    });

    session.start();
    session.dispose();
    ptyProcess.emitData("late");
    await session.flushOutput();

    expect(outputChunks).toEqual([]);
    expect(mirror.writes).toEqual([]);
  });
});

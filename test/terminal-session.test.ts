import { describe, expect, it } from "vitest";
import type {
  Disposable,
  PtyExitEvent,
  PtyProcess,
  TerminalMirrorLike,
} from "../src/terminal-session.ts";
import { TerminalSession } from "../src/terminal-session.ts";

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
  disposed = false;

  write(data: string): Promise<void> {
    this.writes.push(data);
    return Promise.resolve();
  }

  resize(columns: number, rows: number): void {
    this.resizes.push({ columns, rows });
  }

  dispose(): void {
    this.disposed = true;
  }
}

describe("TerminalSession", () => {
  it("mirrors PTY output while forwarding it to the real output writer", () => {
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

  it("removes PTY listeners when disposed", () => {
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

    expect(outputChunks).toEqual([]);
    expect(mirror.writes).toEqual([]);
  });
});

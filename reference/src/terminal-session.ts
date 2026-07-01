import process from "node:process";
import pty from "node-pty";
import { summarizeScreenDiff } from "./screen-diff.ts";
import { assertNotNestedKoshell, createPtyEnv, resolveShell } from "./shell.ts";
import { TerminalMirror } from "./terminal-mirror.ts";
import type { TerminalSnapshot } from "./terminal-mirror.ts";
import type { UntimedTerminalEvent, TimelineRecorder } from "./timeline.ts";

export interface TerminalMirrorLike {
  write(data: string): Promise<void>;
  resize(columns: number, rows: number): void;
  getSnapshot(): TerminalSnapshot;
  dispose(): void;
}

const DEFAULT_COLUMNS = 80;
const DEFAULT_ROWS = 24;

export interface Disposable {
  dispose(): void;
}

export interface PtyExitEvent {
  exitCode: number;
  signal?: number;
}

export interface PtyProcess {
  onData(callback: (data: string) => void): Disposable;
  onExit(callback: (event: PtyExitEvent) => void): Disposable;
  write(data: string): void;
  resize(columns: number, rows: number): void;
  kill(signal?: string): void;
}

export interface OutputWriter {
  write(data: string): unknown;
}

export interface TerminalSessionOptions {
  ptyProcess: PtyProcess;
  mirror: TerminalMirrorLike;
  output: OutputWriter;
  timeline?: TimelineRecorder;
  onOutputError?: (error: unknown) => void;
}

export interface SpawnTerminalShellOptions {
  columns: number;
  rows: number;
  cwd?: string;
  env?: NodeJS.ProcessEnv;
  shell?: string;
}

export interface SpawnedTerminalShell {
  ptyProcess: PtyProcess;
  shell: string;
}

export class TerminalSession {
  private readonly ptyProcess: PtyProcess;
  private readonly mirror: TerminalMirrorLike;
  private readonly output: OutputWriter;
  private readonly timeline: TimelineRecorder | undefined;
  private readonly onOutputError: ((error: unknown) => void) | undefined;
  private readonly disposables: Disposable[] = [];
  private outputQueue = Promise.resolve();
  private nextSnapshotId = 1;
  private previousSnapshot: { snapshotId: string; screen: string } | undefined;
  private running = false;

  constructor(options: TerminalSessionOptions) {
    this.ptyProcess = options.ptyProcess;
    this.mirror = options.mirror;
    this.output = options.output;
    this.timeline = options.timeline;
    this.onOutputError = options.onOutputError;
  }

  start(onExit?: (event: PtyExitEvent) => void): void {
    if (this.running) {
      return;
    }

    this.running = true;
    this.disposables.push(
      this.ptyProcess.onData((data) => {
        this.handleOutput(data);
      }),
      this.ptyProcess.onExit((event) => {
        onExit?.(event);
      }),
    );
  }

  writeInput(data: string): void {
    this.timeline?.record({ type: "human_input", data });
    this.ptyProcess.write(data);
  }

  async flushOutput(): Promise<void> {
    await this.outputQueue;
  }

  resize(columns: number, rows: number): void {
    this.ptyProcess.resize(columns, rows);
    this.mirror.resize(columns, rows);
    this.recordScreenSnapshot();
  }

  kill(signal?: string): void {
    this.ptyProcess.kill(signal);
  }

  dispose(): void {
    while (this.disposables.length > 0) {
      this.disposables.pop()?.dispose();
    }

    this.mirror.dispose();
    this.running = false;
  }

  private handleOutput(data: string): void {
    this.timeline?.record({ type: "pty_output", data });
    this.output.write(data);
    this.outputQueue = this.outputQueue
      .then(async () => {
        await this.mirror.write(data);
        this.recordScreenSnapshot();
      })
      .catch((error: unknown) => {
        this.reportOutputError(error);
      });
  }

  private recordScreenSnapshot(): void {
    if (!this.timeline) {
      return;
    }

    const snapshot = this.mirror.getSnapshot();
    const snapshotId = `snapshot-${String(this.nextSnapshotId)}`;
    this.nextSnapshotId += 1;

    const event: UntimedTerminalEvent = {
      type: "screen_snapshot",
      snapshotId,
      rows: snapshot.rows,
      columns: snapshot.columns,
      altScreen: snapshot.altScreen,
      screen: snapshot.screen,
    };

    if (this.previousSnapshot) {
      event.previousSnapshotId = this.previousSnapshot.snapshotId;
      event.diff = summarizeScreenDiff(
        this.previousSnapshot.screen,
        snapshot.screen,
      );
    }

    this.timeline.record(event);
    this.previousSnapshot = { snapshotId, screen: snapshot.screen };
  }

  private reportOutputError(error: unknown): void {
    if (this.onOutputError) {
      this.onOutputError(error);
      return;
    }

    const message = error instanceof Error ? error.message : String(error);
    process.stderr.write(`koshell mirror write failed: ${message}\n`);
  }
}

export function spawnTerminalShell(
  options: SpawnTerminalShellOptions,
): SpawnedTerminalShell {
  const sourceEnv = options.env ?? process.env;
  assertNotNestedKoshell(sourceEnv);

  const shell = options.shell ?? resolveShell(sourceEnv);

  try {
    const ptyProcess = pty.spawn(shell, [], {
      name: process.env.TERM ?? "xterm-256color",
      cols: options.columns,
      rows: options.rows,
      cwd: options.cwd ?? process.cwd(),
      env: createPtyEnv(sourceEnv),
    });

    return { ptyProcess, shell };
  } catch (error: unknown) {
    const message = error instanceof Error ? error.message : String(error);
    throw new Error(
      `Failed to spawn shell ${JSON.stringify(shell)} in ${JSON.stringify(options.cwd ?? process.cwd())}: ${message}`,
      { cause: error },
    );
  }
}

export function runInteractiveTerminalShell(): void {
  assertNotNestedKoshell();

  if (!process.stdin.isTTY || !process.stdout.isTTY) {
    throw new Error("koshell must be started from an interactive TTY.");
  }

  const columns = process.stdout.columns || DEFAULT_COLUMNS;
  const rows = process.stdout.rows || DEFAULT_ROWS;
  const mirror = new TerminalMirror(columns, rows);
  const { ptyProcess } = spawnTerminalShell({ columns, rows });
  const session = new TerminalSession({
    ptyProcess,
    mirror,
    output: process.stdout,
  });
  let exiting = false;

  const cleanup = (): void => {
    if (exiting) {
      return;
    }

    exiting = true;
    process.stdin.setRawMode(false);
    process.stdin.pause();
    process.stdin.off("data", handleInput);
    process.stdout.off("resize", resize);
    session.dispose();
  };

  const resize = (): void => {
    session.resize(
      process.stdout.columns || DEFAULT_COLUMNS,
      process.stdout.rows || DEFAULT_ROWS,
    );
  };

  process.stdin.setRawMode(true);
  process.stdin.resume();
  process.stdin.on("data", handleInput);
  process.stdout.on("resize", resize);

  session.start(({ exitCode }) => {
    cleanup();
    process.exit(exitCode);
  });

  function handleInput(chunk: Buffer): void {
    session.writeInput(chunk.toString("utf8"));
  }

  for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"] as const) {
    process.once(signal, () => {
      cleanup();
      session.kill(signal);
    });
  }
}

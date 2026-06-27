import { createRequire } from "node:module";
import type { SerializeAddon as XtermSerializeAddon } from "@xterm/addon-serialize";
import type { Terminal as XtermTerminal } from "@xterm/headless";

const require = createRequire(import.meta.url);
const { Terminal } =
  require("@xterm/headless") as typeof import("@xterm/headless");
const { SerializeAddon } =
  require("@xterm/addon-serialize") as typeof import("@xterm/addon-serialize");

export interface TerminalSnapshot {
  timestamp: string;
  rows: number;
  columns: number;
  cursorX: number;
  cursorY: number;
  screen: string;
}

export class TerminalMirror {
  private readonly terminal: XtermTerminal;
  private readonly serializeAddon: XtermSerializeAddon;

  constructor(columns: number, rows: number) {
    this.terminal = new Terminal({
      allowProposedApi: true,
      cols: columns,
      rows,
      scrollback: 2_000,
    });
    this.serializeAddon = new SerializeAddon();
    this.terminal.loadAddon(this.serializeAddon);
  }

  write(data: string): Promise<void> {
    if (data.length === 0) {
      return Promise.resolve();
    }

    return new Promise((resolve) => {
      this.terminal.write(data, resolve);
    });
  }

  resize(columns: number, rows: number): void {
    this.terminal.resize(columns, rows);
  }

  getSnapshot(): TerminalSnapshot {
    const buffer = this.terminal.buffer.active;
    const lines: string[] = [];

    for (let row = 0; row < this.terminal.rows; row += 1) {
      const line = buffer.getLine(buffer.viewportY + row);
      lines.push(line?.translateToString(true) ?? "");
    }

    return {
      timestamp: new Date().toISOString(),
      rows: this.terminal.rows,
      columns: this.terminal.cols,
      cursorX: buffer.cursorX,
      cursorY: buffer.cursorY,
      screen: lines.join("\n").trimEnd(),
    };
  }

  serialize(): string {
    return this.serializeAddon.serialize();
  }

  dispose(): void {
    this.serializeAddon.dispose();
    this.terminal.dispose();
  }
}

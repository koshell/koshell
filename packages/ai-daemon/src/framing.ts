// Incremental newline-delimited JSON decoder. Buffers partial lines across chunks so a
// message split across socket reads is reassembled before parsing.
export class NdjsonDecoder {
  private buffer = "";

  push(chunk: string): string[] {
    this.buffer += chunk;
    const lines: string[] = [];

    for (;;) {
      const index = this.buffer.indexOf("\n");
      if (index === -1) {
        break;
      }
      const line = this.buffer.slice(0, index);
      this.buffer = this.buffer.slice(index + 1);
      if (line.length > 0) {
        lines.push(line);
      }
    }

    return lines;
  }
}

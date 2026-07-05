import { describe, expect, it } from "bun:test";

import { NdjsonDecoder } from "../src/framing.ts";

describe("NdjsonDecoder", () => {
  it("splits multiple complete lines", () => {
    const decoder = new NdjsonDecoder();
    expect(decoder.push('{"a":1}\n{"b":2}\n')).toEqual(['{"a":1}', '{"b":2}']);
  });

  it("reassembles a line split across chunks", () => {
    const decoder = new NdjsonDecoder();
    expect(decoder.push('{"a":')).toEqual([]);
    expect(decoder.push("1}\n")).toEqual(['{"a":1}']);
  });

  it("ignores empty lines and holds an incomplete tail", () => {
    const decoder = new NdjsonDecoder();
    expect(decoder.push('\n{"a":1}\n{"partial":')).toEqual(['{"a":1}']);
    expect(decoder.push("true}\n")).toEqual(['{"partial":true}']);
  });
});

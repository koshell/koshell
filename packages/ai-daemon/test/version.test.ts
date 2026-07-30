import { describe, expect, it } from "bun:test";

import { resolveBuildVersion, utcStamp } from "../scripts/version.ts";

const NOW = new Date(Date.UTC(2026, 6, 30, 7, 47, 45));

describe("resolveBuildVersion", () => {
  it("prefers an explicit version over the tag and the timestamp", () => {
    expect(
      resolveBuildVersion({ explicit: "1.2.0", tag: "v9.9.9", now: NOW }),
    ).toBe("1.2.0");
  });

  it("falls back to the tag on HEAD, without its leading v", () => {
    expect(resolveBuildVersion({ tag: "v1.2.0", now: NOW })).toBe("1.2.0");
    expect(resolveBuildVersion({ tag: "1.2.0", now: NOW })).toBe("1.2.0");
  });

  it("stamps the build time when there is neither", () => {
    expect(resolveBuildVersion({ now: NOW })).toBe("20260730.074745");
  });

  // An unset variable arrives as "" from a shell, and git prints a trailing newline;
  // neither is a version.
  it("treats blank and whitespace-only sources as absent", () => {
    expect(resolveBuildVersion({ explicit: "", tag: "   ", now: NOW })).toBe(
      "20260730.074745",
    );
    expect(resolveBuildVersion({ explicit: "  1.2.0  \n", now: NOW })).toBe(
      "1.2.0",
    );
    expect(resolveBuildVersion({ tag: "v1.2.0\n", now: NOW })).toBe("1.2.0");
  });
});

describe("utcStamp", () => {
  it("is UTC, zero-padded, and sorts chronologically as text", () => {
    expect(utcStamp(new Date(Date.UTC(2026, 0, 2, 3, 4, 5)))).toBe(
      "20260102.030405",
    );
    expect(utcStamp(new Date(Date.UTC(2026, 11, 31, 23, 59, 59)))).toBe(
      "20261231.235959",
    );
    // The same instant expressed in another zone stamps identically, which is what makes
    // two machines' builds comparable.
    expect(utcStamp(new Date("2026-07-30T15:47:45+08:00"))).toBe(
      "20260730.074745",
    );
    expect(
      utcStamp(new Date(Date.UTC(2026, 0, 2, 3, 4, 5))) <
        utcStamp(new Date(Date.UTC(2026, 0, 2, 3, 4, 6))),
    ).toBe(true);
  });
});

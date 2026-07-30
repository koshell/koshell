// Build-time version resolution for the daemon binary (design 0024).
//
// The same chain `crates/koshell-rs/build.rs` runs for the terminal, so the two artifacts
// of one build carry the same string:
//
//   1. KOSHELL_VERSION — an explicit version for a release build. `make` resolves it once
//      and exports it, which is what keeps koshell and the daemon in step.
//   2. The exact tag on HEAD (`git describe --tags --exact-match`), leading `v` stripped.
//   3. The build's UTC timestamp, YYYYMMDD.HHMMSS — sortable, filename- and tag-safe, and
//      comparable across machines, which a local-time stamp would not be.
//
// Kept out of src/ deliberately: this runs at build time, not in the daemon. What the
// daemon reports is the literal `scripts/build-binary.ts` substitutes into it.

import { spawnSync } from "node:child_process";

export interface VersionSources {
  // KOSHELL_VERSION as it reached the build, if set.
  explicit?: string | undefined;
  // The exact tag on HEAD, if this is a tagged checkout.
  tag?: string | undefined;
  // The build's instant, used only when neither of the above is available.
  now: Date;
}

// The version to stamp into this build.
export function resolveBuildVersion({
  explicit,
  tag,
  now,
}: VersionSources): string {
  const fromEnv = firstLine(explicit);
  if (fromEnv !== undefined) {
    return fromEnv;
  }
  const fromTag = firstLine(tag);
  if (fromTag !== undefined) {
    return fromTag.startsWith("v") ? fromTag.slice(1) : fromTag;
  }
  return utcStamp(now);
}

// An instant as YYYYMMDD.HHMMSS in UTC.
export function utcStamp(now: Date): string {
  const pad = (value: number, width = 2): string =>
    String(value).padStart(width, "0");
  const date =
    pad(now.getUTCFullYear(), 4) +
    pad(now.getUTCMonth() + 1) +
    pad(now.getUTCDate());
  const time =
    pad(now.getUTCHours()) +
    pad(now.getUTCMinutes()) +
    pad(now.getUTCSeconds());
  return `${date}.${time}`;
}

// The exact tag on HEAD, or undefined outside a repository, on an untagged commit, or
// without git — each an ordinary build rather than an error.
export function headTag(cwd: string): string | undefined {
  const result = spawnSync(
    "git",
    ["-C", cwd, "describe", "--tags", "--exact-match"],
    { encoding: "utf8" },
  );
  if (result.status !== 0 || typeof result.stdout !== "string") {
    return undefined;
  }
  return firstLine(result.stdout);
}

// The first non-empty line of `value`, trimmed. A version string ends up on a --version
// line and inside a --define expression, neither of which survives an embedded newline.
function firstLine(value: string | undefined): string | undefined {
  const first = value?.split("\n")[0]?.trim();
  return first === undefined || first === "" ? undefined : first;
}

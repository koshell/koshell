// Compiles the daemon into a single executable with its build version stamped in
// (design 0024).
//
// The version cannot come from package.json at runtime: every build would then report the
// same 0.1.0, and the whole point of `koshell version` is telling one build from another.
// `bun build --define` substitutes the `KOSHELL_BUILD_VERSION` identifier declared in
// index.ts with a string literal while parsing, so the compiled binary carries the version
// it was built with and does not consult the environment it later runs in.
//
// Usage: bun scripts/build-binary.ts   (via `bun run build:binary`)

import { spawnSync } from "node:child_process";
import { dirname, join } from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import { headTag, resolveBuildVersion } from "./version.ts";

const packageDir = dirname(dirname(fileURLToPath(import.meta.url)));
const outfile = join("dist", "koshell-ai-daemon");

const version = resolveBuildVersion({
  explicit: process.env.KOSHELL_VERSION,
  tag: headTag(packageDir),
  now: new Date(),
});

const result = spawnSync(
  "bun",
  [
    "build",
    "--compile",
    join("src", "index.ts"),
    "--outfile",
    outfile,
    "--define",
    `KOSHELL_BUILD_VERSION=${JSON.stringify(version)}`,
  ],
  { cwd: packageDir, stdio: "inherit" },
);

if (result.status !== 0) {
  console.error(
    `koshell-ai-daemon build failed (${result.signal ?? String(result.status)})`,
  );
  process.exit(result.status ?? 1);
}

console.log(`koshell-ai-daemon ${version} -> ${outfile}`);

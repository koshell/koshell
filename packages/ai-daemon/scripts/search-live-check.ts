// Live checks for the web search failure paths, run by bun. Opt-in: this is not part
// of `bun run check`, because one case costs money and another takes ten seconds.
//
// Why this exists separately from test/search.test.ts. Those tests hand a fake fetch a
// status code we chose and assert we map it as intended — they cannot tell us whether
// the live service returns that status code at all. That gap has already produced one
// defect: hand-written fixtures carried `summary` and `text` fields, so a fallback that
// read them looked covered, while the real API returns a content field only when the
// request asked for one, making the whole fallback unreachable. Everything below
// asserts an observable the fixtures cannot reach.
//
// Cases, and what each is worth:
//   unauthorized   a deliberately invalid key against the real endpoint. Free, needs no
//                  credential, and pins down the status Exa actually answers with — the
//                  most common user-facing failure (a typo'd or revoked key).
//   unreachable    a local port with nothing listening. Asserts a refused connection
//                  becomes `search_unreachable` rather than an unhandled rejection.
//   timeout        a local server that accepts and then never answers. The only way to
//                  exercise the real `AbortSignal.timeout` path end to end; a fake fetch
//                  rejecting on demand proves nothing about the timeout itself.
//   success        needs EXA_API_KEY and --paid. Costs roughly $0.007 per run.
//
// Not covered, deliberately: HTTP 402 (credits exhausted or budget exceeded). It cannot
// be produced on demand without an account that is actually out of balance, so it stays
// fixture-covered until one exists. Run this from such an account with --paid to close
// that gap; the success case will fail with `search_payment_required` and that failure
// is the evidence.
//
// Usage: bun scripts/search-live-check.ts [--paid]

import net from "node:net";
import process from "node:process";

import type { SearchConfig } from "../src/config.ts";
import { runSearch, SearchError } from "../src/search.ts";

const paid = process.argv.includes("--paid");

function config(overrides: Partial<SearchConfig> = {}): SearchConfig {
  return { provider: "exa", max_results: 3, ...overrides };
}

// Returns the error code `runSearch` produced, or null when it unexpectedly resolved.
async function codeOf(options: Parameters<typeof runSearch>[0]) {
  try {
    await runSearch(options);
    return null;
  } catch (error) {
    if (error instanceof SearchError) {
      return error.code;
    }
    throw error;
  }
}

let failures = 0;

function report(name: string, ok: boolean, detail: string): void {
  if (!ok) {
    failures += 1;
  }
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}  ${detail}`);
}

// Binds a socket, optionally leaving connections hanging. Returns its port and a close
// handle. A hanging server is what makes the timeout case deterministic: routing a
// request at an unroutable address instead would return "host unreachable" on some
// networks and hang on others, so the assertion would depend on where it ran.
function listen(hang: boolean): Promise<{ port: number; close: () => void }> {
  return new Promise((resolve, reject) => {
    const server = net.createServer((socket) => {
      if (!hang) {
        socket.destroy();
      }
      // Otherwise hold the connection open and answer nothing.
    });
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      if (address === null || typeof address === "string") {
        reject(new Error("could not determine the listening port"));
        return;
      }
      resolve({
        port: address.port,
        close: () => {
          server.close();
          // A hung client keeps the server from closing on its own.
          server.unref();
        },
      });
    });
  });
}

// --- unauthorized: the one case that touches the real endpoint for free -------------
// What leaves this machine: a POST to https://api.exa.ai/search carrying an obviously
// invalid key and the literal query below. No real credential and no terminal content.
{
  const code = await codeOf({
    config: config(),
    apiKey: "koshell-live-check-invalid-key",
    query: "koshell live check",
  });
  report(
    "unauthorized",
    code === "search_unauthorized",
    `invalid key against the live endpoint -> ${String(code)}`,
  );
}

// --- unreachable: connection refused -------------------------------------------------
{
  const server = await listen(false);
  const port = server.port;
  server.close();
  // The port is now free, so the connection is refused rather than accepted.
  const code = await codeOf({
    config: config({ base_url: `http://127.0.0.1:${String(port)}` }),
    apiKey: "k",
    query: "q",
  });
  report(
    "unreachable",
    code === "search_unreachable",
    `refused connection -> ${String(code)}`,
  );
}

// --- timeout: accepted but never answered --------------------------------------------
{
  const server = await listen(true);
  const started = Date.now();
  const code = await codeOf({
    config: config({ base_url: `http://127.0.0.1:${String(server.port)}` }),
    apiKey: "k",
    query: "q",
  });
  const elapsed = Date.now() - started;
  server.close();
  report(
    "timeout",
    code === "timeout",
    `no response for ${String(Math.round(elapsed / 1000))}s -> ${String(code)}`,
  );
}

// --- success: opt-in, billed ----------------------------------------------------------
const apiKey = process.env.EXA_API_KEY;
if (!paid) {
  console.log("SKIP  success  needs --paid (a live search costs about $0.007)");
} else if (apiKey === undefined || apiKey.length === 0) {
  report("success", false, "--paid was given but EXA_API_KEY is not set");
} else {
  try {
    const response = await runSearch({
      config: config(),
      apiKey,
      query: "exa api search highlights",
    });
    // Highlights are the only content field requested, so they are the only one that
    // can carry evidence. A run where every result comes back without one means the
    // request shape drifted, even though nothing errored.
    const withHighlight = response.results.filter(
      (result) => result.snippet.length > 0,
    ).length;
    report(
      "success",
      response.results.length > 0 && withHighlight > 0,
      `${String(response.results.length)} result(s), ${String(withHighlight)} with a highlight, cost $${(response.costDollars ?? 0).toFixed(4)}`,
    );
  } catch (error) {
    const code = error instanceof SearchError ? error.code : String(error);
    report("success", false, `live search failed: ${code}`);
  }
}

console.log(
  failures === 0
    ? "search live check PASS"
    : `search live check FAIL (${String(failures)})`,
);
process.exit(failures === 0 ? 0 : 1);

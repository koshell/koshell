// Web search backends for the Koshell-owned `web_search` tool.
//
// Why Koshell owns this instead of borrowing the model provider's search:
// pi's tool abstraction is function-calling only. `Tool` is `{ name, description,
// parameters }` with no `type` discriminator, and every API adapter converts it to a
// plain function schema (see `convertTools` in pi-ai's anthropic-messages adapter), so
// there is no path for Anthropic's `web_search_20250305`, OpenAI Responses'
// `web_search`, or Gemini's `google_search` grounding tool to reach the wire. pi also
// ships no MCP client, so an external search MCP server is not an escape hatch either.
//
// A dedicated search API is also the only option that stays independent of which of
// pi's 30+ providers the user configured: search works identically on OpenRouter, a
// custom OpenAI-compatible endpoint, and a Claude Pro subscription.
//
// This module is transport only — no pi imports, no tool registration. `tools.ts`
// wraps it as a pi custom tool.
import type { SearchBackend, SearchConfig } from "./config.ts";

// One normalized hit. Backends differ in field names and in whether they return page
// text at all; everything below this boundary speaks this shape.
export interface SearchResult {
  title: string;
  url: string;
  /** Snippet or extracted page text. Empty when the backend returned none. */
  snippet: string;
  /** ISO date when the backend supplied one. */
  published?: string;
}

export interface SearchResponse {
  query: string;
  backend: SearchBackend;
  results: SearchResult[];
  /** Set when the backend returned fewer results than requested, or none. */
  note?: string;
  /**
   * What the backend charged for this call, when it says. Daemon-log only: it never
   * reaches the model (irrelevant to the answer) and never reaches the event log
   * (design 0007 records terminal facts, and this is a vendor's number).
   */
  costDollars?: number;
}

// A search problem the agent should see as a tool result rather than as a crash.
// `code` is stable and safe to show the model; it never carries the API key.
export class SearchError extends Error {
  readonly code: string;
  constructor(code: string, message: string) {
    super(message);
    this.name = "SearchError";
    this.code = code;
  }
}

// Conventional environment variable, used when `[search].api_key` is absent. This
// mirrors how builtin model providers fall back to their provider env var.
const ENV_KEYS: Record<SearchBackend, string> = {
  exa: "EXA_API_KEY",
};

const DEFAULT_BASE_URLS: Record<SearchBackend, string> = {
  exa: "https://api.exa.ai",
};

// One search must not stall a `#?` answer indefinitely. The terminal's own stall
// notice is 30s, so keep the network leg well inside it.
const REQUEST_TIMEOUT_MS = 10_000;

// Per-result snippet cap. A search result set that blows past the model's context
// helps nobody, and a terminal answer quotes a line or two at most.
const MAX_SNIPPET_CHARACTERS = 1_200;

// Exa's live-crawl budget defaults to 10s — the whole request budget above. A page
// that needs crawling would then consume the entire allowance and time the call out
// with nothing to show, so cap it at half and let the cached corpus answer the rest.
const EXA_LIVECRAWL_TIMEOUT_MS = 5_000;

export function resolveSearchApiKey(
  config: SearchConfig,
  env: Record<string, string | undefined>,
): string {
  if (config.api_key !== undefined) {
    return config.api_key;
  }
  const envKey = ENV_KEYS[config.provider];
  const fromEnv = env[envKey];
  if (fromEnv !== undefined && fromEnv.length > 0) {
    return fromEnv;
  }
  throw new SearchError(
    "missing_credentials",
    `web search is configured for "${config.provider}" but has no API key. Set [search].api_key in koshell.toml, or export ${envKey}.`,
  );
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function asString(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function asNumber(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value)
    ? value
    : undefined;
}

function clampSnippet(value: string): string {
  const collapsed = value.replace(/\s+/g, " ").trim();
  return collapsed.length > MAX_SNIPPET_CHARACTERS
    ? `${collapsed.slice(0, MAX_SNIPPET_CHARACTERS)}…`
    : collapsed;
}

// A snippet field may hold a string or, for Exa's `highlights`, an array of separately
// selected excerpts. Joining with an ellipsis keeps the discontinuity visible, so the
// model does not read two excerpts from opposite ends of a page as one passage.
function asSnippetSource(value: unknown): string | undefined {
  if (Array.isArray(value)) {
    const parts = value.filter(
      (entry): entry is string => typeof entry === "string" && entry.length > 0,
    );
    return parts.length > 0 ? parts.join(" … ") : undefined;
  }
  return asString(value);
}

// Backend responses are untrusted JSON: a shape change upstream must degrade to
// "fewer/no results", never to a thrown TypeError inside the agent loop.
function normalizeResults(
  raw: unknown,
  fields: { snippet: readonly string[]; published?: readonly string[] },
): SearchResult[] {
  if (!Array.isArray(raw)) {
    return [];
  }
  const results: SearchResult[] = [];
  for (const entry of raw) {
    if (!isRecord(entry)) {
      continue;
    }
    const url = asString(entry.url);
    if (url === undefined) {
      continue;
    }
    let snippet = "";
    for (const field of fields.snippet) {
      const candidate = asSnippetSource(entry[field]);
      if (candidate !== undefined) {
        snippet = clampSnippet(candidate);
        break;
      }
    }
    let published: string | undefined;
    for (const field of fields.published ?? []) {
      published = asString(entry[field]);
      if (published !== undefined) {
        break;
      }
    }
    results.push({
      title: asString(entry.title) ?? url,
      url,
      snippet,
      ...(published !== undefined ? { published } : {}),
    });
  }
  return results;
}

interface RequestSpec {
  url: string;
  init: RequestInit;
  /** Pulls the backend's result array out of its envelope. */
  extract: (body: unknown) => unknown;
  snippetFields: readonly string[];
  publishedFields?: readonly string[];
  /** Backends that price per call and report it; used for the daemon log only. */
  extractCost?: (body: unknown) => number | undefined;
}

// Exa is the only backend, so this dispatches on nothing. The keyed lookups above stay
// in their exhaustive `Record<SearchBackend, _>` form, which still forces a second
// backend to supply an env var and a base URL before it compiles; the request shape
// itself has no such guard, and a `switch` on the one-member union cannot provide one
// (lint rejects the always-true comparison). Adding a backend therefore means branching
// here by hand.
//
// The request is built to Exa's own guidance for agent workflows rather than to a
// generic search-API shape. Three choices come straight from their documentation:
//
//   * `contents` inline on `/search` instead of a follow-up `/contents` call. Exa
//     documents the inline form as the streamlined path and the standalone endpoint as
//     the one to use when you already hold the URLs — which we never do, since the
//     point of the call is to discover them. One round trip also fits the latency
//     budget above; two would not reliably.
//   * `highlights` rather than `text`. Exa selects the excerpts that match the query;
//     asking for `text` and slicing the first N characters locally — what this adapter
//     used to do — returns the top of the page, which on a docs or issue page is
//     navigation and boilerplate, not the answer. Their agent guide recommends
//     highlights for exactly this loop, at roughly a tenth the tokens.
//   * `type: "auto"`. Their guidance is that auto is almost always right and that
//     `fast`/`instant` are for when latency outweighs quality. At a ~1s p50 the 10s
//     budget has ample room, and a terminal answer is only as good as the evidence
//     under it, so quality wins. Set explicitly rather than left to the default so a
//     change to that default cannot silently retune `#?`.
function buildRequest(
  config: SearchConfig,
  apiKey: string,
  query: string,
): RequestSpec {
  const base = (config.base_url ?? DEFAULT_BASE_URLS[config.provider]).replace(
    /\/+$/,
    "",
  );
  return {
    url: `${base}/search`,
    init: {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-api-key": apiKey,
      },
      body: JSON.stringify({
        query,
        type: "auto",
        numResults: config.max_results,
        contents: {
          highlights: { query, maxCharacters: MAX_SNIPPET_CHARACTERS },
          livecrawlTimeout: EXA_LIVECRAWL_TIMEOUT_MS,
        },
      }),
    },
    extract: (body) => (isRecord(body) ? body.results : undefined),
    // Highlights only, and deliberately with no fallback behind them. `summary` and
    // `text` were listed here once as a safety net, but Exa returns a content field
    // only when it was requested, and the body above requests neither — so both were
    // unreachable, and the comment claiming they covered un-excerptable pages was
    // simply wrong. Listing the one reachable field makes the failure legible: a
    // result Exa could not excerpt yields an empty snippet, and its title and URL
    // still say the page exists. Buying a real fallback means requesting `summary`
    // too, which Exa bills per result on every call, for a case not yet observed.
    snippetFields: ["highlights"],
    publishedFields: ["publishedDate"],
    extractCost: (body) =>
      isRecord(body) && isRecord(body.costDollars)
        ? asNumber(body.costDollars.total)
        : undefined,
  };
}

// Maps transport and HTTP failures onto stable codes. The response body is not
// echoed: a provider error page can be long, and an auth failure's body sometimes
// repeats the submitted key.
function httpErrorCode(status: number): string {
  if (status === 401 || status === 403) {
    return "search_unauthorized";
  }
  // Exa documents 402 as credits exhausted or budget exceeded. It is worth its own
  // code because the remedy is the user's, not the agent's: a valid key with no
  // balance looks identical to a generic failure otherwise, and the agent would keep
  // retrying a call that cannot succeed until the account is topped up.
  if (status === 402) {
    return "search_payment_required";
  }
  if (status === 429) {
    return "search_rate_limited";
  }
  return "search_failed";
}

export interface SearchOptions {
  config: SearchConfig;
  apiKey: string;
  query: string;
  /** Caller's cancellation (Ctrl+C, agent abort), combined with the timeout. */
  signal?: AbortSignal | undefined;
  fetchImpl?: typeof fetch;
}

export async function runSearch(
  options: SearchOptions,
): Promise<SearchResponse> {
  const query = options.query.trim();
  if (query.length === 0) {
    throw new SearchError("invalid_arguments", "query must not be empty");
  }

  const spec = buildRequest(options.config, options.apiKey, query);
  const doFetch = options.fetchImpl ?? fetch;

  const timeout = AbortSignal.timeout(REQUEST_TIMEOUT_MS);
  const signal =
    options.signal !== undefined
      ? AbortSignal.any([options.signal, timeout])
      : timeout;

  let response: Response;
  try {
    response = await doFetch(spec.url, { ...spec.init, signal });
  } catch (error) {
    if (options.signal?.aborted === true) {
      throw new SearchError("cancelled", "the search was cancelled");
    }
    if (timeout.aborted) {
      throw new SearchError(
        "timeout",
        `the ${options.config.provider} search did not respond within ${String(REQUEST_TIMEOUT_MS / 1000)}s`,
      );
    }
    throw new SearchError(
      "search_unreachable",
      `cannot reach the ${options.config.provider} search API: ${error instanceof Error ? error.message : String(error)}`,
    );
  }

  if (!response.ok) {
    throw new SearchError(
      httpErrorCode(response.status),
      `the ${options.config.provider} search API returned HTTP ${String(response.status)}`,
    );
  }

  let body: unknown;
  try {
    body = await response.json();
  } catch {
    throw new SearchError(
      "search_failed",
      `the ${options.config.provider} search API returned a non-JSON response`,
    );
  }

  const results = normalizeResults(spec.extract(body), {
    snippet: spec.snippetFields,
    ...(spec.publishedFields !== undefined
      ? { published: spec.publishedFields }
      : {}),
  }).slice(0, options.config.max_results);

  const note =
    results.length === 0
      ? "the search returned no usable results"
      : results.length < options.config.max_results
        ? `the search returned ${String(results.length)} of ${String(options.config.max_results)} requested results`
        : undefined;

  const costDollars = spec.extractCost?.(body);

  return {
    query,
    backend: options.config.provider,
    results,
    ...(note !== undefined ? { note } : {}),
    ...(costDollars !== undefined ? { costDollars } : {}),
  };
}

// Renders a response as the plain text handed to the model. Kept here so the wire
// shape and its rendering stay together; `tools.ts` only wraps it.
export function renderSearchResponse(response: SearchResponse): string {
  const lines = [`Web search (${response.backend}) for: ${response.query}`];
  if (response.note !== undefined) {
    lines.push(`Note: ${response.note}`);
  }
  response.results.forEach((result, index) => {
    lines.push("");
    lines.push(`[${String(index + 1)}] ${result.title}`);
    lines.push(`    ${result.url}`);
    if (result.published !== undefined) {
      lines.push(`    published: ${result.published}`);
    }
    if (result.snippet.length > 0) {
      lines.push(`    ${result.snippet}`);
    }
  });
  return lines.join("\n");
}

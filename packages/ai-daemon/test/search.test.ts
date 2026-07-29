import { describe, expect, it } from "bun:test";

import type { SearchConfig } from "../src/config.ts";
import {
  renderSearchResponse,
  resolveSearchApiKey,
  runSearch,
  SearchError,
} from "../src/search.ts";

function config(overrides: Partial<SearchConfig> = {}): SearchConfig {
  return {
    provider: "exa",
    max_results: 3,
    ...overrides,
  };
}

// Captures the request the backend adapter built, and replies with `body`.
function fakeFetch(body: unknown, status = 200) {
  const calls: { url: string; init: RequestInit }[] = [];
  const impl = ((url: string, init: RequestInit) => {
    calls.push({ url, init });
    return Promise.resolve(
      new Response(JSON.stringify(body), {
        status,
        headers: { "content-type": "application/json" },
      }),
    );
  }) as unknown as typeof fetch;
  return { impl, calls };
}

describe("resolveSearchApiKey", () => {
  it("prefers the configured key over the environment", () => {
    expect(
      resolveSearchApiKey(config({ api_key: "from-config" }), {
        EXA_API_KEY: "from-env",
      }),
    ).toBe("from-config");
  });

  it("falls back to the backend's conventional env var", () => {
    expect(resolveSearchApiKey(config(), { EXA_API_KEY: "from-env" })).toBe(
      "from-env",
    );
  });

  it("names both the config key and the env var when neither is set", () => {
    expect(() => resolveSearchApiKey(config(), {})).toThrow(
      /\[search\]\.api_key.*EXA_API_KEY/s,
    );
  });
});

describe("runSearch", () => {
  it("normalizes an Exa response and sends the key as x-api-key", async () => {
    const { impl, calls } = fakeFetch({
      results: [
        {
          title: "ENOSPC explained",
          url: "https://example.test/enospc",
          highlights: ["  no   space   left  "],
          publishedDate: "2026-01-02",
        },
      ],
    });

    const response = await runSearch({
      config: config(),
      apiKey: "secret",
      query: "ENOSPC",
      fetchImpl: impl,
    });

    expect(calls[0]?.url).toBe("https://api.exa.ai/search");
    expect(
      (calls[0]?.init.headers as Record<string, string>)["x-api-key"],
    ).toBe("secret");
    expect(response.results).toEqual([
      {
        title: "ENOSPC explained",
        url: "https://example.test/enospc",
        snippet: "no space left",
        published: "2026-01-02",
      },
    ]);
  });

  // The request body is the whole point of the Exa specialization, so assert its
  // shape rather than only what comes back. Each field here is a documented choice
  // from Exa's agent guidance; a silent revert to `text` would go unnoticed without
  // this, because a text-sliced snippet still looks like a plausible result.
  it("asks Exa for query-guided highlights with an explicit search type", async () => {
    const { impl, calls } = fakeFetch({ results: [] });

    await runSearch({
      config: config(),
      apiKey: "k",
      query: "brew shallow clone",
      fetchImpl: impl,
    });

    const body = JSON.parse(calls[0]?.init.body as string) as Record<
      string,
      unknown
    >;
    expect(body.type).toBe("auto");
    expect(body.numResults).toBe(3);
    expect(body.contents).toEqual({
      highlights: { query: "brew shallow clone", maxCharacters: 1200 },
      livecrawlTimeout: 5000,
    });
    // Deprecated per Exa's guide: content modes must be nested under `contents`.
    expect(body.text).toBeUndefined();
    expect(body.highlights).toBeUndefined();
    expect(body.useAutoprompt).toBeUndefined();
    expect(body.livecrawl).toBeUndefined();
  });

  it("joins several Exa highlights so separate excerpts stay distinguishable", async () => {
    const { impl } = fakeFetch({
      results: [
        {
          url: "https://example.test/a",
          highlights: ["first excerpt", "", "second excerpt"],
        },
      ],
    });

    const response = await runSearch({
      config: config(),
      apiKey: "k",
      query: "q",
      fetchImpl: impl,
    });

    expect(response.results[0]?.snippet).toBe("first excerpt … second excerpt");
  });

  // Highlights are the only content field requested, so they are the only one read.
  // A result Exa could not excerpt keeps its title and URL and reports an empty
  // snippet, rather than appearing to carry evidence it does not have.
  it("leaves the snippet empty when a result carries no highlight", async () => {
    const { impl } = fakeFetch({
      results: [
        {
          title: "no excerpt",
          url: "https://example.test/a",
          highlights: [],
          summary: "a summary",
          text: "raw page text",
        },
      ],
    });

    const response = await runSearch({
      config: config(),
      apiKey: "k",
      query: "q",
      fetchImpl: impl,
    });

    expect(response.results).toEqual([
      { title: "no excerpt", url: "https://example.test/a", snippet: "" },
    ]);
  });

  it("reports what Exa charged without showing it to the model", async () => {
    const { impl } = fakeFetch({
      results: [{ url: "https://example.test/a", highlights: ["hi"] }],
      costDollars: { total: 0.007, search: { neural: 0.007 } },
    });

    const response = await runSearch({
      config: config(),
      apiKey: "k",
      query: "q",
      fetchImpl: impl,
    });

    expect(response.costDollars).toBe(0.007);
    expect(renderSearchResponse(response)).not.toContain("0.007");
  });

  // Credits exhausted is the user's problem to fix, not something to retry.
  it("maps Exa's payment-required status to its own code", async () => {
    const { impl } = fakeFetch({ error: "insufficient credits" }, 402);
    const error = (await runSearch({
      config: config(),
      apiKey: "k",
      query: "q",
      fetchImpl: impl,
    }).catch((caught: unknown) => caught)) as SearchError;
    expect(error.code).toBe("search_payment_required");
  });

  // A backend shape change must degrade to "no results", never crash the agent loop.
  it("drops malformed entries instead of throwing", async () => {
    const { impl } = fakeFetch({
      results: [
        null,
        "not an object",
        { title: "no url here" },
        { url: "https://example.test/ok" },
      ],
    });

    const response = await runSearch({
      config: config(),
      apiKey: "k",
      query: "q",
      fetchImpl: impl,
    });

    expect(response.results).toEqual([
      {
        title: "https://example.test/ok",
        url: "https://example.test/ok",
        snippet: "",
      },
    ]);
    expect(response.note).toContain("1 of 3");
  });

  it("reports an entirely unexpected envelope as no usable results", async () => {
    const { impl } = fakeFetch({ unexpected: true });
    const response = await runSearch({
      config: config(),
      apiKey: "k",
      query: "q",
      fetchImpl: impl,
    });
    expect(response.results).toEqual([]);
    expect(response.note).toBe("the search returned no usable results");
  });

  it("maps auth failures to a stable code without echoing the body", async () => {
    const { impl } = fakeFetch({ error: "bad key secret-value" }, 401);
    const error = (await runSearch({
      config: config(),
      apiKey: "secret-value",
      query: "q",
      fetchImpl: impl,
    }).catch((caught: unknown) => caught)) as SearchError;

    expect(error).toBeInstanceOf(SearchError);
    expect(error.code).toBe("search_unauthorized");
    expect(error.message).not.toContain("secret-value");
  });

  it("maps rate limiting to its own code", async () => {
    const { impl } = fakeFetch({}, 429);
    const error = (await runSearch({
      config: config(),
      apiKey: "k",
      query: "q",
      fetchImpl: impl,
    }).catch((caught: unknown) => caught)) as SearchError;
    expect(error.code).toBe("search_rate_limited");
  });

  it("rejects an empty query before any network call", async () => {
    let called = false;
    const impl = (() => {
      called = true;
      return Promise.resolve(new Response("{}"));
    }) as unknown as typeof fetch;

    const error = (await runSearch({
      config: config(),
      apiKey: "k",
      query: "   ",
      fetchImpl: impl,
    }).catch((caught: unknown) => caught)) as SearchError;

    expect(error.code).toBe("invalid_arguments");
    expect(called).toBe(false);
  });

  it("surfaces caller cancellation as cancelled", async () => {
    const controller = new AbortController();
    controller.abort();
    const impl = (() =>
      Promise.reject(new Error("aborted"))) as unknown as typeof fetch;

    const error = (await runSearch({
      config: config(),
      apiKey: "k",
      query: "q",
      signal: controller.signal,
      fetchImpl: impl,
    }).catch((caught: unknown) => caught)) as SearchError;

    expect(error.code).toBe("cancelled");
  });

  it("honors the configured base_url", async () => {
    const { impl, calls } = fakeFetch({ results: [] });
    await runSearch({
      config: config({ base_url: "https://proxy.test/api/" }),
      apiKey: "k",
      query: "q",
      fetchImpl: impl,
    });
    expect(calls[0]?.url).toBe("https://proxy.test/api/search");
  });
});

describe("renderSearchResponse", () => {
  it("renders numbered hits with their urls", () => {
    const text = renderSearchResponse({
      query: "tar flags",
      backend: "exa",
      results: [
        {
          title: "tar(1)",
          url: "https://example.test/tar",
          snippet: "extract with -x",
          published: "2026-01-01",
        },
      ],
    });

    expect(text).toContain("Web search (exa) for: tar flags");
    expect(text).toContain("[1] tar(1)");
    expect(text).toContain("https://example.test/tar");
    expect(text).toContain("published: 2026-01-01");
    expect(text).toContain("extract with -x");
  });

  it("states the note when the backend returned nothing", () => {
    const text = renderSearchResponse({
      query: "q",
      backend: "exa",
      results: [],
      note: "the search returned no usable results",
    });
    expect(text).toContain("Note: the search returned no usable results");
  });
});

# Design 0019 — the web_search tool and the custom-tool seam

Date: 2026-07-28 15:55 CST +0800

Status: implemented.

## Why

`#?` answers are grounded only in what the terminal already showed. That is the right
default, but it fails on a recurring class of question: an error string the model has
never seen, a tool's current flags, a package version, anything that changed after the
model's training cutoff. The agent's honest answer there is "I don't know", and the user
leaves the terminal to go search — the exact context switch `#?` exists to remove.

The requested change was "add web search", with an explicit question attached: can we
borrow the model provider's own search, or must we integrate a dedicated search API?

## Investigation: the provider's search is unreachable

Provider-native web search exists and would be the cheapest option — no extra key, no
extra vendor, results already grounded by the provider. Anthropic ships
`web_search_20250305` as a server-side tool, OpenAI's Responses API ships `web_search`,
and Gemini ships `google_search` grounding. All three are declared by putting an object
with a `type` discriminator into the request's `tools` array.

That array is not reachable from Koshell. The daemon talks to every provider through
`@earendil-works/pi-coding-agent` / `pi-ai` (0.80.3), and pi's tool model is
function-calling only:

- `pi-ai`'s `Tool` interface is `{ name, description, parameters }` — no `type` field
  (`pi-ai/dist/types.d.ts:318`).
- Each API adapter converts that to a plain function schema. The Anthropic adapter's
  `convertTools()` emits `{ name, description, input_schema }` and nothing else
  (`pi-ai/dist/api/anthropic-messages.js:930`).
- Grepping the whole of `pi-ai` and `pi-coding-agent` for
  `server_tool|web_search|google_search|url_context` returns zero hits.

pi's own builtin tools are `bash`, `edit`, `find`, `grep`, `ls`, `read`, `write` — none
of them network tools. pi also ships no MCP client by design ("It intentionally does not
include built-in MCP", `pi-coding-agent/docs/usage.md:306`), so pointing the daemon at an
existing search MCP server is not an escape hatch either.

Three options remained:

1. **A Koshell custom tool over a dedicated search API.** Provider-independent; costs the
   user one extra API key.
2. **Fork `pi-ai` to pass provider server tools through.** Most "native", but requires a
   separate implementation for each of three unrelated API shapes, for an upstream
   dependency Koshell would then have to track forever.
3. **A custom tool that bypasses pi and calls the provider's native API directly.** No
   extra key, but only for the handful of providers that offer search — producing a
   capability that silently disappears when the user switches to OpenRouter or a custom
   endpoint.

Option 1 was chosen. Koshell deliberately supports pi's whole builtin catalog (30+
providers, 1000+ models, design 0013), plus custom OpenAI-compatible endpoints. A search
capability that works on some of those and not others is worse than one extra key: it
makes the tool's presence depend on a model choice made for unrelated reasons.

## Why this belongs in `#?`

Recorded 2026-07-28 by the product owner. Search was requested from a live case: `#?`
was genuinely useful for diagnosing a homebrew problem, and the gap it hit was that the
AI could only reason from its training corpus when the answer needed current material
from the internet.

That is the whole justification, and it is sufficient. The 2026-07-25 repositioning made
the observation layer the product core and `#?` a secondary head; it did not make `#?`
disposable, and a change that makes the surviving head better is on-goal. Positioning is
treated as a live judgment rather than a fixed rule.

## The custom-tool seam

Registering any custom tool means changing `noTools` from `"all"` to `"builtin"` — pi's
one switch that keeps its file/shell/edit/write tools disabled while enabling custom
ones. That flip is the load-bearing change, so it is made once, in one place, and driven
by what is actually configured:

- `tools.ts` is the only module that turns a Koshell capability into a pi tool. It
  returns a possibly-empty array.
- `agent-runtime.ts` keeps `noTools: "all"` when that array is empty (the historical
  push-only session, byte-for-byte) and uses `noTools: "builtin"` with `customTools` when
  it is not.
- The observe-only boundary is therefore enforced by what `tools.ts` contains, not by
  pi's defaults. Nothing in this slice writes to the PTY, runs a process, or reads a file.

This seam is also what the completed-command tools (`list_recent_commands`,
`read_command_output`) plug into later; the search tool is its first occupant.

## Registration is conditional, not just execution

A tool whose backing capability is unconfigured is **not registered**, rather than
registered and failing when called. Two failure modes motivate this:

- An agent told it has a search tool will promise the user a lookup. If the tool then
  returns `missing_credentials`, the answer has already been shaped around evidence that
  never arrived.
- The system prompt has to agree with the tool set in both directions. The old prompt
  said "you cannot run commands, read files, or fetch anything beyond what the request
  contains" — with a search tool registered, that sentence makes the agent decline to use
  it. `buildSystemPrompt({ webSearch })` now emits the matching capability clause and,
  when search is on, the deterministic use rules.

A `[search]` block whose key resolves to nothing logs a warning and leaves the tool off.
Optional capabilities never fail conversation creation: `#?` must still answer.

### The prompt names the backend, 2026-07-29

`buildSystemPrompt` originally took `webSearch: boolean`, and the tool description named
no vendor. That was enough for the agent to _use_ search and not enough for it to
describe what it had: asked directly which search it was using, it answered that it had
no such tool. The backend name existed only in `renderSearchResponse`'s
`Web search (exa) for:` header, which is not in context until a search has already run —
so the one question a user is likely to ask before trusting the capability was the one it
could not answer.

The option is now `webSearch?: { backend: string }`, and the backend is interpolated into
both the system prompt's capability clause and the tool description. Carrying the name
rather than a boolean is what makes the wrong state unrepresentable: there is no way to
say "search is on" without saying what it is on top of. `agent-runtime.ts` reads the
provider off the config only when the tool actually registered, so the prompt cannot name
a vendor the session has no way to reach.

The prompt also states that the backend is a dedicated search vendor rather than the
model provider's own search. Without that, an agent on Anthropic or OpenAI has every
reason to assume it is calling its own provider's search — and would say so.

## Configuration

```toml
[search]
provider = "exa"      # the only accepted value
api_key = "..."       # optional; falls back to EXA_API_KEY
base_url = "..."      # optional endpoint override
max_results = 5       # 1..20, default 5
```

The backend sits behind one normalized `SearchResult` shape, so adding a second is a
request builder plus a field mapping. Backend responses are untrusted JSON and are
parsed defensively: an upstream shape change degrades to "no usable results" with an
explicit note, never to a thrown `TypeError` inside the agent loop.

`configurationFingerprint` includes `search`, so `koshell reload` treats a search change
as a conversation rebuild rather than an in-place switch. Unlike the model, the tool
catalog and system prompt are fixed when the pi session is created.

## Exa is the only backend

This slice was first written with three adapters — Exa, Tavily, and Brave — behind one
config enum. Tavily and Brave were removed on 2026-07-29, before any of it was
committed.

They were written from published request/response shapes and covered only by fixtures
built from those same documents, so the fixtures could not detect a shape that was
documented wrong, has since changed, or that the adapter simply misread. Neither
followed its vendor's own agent guidance the way the Exa adapter was later revised to:
Tavily's `search_depth: "basic"` was a plain default rather than a considered choice,
and Brave returns only a short `description` per result with no page text at all, so
even a correct adapter would have yielded materially thinner evidence.

Labelling them experimental was the first attempt at handling this, and it was the
wrong instrument. A warning at registration does not stop the code from having to be
kept compiling, kept covered, and kept reviewed; it only transfers the risk to the user
while the maintenance stays here. Two adapters that had never returned a real result
were carrying a supported integration's weight and proving nothing.

The reason recorded for keeping them — that removal would break existing configs — was
simply false. Nothing in this slice had been committed, let alone released, so there
were no configs to break. It is worth naming as a failure mode: a compatibility
argument was accepted without checking whether the thing needing compatibility existed.

`provider` stays a required enum with one legal value rather than being dropped, so
that a second backend, if one is ever warranted, needs no config-shape change. The env
var and base URL lookups in `search.ts` likewise keep their exhaustive
`Record<SearchBackend, _>` form, so adding a backend fails to compile until both are
supplied. The request builder has no equivalent guard — a `switch` over a one-member
union is an always-true comparison that lint rejects — so it dispatches on nothing and
a second backend means branching there by hand.

## The Exa adapter

Three choices come from Exa's documentation rather than from this codebase's judgment.

**`contents` inline on `/search`, not a follow-up `/contents` call.** Exa exposes both,
and the two-step form is the more visible one, so the single call looked like a corner
being cut. It is not: Exa documents the inline form as the streamlined path and the
standalone `/contents` endpoint as the one for URLs you already hold — which is never
our situation, since discovering the URLs is what the call is for. One round trip also
fits the latency budget below, where two would not reliably.

**Highlights are read with no fallback behind them.** `snippetFields` listed `summary`
and `text` after `highlights` at first, described as covering a page Exa could not
excerpt. They could not: Exa returns a content field only when it was requested, and
the request asks for neither, so both entries were unreachable and the safety they
implied did not exist. Only `highlights` is read now. A result Exa cannot excerpt
therefore yields an empty snippet, keeping its title and URL — legibly nothing, rather
than an entry that appears to carry evidence. Buying a real fallback would mean
requesting `summary` as well, which Exa bills per result on every call, for a case that
has not yet been observed. The narrower chain also makes the live check unambiguous:
with one reachable field, a non-empty snippet can only have come from `highlights`.

**`highlights`, not `text`.** This is where the first implementation had invented its
own method. It requested `text` with a character cap and then sliced the first 1,200
characters locally — which returns the _top_ of the page. On a documentation page or a
GitHub issue that is navigation, banners, and boilerplate, not the passage that matches
the query. Exa's `highlights: { query, maxCharacters }` has the vendor select the
matching excerpts instead, and their agent guide recommends exactly this for repeated
search loops, at roughly a tenth the tokens. Several highlights per result are joined
with an ellipsis so the model does not read two excerpts from opposite ends of a page as
one continuous passage.

**`type: "auto"`, set explicitly.** Exa's guidance is that `auto` is almost always right
and that `fast`/`instant` are for when latency outweighs quality. At a ~1s p50 the
10-second budget has ample room, and a terminal answer is only as good as the evidence
under it. It is sent explicitly rather than left to the default so that a change to that
default cannot silently retune `#?`.

Two smaller bounds follow from the same reading: `contents.livecrawlTimeout` is capped at
5s, because Exa's default is 10s — the entire request budget, which a crawled page would
consume before returning anything; and HTTP 402 gets its own `search_payment_required`
code, because exhausted credits are the user's to fix and otherwise look identical to a
generic failure the agent would keep retrying.

## Live verification, 2026-07-29

The adapter was run against the real Exa API for the first time on 2026-07-29, through
`#?` in a normal terminal rather than a harness. What that confirmed:

- The whole path works end to end: key resolution, the request, HTTP, JSON parsing,
  normalization, rendering, and the model choosing to call the tool and citing the URLs
  it got back. Five results for the default `max_results = 5`.
- `highlights` arrives populated. This is unambiguous rather than inferred: `highlights`
  is the only content field requested and the only one read, so a non-empty snippet
  cannot have come from anywhere else. The quiet fallback that made this hard to confirm
  was removed in the same pass — it had never been reachable.
- `costDollars.total` extracts from a real response: **$0.0070 for one 5-result call**.
  That is the number this field existed to produce. At the prompt's two-searches-per-
  question bound, a search-answering `#?` costs roughly 1.4 cents in search alone.

An earlier attempt the same day looked successful but proved nothing: the daemon binary
had been rebuilt while the old process kept running its replaced inode, so the call was
served by the pre-specialization adapter. The tell was a log line that the new code
emits unconditionally on success and that never appeared. Worth remembering as a
verification hazard — for a long-lived daemon, "the code on disk is new" and "the code
that ran is new" are different claims, and only the second one is evidence.

## Failure-path verification, 2026-07-29 11:00 CST

The success run above left every failure branch unverified. Three of them are now
verified too, by `packages/ai-daemon/scripts/search-live-check.ts` (`bun run
search:live` in the daemon package) — opt-in, outside `bun run check`, because one case
is billed and another takes ten seconds.

The script exists because `test/search.test.ts` structurally cannot close this gap. Those
tests hand a fake fetch a status code _we chose_ and assert we map it as intended; they
say nothing about which status the live service returns. That is the same seam the dead
`summary`/`text` fallback slipped through — fixtures asserted a defensive path worked
while the real API made it unreachable. Each case below asserts something a fixture
cannot reach:

- **401 → `search_unauthorized`.** A deliberately invalid key against the real endpoint.
  Exa answers **HTTP 401** (confirmed directly, not inferred from the mapped code, since
  `httpErrorCode` folds 401 and 403 together). This is the failure a user actually meets
  — a typo'd, revoked, or unexported key — and it costs nothing to check, needs no
  credential, and sends no terminal content.
- **Refused connection → `search_unreachable`.** A local port with nothing listening.
- **No response → `timeout` after 10s.** A local server that accepts and then answers
  nothing. This is the only way to exercise the real `AbortSignal.timeout` end to end; a
  fake fetch rejecting on demand proves nothing about the timeout itself. The local
  hanging server is deliberate — pointing at an unroutable address would return "host
  unreachable" on some networks and hang on others, making the assertion depend on where
  it ran.

Still unverified, and why: **402** (credits exhausted or budget exceeded) cannot be
produced on demand without an account that is genuinely out of balance. The script says
so rather than pretending to cover it, and names the way to close it — run with `--paid`
from such an account, and the success case failing with `search_payment_required` _is_
the evidence. **429** would require deliberately hammering a vendor's rate limiter, which
is not a reasonable thing to do to close a documentation gap. The malformed-envelope
branch and the `livecrawlTimeout` bound remain fixture-covered.

The billed success case (`--paid`, needs `EXA_API_KEY`, ~$0.007) is in the script too, so
the 2026-07-29 success run is now repeatable rather than a one-off. It asserts more than
"no error": at least one result must carry a non-empty snippet, which fails loudly if the
request shape ever drifts back to a content mode Exa does not return.

Exa reports `costDollars` per call. It is written to the daemon log only — it is not
evidence, so it never reaches the model, and it is a vendor's number rather than a
terminal fact, so it never reaches the design 0007 event log. The one question it
answers is whether search stays affordable under daily `#?` use.

## Bounds and trust

- 10-second per-call timeout, well inside the terminal's 30-second stall notice
  (design 0010). The caller's `AbortSignal` (Ctrl+C, agent abort) is combined with it.
- 1,200-character snippet cap per result; whitespace collapsed. Applied locally even
  though the backend was asked for the same bound, so a backend that ignores it cannot
  flood the model's context.
- The prompt bounds the agent to at most two searches per question. This is a sentence
  in the system prompt, not an enforced cap: nothing counts `web_search` calls, and the
  32-call bridge budget covers only terminal-served tools. The number was chosen so two
  sequential 10-second calls stay inside the 30-second stall notice; it has not been
  validated against how many searches a question actually needs.
- Failures return as ordinary tool results carrying a stable code
  (`search_unauthorized`, `search_payment_required`, `search_rate_limited`, `timeout`,
  `search_unreachable`, `cancelled`, `invalid_arguments`, `search_failed`), so the agent answers from the
  terminal evidence it already has instead of aborting the turn. Response bodies are
  never echoed into the code's message — an auth error page sometimes repeats the
  submitted key.
- Search results are third-party text entering the model's context. Both the tool
  description and the system prompt state that they are untrusted evidence, never
  instructions, and that no command should be recommended solely because a page said so.

## Privacy

Calling the tool sends **the query** to the search vendor — a different vendor from the
model provider. Terminal content is not sent. The query is model-authored, so it can
quote an error string from the user's terminal; that is the point of the tool and the
same exposure the pushed context package already has toward the model provider. The
capability is off unless the user configures it.

The query is also **printed to the terminal** as the search starts
(`design-0021-visible-tool-activity.md`). A search runs entirely inside the daemon, so
without that announcement the one outbound transfer koshell makes on the user's behalf
would be the least visible thing it does.

## Open issues

- The success path, 401, unreachable, and timeout are live-verified (2026-07-29, above).
  **402, 429, and the malformed envelope are not**, for the reasons given in that
  section. The envelope case is the one that fails quietly: a vendor changing its shape
  degrades to "no usable results" rather than breaking, so a wrong guess about it looks
  like a search that found nothing.
- `web_search` returns excerpts only; there is no page-fetch tool. A result whose answer
  lies outside the returned highlights cannot be read further. This couples to the
  two-search bound: not being able to read a promising page raises the value of
  searching again, and both bounds were set without data.
- No per-session or per-day call budget beyond the prompt's two-searches instruction,
  which is guidance rather than an enforced cap.
- The search vendor is a third recipient of user-derived text alongside the model
  provider. There is no redaction of the model-authored query.
- Exa's `type` is fixed at `auto`. A user on a slow link who would prefer `fast` has no
  way to say so; adding config for it was judged premature while the backend is
  unverified.

## Resolution conditions

- ~~Smoke Exa against its live API, checking that `highlights` arrives populated.~~ Done
  2026-07-29; see the verification section above.
- ~~Exercise the failure branches against the live API — at minimum a bad key (401) and
  an exhausted account (402).~~ 401 done 2026-07-29, along with unreachable and timeout.
  402 remains: run `bun run search:live --paid` from an account that is actually out of
  balance, and record the resulting `search_payment_required`. 429 and the malformed
  envelope stay fixture-covered by choice.
- Add a second backend only when a concrete need appears, and only with the same
  standard Exa was held to: a live smoke plus a pass over that vendor's own agent
  guidance. A backend that cannot meet both does not go in behind a warning.
- Revisit a page-fetch tool only if dogfooding shows highlights are routinely
  insufficient; revisit the two-search bound at the same time, since the two trade off.
- Decide an enforced call budget if the prompt-level bound proves insufficient in
  dogfooding — the `tool_activity` events (design 0007) are what will show it.

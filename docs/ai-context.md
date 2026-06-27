# AI Context Contract

## Requirement

Build the first cache-aware AI context contract for Koshell's agent-runtime boundary. AI is a core Koshell product capability, but terminal-core, terminal context, and timeline must remain independent from concrete LLM providers and full agent harnesses.

## Timestamp

Performed at: 2026-06-27 22:58:25 CST +0800.

## Implementation

- Added `src/ai-context.ts` as the first agent-runtime-facing context module.
- Added a stable context package contract version: `koshell_ai_context_v1`.
- Added a stable prompt-prefix version: `koshell_ai_stable_prefix_v1`.
- Added a stable context tool catalog version: `koshell_context_tools_v1`.
- Added `createKoshellContextTools()` to expose Koshell context operations as `@earendil-works/pi-agent-core` `AgentTool` definitions.
- Added TypeBox tool parameter schemas through `@earendil-works/pi-ai`.
- Added `executeKoshellContextTool()` for provider-independent internal execution with `pi-ai` tool-argument validation.
- Added standard `AgentToolResult` output with text `content` for model consumption and structured `details` for runtime consumers.
- Added a cache policy that keeps tools and static instructions in the stable prefix while placing changing terminal state in a dynamic suffix.

## Tool Catalog

The initial stable tool catalog is:

- `koshell_get_current_context`
- `koshell_get_screen_snapshot`
- `koshell_diff_screen_snapshots`
- `koshell_list_recent_screen_changes`
- `koshell_get_recent_timeline_events`

The catalog names, descriptions, schema shape, and order are intended to stay stable between requests so future LLM provider adapters can preserve prompt-cache prefix matches.

## Dependency Boundary

The AI context module depends on lower-level pi packages because AI is a core Koshell runtime capability:

- `@earendil-works/pi-agent-core` for `AgentTool` and `AgentToolResult` abstractions.
- `@earendil-works/pi-ai` for TypeBox re-exports, tool validation, and provider/cache terminology such as `CacheRetention`.

The terminal timeline and terminal context modules do not depend on these packages. They expose explicit interfaces that `ai-context` consumes.

## Cache-Aware Design

Stable provider prompt prefix should contain:

- Koshell AI instructions.
- Stable context tool definitions.
- Stable output and context contract descriptions.

Dynamic provider prompt suffix should contain:

- Current user request.
- Current terminal context package.
- Recent screen-change summaries.
- Snapshot ids and follow-up handles.

Large or volatile data, such as detailed screen diffs or full snapshots, should be fetched by tool call instead of being inserted into the default context payload.

## Supply Chain Note

Adding the lower-level pi dependencies introduced transitive packages with install build scripts. The project explicitly denied build scripts for `@google/genai` and `protobufjs` in `pnpm-workspace.yaml`; `node-pty` remains allowed because Koshell already depends on it for PTY functionality.

## Validation

Public unit tests cover:

- Stable cache policy.
- Stable AgentTool catalog order and shape.
- Stable dynamic context object shape across empty and active terminal states.
- Budget trimming.
- Follow-up handles for snapshots and diffs.
- AgentToolResult `content` and `details` output.
- Tool input validation at the agent-runtime boundary.

## Open Issues

- No LLM provider is connected yet.
- No `#?` trigger is connected yet.
- No prompt builder or provider payload serializer exists yet.
- No cache-hit telemetry is recorded yet.
- Tool argument validation uses the lower-level pi validation behavior, including primitive coercion before schema validation.
- No retention or redaction policy is applied to AI context packages yet.

## Resolution Conditions

- Connect provider adapters after the agent-runtime boundary is stable.
- Add the `#?` trigger after shell integration is introduced.
- Add prompt building only after stable-prefix and dynamic-suffix ordering is documented and tested.
- Add cache-hit telemetry when provider calls are implemented.
- Decide whether primitive coercion is acceptable after agent-runtime dogfooding.
- Specify redaction and retention before any persistent AI context storage ships.

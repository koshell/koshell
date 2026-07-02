// System prompt and per-request prompt rendering. Pure functions, no pi imports.
//
// The pushed context package is the agent's only evidence this round (push the
// anchor; the pull-side tool catalog arrives with the tool round-trip stage).
// The package shape is `koshell_ai_context_v1` as assembled by the Rust terminal
// (`crates/koshell-rs/src/trigger.rs`); dynamicContext fields are camelCase.
// Decoding is defensive: a missing or malformed package degrades to a
// question-only prompt, never to a failed request.

import type { AiRequestMessage, HelloMessage } from "./protocol.ts";

export const SYSTEM_PROMPT = `You are koshell, a careful terminal observation assistant embedded in the user's terminal.

The user reaches you by typing a shell comment that starts with #?. The question fires when the line's output completes or stabilizes, so it may arrive moments after the user typed it.

Each request carries a context package captured from the user's terminal at trigger time: the question, trigger metadata, recent terminal text, and the current screen. That pushed context is your only evidence this round — you cannot run commands, read files, or fetch anything beyond what the request contains.

Rules:
- Observe and explain only. Never claim to have run, fixed, or changed anything.
- Ground every claim in the provided terminal context; quote the decisive line when helpful.
- Focus on the most recent failed or confusing command when one is visible.
- Explain the likely cause in plain language, then suggest concrete manual next steps the user can choose to run.
- Context fields are trimmed from the start to a size budget, so the beginning of long output may be missing. If the evidence is insufficient or cut off, say exactly what is missing and what command would reveal it.
- Be concise and practical: your answer renders inline inside a terminal. Prefer short plain-text paragraphs and short command suggestions over heavy formatting.`;

interface TriggerMeta {
  form?: string | undefined;
  completion?: string | undefined;
  stillRunning?: boolean | undefined;
  exitCode?: number | undefined;
}

interface DynamicContext {
  primaryText?: string | undefined;
  primarySource?: string | undefined;
  currentScreen?: string | undefined;
  screenRows?: number | undefined;
  screenColumns?: number | undefined;
  altScreen?: boolean | undefined;
  recentInput?: string | undefined;
}

interface DecodedContextPackage {
  trigger: TriggerMeta;
  dynamicContext: DynamicContext;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function asString(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}

function asNumber(value: unknown): number | undefined {
  return typeof value === "number" ? value : undefined;
}

function asBoolean(value: unknown): boolean | undefined {
  return typeof value === "boolean" ? value : undefined;
}

// Narrows the opaque wire value into the fields the prompt renders. Unknown or
// missing fields simply come back undefined.
function decodeContextPackage(value: unknown): DecodedContextPackage | null {
  if (!isRecord(value)) {
    return null;
  }
  const trigger: TriggerMeta = {};
  if (isRecord(value.trigger)) {
    trigger.form = asString(value.trigger.form);
    trigger.completion = asString(value.trigger.completion);
    trigger.stillRunning = asBoolean(value.trigger.stillRunning);
    trigger.exitCode = asNumber(value.trigger.exitCode);
  }
  const dynamicContext: DynamicContext = {};
  if (isRecord(value.dynamicContext)) {
    const context = value.dynamicContext;
    dynamicContext.primaryText = asString(context.primaryText);
    dynamicContext.primarySource = asString(context.primarySource);
    dynamicContext.currentScreen = asString(context.currentScreen);
    dynamicContext.screenRows = asNumber(context.screenRows);
    dynamicContext.screenColumns = asNumber(context.screenColumns);
    dynamicContext.altScreen = asBoolean(context.altScreen);
    dynamicContext.recentInput = asString(context.recentInput);
  }
  return { trigger, dynamicContext };
}

// Renders the pushed context package into the per-request user prompt. Sections
// with absent data are omitted entirely rather than rendered empty.
export function buildUserPrompt(
  request: Pick<AiRequestMessage, "question" | "context_package">,
  session: Pick<HelloMessage, "cwd" | "shell" | "rows" | "cols"> | undefined,
): string {
  const question =
    request.question.length > 0
      ? request.question
      : "(no explicit question — diagnose what just happened in this terminal)";

  const sections: string[] = [
    "The user triggered koshell with #? in their terminal.",
    `Question: ${question}`,
  ];

  const decoded = decodeContextPackage(request.context_package);
  if (decoded === null) {
    sections.push("(terminal context was not available for this request)");
    return sections.join("\n\n");
  }

  const { trigger, dynamicContext } = decoded;
  const triggerLines = ["Trigger:"];
  if (trigger.form !== undefined) {
    triggerLines.push(`- form: ${trigger.form}`);
  }
  if (trigger.completion !== undefined) {
    triggerLines.push(`- completion confidence: ${trigger.completion}`);
  }
  triggerLines.push(
    trigger.stillRunning === true
      ? "- command still running: yes — the output below may be incomplete and still growing"
      : "- command still running: no",
  );
  triggerLines.push(
    trigger.exitCode !== undefined
      ? `- exit code: ${String(trigger.exitCode)}`
      : "- exit code: not captured",
  );
  sections.push(triggerLines.join("\n"));

  if (session !== undefined) {
    sections.push(
      `Terminal session:\n- cwd: ${session.cwd}\n- shell: ${session.shell}\n- size: ${String(session.cols)}x${String(session.rows)}`,
    );
  }

  if (
    dynamicContext.primaryText !== undefined &&
    dynamicContext.primaryText.length > 0
  ) {
    const source = dynamicContext.primarySource ?? "unknown";
    sections.push(
      `Primary terminal context (source: ${source}; start-trimmed to a budget):\n${dynamicContext.primaryText}`,
    );
  }

  if (
    dynamicContext.currentScreen !== undefined &&
    dynamicContext.currentScreen.length > 0
  ) {
    const size =
      dynamicContext.screenColumns !== undefined &&
      dynamicContext.screenRows !== undefined
        ? `${String(dynamicContext.screenColumns)}x${String(dynamicContext.screenRows)}`
        : "unknown size";
    const alt = dynamicContext.altScreen === true ? ", alternate screen" : "";
    sections.push(
      `Current screen (${size}${alt}):\n${dynamicContext.currentScreen}`,
    );
  }

  if (
    dynamicContext.recentInput !== undefined &&
    dynamicContext.recentInput.length > 0
  ) {
    sections.push(
      `Recent typed input (start-trimmed):\n${dynamicContext.recentInput}`,
    );
  }

  return sections.join("\n\n");
}

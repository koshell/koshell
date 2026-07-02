# Design 0002 — AI output presentation and context boundaries

Date: 2026-07-02 12:28 CST

Status: designed, not yet implemented. These decisions govern the next stage (pi-backed
AI daemon, streaming responses, presentation); nothing in the current codebase implements
them yet.

## Context

Design 0001 settles _when_ the AI is asked (`#?` semantics and firing). This document
settles four adjacent questions that surfaced while reviewing the product design for
remaining ambiguity, all sitting on the critical path to the AI-integration stage:

1. AI responses are written to the user's terminal, but the terminal mirror previously
   consumed only PTY bytes — after one AI answer, screen snapshots would no longer match
   what the user sees, undermining both context truthfulness (the product promise is "AI
   uses the context you are looking at") and the echo-based arming check.
2. The "buffer shell output while AI streams" presentation model assumed the command had
   ended, leaving only a bounded prompt to buffer; stabilization-point firing on
   still-running commands (`pnpm dev`) breaks that assumption with unbounded output.
3. The split between context pushed with an `ai_request` and context pulled by the agent
   through terminal tools was undefined. Pushing everything bloats the model context and
   cost; pure pulling suffers from agent passivity and mistimed fetches.
4. The AI conversation's scope (per terminal window versus per user) was still stated in
   pre-rewrite wording.

## Mirror-feed invariant

**The terminal mirror consumes the exact byte stream written to the user's terminal —
PTY output plus presentation (AI) output, in the same order.**

Synchronization holds by construction: anything the user sees, the mirror saw. Screen
snapshots stay truthful after AI responses, and the echo-verification arming check
(design 0001) keeps working because the mirror cursor tracks the real cursor.

Consequence for the evidence rule: AI response text stays out of terminal _text_ context
(it is an AI lifecycle event, never PTY output, and never appears in recent-text
queries), but screen snapshots now inevitably contain the AI's own rendered lines —
exactly as the user's screen does. To keep those lines self-identifying, AI output is
rendered with a fixed recognizable prefix or style, so the agent can tell its own past
words from shell output when reading a snapshot.

## Presentation: buffer the bounded side

The previous model (validated in the pre-rewrite prototype): while an AI response
streams, buffer subsequent shell output and flush it afterwards, so `#?` feels like a
shell command that prints AI output before the next prompt. That model silently assumed
the buffered side is bounded — true only when the command has ended.

The generalized rule: **buffer whichever side is bounded.**

- **Command ended** (shell `command_end` observed): the leftover shell output is bounded —
  typically just the returned prompt. Keep the prototype behavior: AI text streams first,
  buffered output flushes after, in original order.
- **Command still running** (stabilization-point firing): the program's future output is
  unbounded and owns the terminal in real time. The AI response is buffered instead and
  inserted **block-atomically at the next output-quiescence gap**. If no gap arrives
  within a bounded max-wait, the block is inserted anyway — one seam, never line-level
  interleaving.

Program output never waits on AI output while the producer is alive; preserving normal
terminal behavior is the priority. The gap-insertion primitive is also what a future
watch capability would use to speak.

## Context: push the anchor, pull the exploration

Pulling through tools is not cheaper than pushing: tool results enter the same model
context, and each pull costs an extra model round trip (latency plus reasoning). Pulls
win only because they are conditional. The boundary is therefore: **push what most
answers need; pull the exploratory tail.**

Pushed with every `ai_request` (small, deterministic, question-anchored):

- the question plus trigger metadata (form, completion confidence, still-running
  annotation);
- the bounded tail of the triggering line's output span, with an explicit truncation
  marker;
- the current screen snapshot (naturally bounded by rows × columns);
- session facts: cwd, shell, dimensions, last command and exit code;
- an **inventory** of what can be pulled: recent snapshot ids, recent command-span ids,
  approximate counts of further timeline text.

Pulled through the terminal tool catalog: earlier snapshots, snapshot diffs, longer
timeline ranges, raw PTY bytes, previous-question anchors.

The inventory is the anti-passivity mechanism: an agent fails to pull mostly because it
does not know what exists. Advertising the pullable material explicitly — plus a
deterministic static instruction in the persistent agent session, such as "if the pushed
span is truncated or annotated still-running, fetch a fresh snapshot before answering" —
turns pulling from a curiosity problem into an instruction-following problem.

## Conversation scope

One persistent AI conversation per terminal session. The foreground terminal process
requests a new conversation by default; the shared daemon holds multiple concurrent
conversation states keyed by terminal session, and discards a conversation when its
terminal disconnects. Attaching multiple terminals to one shared conversation is a
deliberate future capability, not built now.

## Decision

- Mirror input = the full byte stream written to the user's terminal (PTY plus
  presentation output), in order; AI lines carry a fixed recognizable prefix or style.
- Presentation buffers the bounded side: command ended → buffer the shell leftover and
  stream AI first; still running → program output flows in real time and the AI response
  is inserted block-atomically at the next quiescence gap, with a bounded max-wait.
- Context package = anchor plus inventory (question, span tail, current screen, session
  facts, pullable-material inventory); everything deeper is pulled through tools, guided
  by deterministic static instructions.
- One conversation per terminal session, held in the shared daemon; discarded on
  disconnect; multi-terminal attach deferred.

## Open issues

- The exact pushed-package schema and size budgets are not pinned (open since
  rewrite-0001, where `context_package` is opaque JSON on the wire).
- The AI-output prefix or style is not designed.
- The quiescence-gap insertion and max-wait values need dogfooding alongside the
  stabilization debounce tiers of design 0001.
- Conversation transcript growth over a long session (compaction, `#? /new`) is not
  specified.

## Resolution conditions

- Pin the context-package schema and budgets when the daemon starts consuming the
  package.
- Design the AI-output prefix or style with the presentation layer.
- Tune gap insertion and max-wait during real-PTY dogfooding of the AI stage.
- Specify the transcript lifecycle together with session commands.

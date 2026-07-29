# Design 0022 — User instructions from AGENTS.md

Date: 2026-07-29 11:08 CST +0800

Status: implemented. `AGENTS.md` in the config directory is loaded into the system
prompt (`config.ts`, `prompt.ts`, `agent-runtime.ts`, `server.ts`), covered by
`test/config.test.ts`, `test/prompt.test.ts`, and `test/agent-runtime.test.ts`.

## Why, and the wrong first attempt

The request was to take the useful parts of the owner's personal agent preferences
(`~/.claude/CLAUDE.md`) and put them into Koshell's system prompt. That was done first,
as five hardcoded rules. The owner then corrected it: koshell should read an `AGENTS.md`
from its config directory instead.

The correction is right, and worth recording as a general principle rather than a
one-off. The first attempt failed a test it should have been held to from the start:
**it wrote one person's preferences into the defaults every other user gets.** The
extraction work was not wasted — the filtering was sound — but the destination was. A
preference about tone, depth, or language is the user's to state, in a file they own;
baking it into the product makes it something they must discover and cannot change.

The tell was visible before the correction: the original design had to argue that these
were defensible as product voice, and had to flag "these were one person's preferences
and koshell ships to strangers" as an open judgment call. A rule that needs that
argument is a rule in the wrong place.

## What stayed hardcoded, and why exactly two

Two of the five rules survived, because neither is a preference:

- **Secrets are not echoed, and their exposure is reported.** Terminal output routinely
  carries tokens and keys. The rule tells the model not to repeat one back, and to say
  when one is visible — because whatever was on screen already reached the model
  provider inside this request's context package. That last clause is a fact about
  koshell's architecture that a user writing `AGENTS.md` has no way to know, and a
  privacy default should not be opt-in. The user cannot un-send it; they can rotate it,
  and only if told.
- **Capabilities are answered from the prompt, not guessed.** This is about koshell's
  own tool catalog, and it closes an observed failure (design 0019: asked which search
  it used, the agent replied it had no such tool). Nothing about it is a matter of taste.

The other three — label inferences as inferences, do not assume the question's premise,
name a command's cost before suggesting it — were reverted. They are good assistant
behavior, but they are exactly the kind of thing a user should be able to ask for,
adjust, or decline. They belong in `AGENTS.md`, and the owner's own file can carry them.

## The file

`$XDG_CONFIG_HOME/koshell/AGENTS.md`, default `~/.config/koshell/AGENTS.md` — next to
`koshell.toml`, resolved through the same `resolveConfigDir()`.

- **User-global, not project-scoped.** pi's `DefaultResourceLoader` discovers `AGENTS.md`
  by walking up from the cwd, and koshell's loader has always returned an empty list to
  disable that. It stays disabled. A repository's `AGENTS.md` is written for agents that
  write code in that repository; silently feeding it to a terminal explainer because the
  user happened to `cd` somewhere would be surprising, and would make the assistant's
  behavior change under them as they move around the filesystem.
- **Not routed through pi's `getAgentsFiles()`.** Koshell already overrides
  `getSystemPrompt` wholesale, and pi renders context files at a position and with a
  wrapper koshell does not control. Assembling the text here keeps placement and framing
  ours rather than an upstream detail that can shift between pi releases.
- **Optional and silent when absent.** Most users will never create it. A blank file
  means "no instructions", not "an empty instruction".
- **Bounded at 16 KiB**, keeping the head. This text is prepended to every `#?` for the
  life of the conversation, so an unbounded file would quietly spend the budget the
  terminal evidence needs. The head is kept — the opposite of how terminal context is
  trimmed — because instructions are written most-important-first, while terminal output
  ends with the part being asked about. Truncation is stated in the prompt: a silently
  clipped file would have the model confidently following half a policy.
- **An unreadable file is reported, not skipped.** Missing returns nothing; a permission
  error is logged once and the answer proceeds. Silently dropping instructions the user
  believes are in force is the worse failure.

## Trust and precedence

The file is **trusted input**, unlike search results and terminal output. Only the user
writes it, on their own machine, in a directory only they control — the same trust model
`koshell.toml(5)` already applies to an `!command` credential. So it is quoted plainly
rather than fenced off as hostile text.

It is appended after koshell's own rules, with three things stated:

1. It decides tone, depth, format, and language, and is **preferred over the built-in
   style guidance**. Without this, "be concise, your answer renders inline in a
   terminal" quietly overrides an explicit request for detail — the file would be
   accepted and then ignored on exactly the axis it exists for.
2. It does **not** relax the rules above: observe-and-explain-only, ground claims in the
   terminal context, never claim to have run anything. Those are product guarantees, and
   observe-only is enforced structurally by the tool catalog regardless of what any
   prompt says.
3. Its path, so the user can find what is steering the answers.

## Reload

`AGENTS.md` is part of the system prompt, which is fixed when the conversation is
created. It is therefore in `configurationFingerprint`, so `koshell reload` rebuilds the
agent and reports that the transcript was discarded — the same treatment `[search]` gets,
and for the same reason. The **text** is fingerprinted rather than an mtime: rewriting a
file to identical bytes is not a change worth losing a conversation over.

The server's reload path reads the file itself, alongside the config, because the
fingerprint it compares has to describe the same inputs the next agent will be built
from.

## Reporting it in `koshell status`

Two failures here are silent by construction, and both were fixed the same day by
adding an `instructions` block to the additive `instance_status` reply:

1. **A file the daemon never looks at.** A typo in the filename, or a file written to a
   different config directory, produces exactly the same behavior as no file at all.
   So the **path is printed in every case, including "none at ..."** — a bare "none"
   would send the user back to re-read a file that was never the problem.
2. **A file that is right on disk but not in effect.** The prompt is fixed at
   conversation creation, so an edit lands only on `koshell reload`. Status therefore
   reads the file fresh _and_ compares it against the text the live conversation was
   actually built from (`KoshellAgent.userInstructions`, cached on the connection
   alongside the configuration fingerprint). Reporting only the first would make an
   edited-but-unapplied file indistinguishable from one being followed.

Also reported: the loaded size, whether the 16 KiB ceiling truncated it, and a read
error with its reason — a permission problem must not render as "you have no
instructions file".

The daemon computes all of this rather than the terminal resolving the path itself,
because the two processes can disagree about `XDG_CONFIG_HOME` and the path that
matters is the one actually read. The field is optional on the wire: a daemon that
predates it omits the key, and the terminal prints nothing rather than claiming "none"
— an older daemon genuinely does not read the file, which is a different fact.

## Open issues

- **Nothing measures the cost.** A long `AGENTS.md` is charged against every `#?` in the
  conversation, and the 16 KiB ceiling was chosen by judgment, not from data. No event
  records how large the loaded file was.
- **The precedence wording is untested against a real conflict.** Whether a model
  actually prefers the user's "explain at length" over the built-in "be concise" is
  asserted only by the prompt containing the sentence.
- **The secrets rule has no detection behind it.** The prompt tells the model to notice a
  credential; nothing in koshell scans the context package for one. A model that misses
  it produces no warning and the user gets no independent signal — and by then the
  material has already reached the model provider.

## Resolution conditions

- ~~Report the loaded instructions file in `koshell status`.~~ Done 2026-07-29; see the
  section above. It is asserted by unit tests on both sides and has not yet been run
  against a real daemon.
- Revisit the 16 KiB ceiling only against a real file that hits it; until then it is a
  guess and should be described as one.
- If dogfooding shows a secret reaching an answer unflagged, design terminal-side
  redaction before the context leaves the machine — the only place it would help — rather
  than strengthening the prompt wording.

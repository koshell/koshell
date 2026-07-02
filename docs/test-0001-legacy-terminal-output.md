# Test 0001 — legacy terminal output compatibility

## Why

The project owner asked for a test-only check that Koshell output does not become fragile on old terminal environments, such as terminals without True Color or color support.

## When

Performed at: 2026-07-02 18:39:17 CST +0800.

## How

- Added a presentation unit regression that exercises Koshell-owned AI headers, waiting notices, release notices, streamed text, block text, and error notices, then asserts the rendered bytes do not contain True Color, 256-color, or OSC 8 hyperlink escape sequences.
- Added a real-PTY regression that runs the real `koshell` binary with `TERM=dumb`, `NO_COLOR=1`, and no `COLORTERM`, asks a `#?` question with no daemon available, and asserts the graceful-degrade feedback remains text-readable and avoids the same rich terminal sequences.

## Open Issues

- The current placeholder style may still use basic SGR dim/reset sequences. These tests intentionally guard against richer terminal features becoming required; they do not yet specify a full no-ANSI rendering mode for `NO_COLOR` or `TERM=dumb`.

## Resolution Conditions

- If Koshell later chooses to honor `NO_COLOR` or `TERM=dumb` by suppressing all SGR output, add behavior tests for that policy and update the presentation implementation accordingly.

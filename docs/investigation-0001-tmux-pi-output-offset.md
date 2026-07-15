# Investigation 0001 — tmux/pi output offset under global Koshell

Date: 2026-07-15 09:59:26 CST +0800

Live counterexample update: 2026-07-15 10:09:33 CST +0800

Targeted refresh confirmation: 2026-07-15 10:27:46 CST +0800

## Why

When Koshell is installed as the global interactive-shell wrapper, pi can intermittently
render output at incorrect terminal positions inside tmux. Entering and leaving tmux
copy mode repairs the display. The investigation needed to determine whether Koshell,
pi, or tmux owned the mismatch.

## Finding

The isolated investigation proved one real timing race in tmux 3.7b's incoming DEC
synchronized-output handling. Koshell does not alter pi's visible bytes, but its extra
PTY changes their chunking and scheduling enough to expose that race frequently.

A subsequent live incident on tmux `next-3.8` proved that this race is not a complete
root cause for the reported symptom. The broader issue remains an unconfirmed physical
display divergence: tmux's pane state stays usable, while a forced tmux redraw repairs
the client. The 3.7b race is therefore a contributing defect and reproducer, not a
complete explanation of every observed incident.

The installed and running tmux was verified as 3.7b at `e802909d`, not the local
`master` branch. `/usr/local/bin/tmux` and the binary built in `~/Projects/tmux` had the
same SHA-256. The running server was started after that binary was installed. The local
`master` was `fcce73af` and identified itself as `next-3.8`.

pi-tui 0.80.7 wraps full and differential renders in `CSI ?2026 h` and
`CSI ?2026 l`. Koshell reads the inner PTY into 8192-byte buffers and forwards the
visible stream through its processor thread. This preserves byte order and content, but
one pi write can reach tmux in several bursts.

In tmux 3.7b, ending a synchronized update clears `MODE_SYNC` and queues
`PANE_REDRAW`. If the next pi frame starts before the client processes that pending
redraw, the sequence is:

1. Frame N ends and sets `PANE_REDRAW`.
2. Frame N+1 starts and sets `MODE_SYNC`.
3. The pending full-pane redraw runs while frame N+1 is incomplete.
4. `screen_redraw_draw_pane()` calls `screen_write_stop_sync()`, cancelling frame N+1's
   synchronization early.
5. The rest of frame N+1 can be emitted incrementally while tmux's internal grid remains
   correct.

This leaves the real terminal temporarily inconsistent with tmux's grid. Copy mode
redraws from that correct grid, which explains why entering and leaving it repairs the
screen.

## Validation

An isolated real-PTY stress test replayed pi-tui 0.80.7 synchronized full renders through
both a bare pane and a Koshell-wrapped pane, against the installed 3.7b and a temporary
build of the true local `master`. Each case consumed the same 821,834-byte pi stream.

| tmux                  | Pane path | Sync starts | Normal sync ends | Mid-frame cancellations | Client output |
| --------------------- | --------- | ----------: | ---------------: | ----------------------: | ------------: |
| 3.7b                  | bare      |          41 |               41 |                       0 | 109,000 bytes |
| 3.7b                  | Koshell   |          41 |               41 |                      25 | 501,531 bytes |
| `master` (`next-3.8`) | bare      |          41 |               41 |                       0 | 286,951 bytes |
| `master` (`next-3.8`) | Koshell   |          41 |               41 |                       0 | 148,882 bytes |

The 3.7b + Koshell server log showed the next `?2026h`, partial frame content, and then a
pending pane redraw calling `screen_write_stop_sync()` before the matching `?2026l`.
All four cases converged to identical internal and physical final screens after output
stopped; the defect exists while rapid frames continue, matching the intermittent report
and the effect of a forced redraw.

The true master no longer queues a full pane redraw at synchronized-update end. Commits
`724f85d2` and `565db46a` suppress obsolete tty drawing and flush only dirty lines at
`screen_write_stop_sync()`, removing the isolated test's overlap window. Commit
`c5a73099` additionally marks the full screen dirty for ED 2, which matters because pi
emits clear-screen before moving the cursor home.

## Live next-3.8 counterexample

While the display was wrong in the remote `notes` session on `sjdhome-macmini`, read-only
metadata showed:

- both the binary and server were `next-3.8` at `fcce73af`;
- two Ghostty clients were attached to the same session, sized 296x74 and 298x74;
- the window used `window-size latest` and was 298x73;
- the active right pane contained pi behind Koshell and was 148x73;
- `synchronized_output_flag` was zero, so the pane was not stuck in sync mode.

Before a targeted `refresh-client` could be run, clicking the left pane caused the right
pane to render correctly again. `/dev/ttys005`, reported from inside that pane, was the
Koshell inner PTY; the actual attached tmux clients were `/dev/ttys003` and
`/dev/ttys001`. The focus change is still useful evidence because it triggered a tmux
redraw without changing pi's content, but the exact affected client and its pre-refresh
physical bytes were not captured.

A second occurrence in the same right pane remained visible while pre-redraw metadata
was sampled. The user confirmed `/dev/ttys001` as the affected client. Before refresh:

- the affected client and window were both 298 columns wide, with no viewport offset;
- the stale 296-column `/dev/ttys003` client was still attached;
- ten samples at 50-millisecond intervals all reported `synchronized_output_flag=0`;
- the 148-column pane reported cursor position `148,34`, the pending-wrap position at its
  right edge;
- the pane had no pending redraw flag.

A single `tmux refresh-client -t /dev/ttys001` immediately repaired the display. Post-
refresh pane size, cursor, sync mode, alternate-screen mode, and flags were unchanged.
No pane focus, resize, copy-mode transition, or pi input was involved.

This is direct confirmation of a client-specific physical display divergence: tmux could
redraw the correct result from its existing state, while the incremental display sent to
that client had become wrong. It rules out "install master" as a complete resolution and
makes a persistent pi viewport-state error unlikely. It does not yet distinguish a tmux
next-3.8 outgoing tty bug from a Ghostty interpretation bug. The second attached client,
different client widths, synchronized dirty-line flushing, and the right-edge
pending-wrap cursor are the strongest remaining conditions absent from the original
isolated test.

Related upstream reports:

- tmux issue 4983: <https://github.com/tmux/tmux/issues/4983>
- tmux pull request 5304: <https://github.com/tmux/tmux/pull/5304>
- tmux pull request 5322: <https://github.com/tmux/tmux/pull/5322>

## Open issues

No production pane grid or outer-terminal byte stream was captured while visibly wrong,
by request, so the investigation does not preserve a screenshot or a byte-for-byte trace
of the original incident. The isolated test reproduced the specific 3.7b mid-frame
cancellation only when Koshell was present, but the live next-3.8 incidents prove another
path remains.

The next investigation must distinguish among:

- a tmux next-3.8 per-client redraw defect involving multiple attached client sizes;
- another synchronized-output dirty-line or pending-wrap case not covered by the isolated
  frame;
- a Ghostty state divergence caused by tmux's incremental outgoing sequence.

No Koshell code was changed. Coalescing PTY reads cannot guarantee one tmux read for a
large frame, and stripping DEC 2026 inside tmux would weaken wrapper transparency.

## Resolution conditions

The targeted-refresh test is complete: it repairs the affected client without changing
pane state. The next step is to reproduce the same split layout with two attached clients
at different widths under an isolated `-vv` server, preserving tmux's outgoing client
bytes. The test should include frames ending at the pane's right margin so the cursor is
in pending-wrap position. Comparing those bytes with tmux's internal grid and Ghostty's
physical result can assign the remaining defect to tmux's tty output or the outer
terminal. A production pane capture remains optional and requires explicit permission.

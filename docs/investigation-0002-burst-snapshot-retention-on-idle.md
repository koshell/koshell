# Investigation 0002 — burst snapshots retained forever once a pane goes idle

Date: 2026-07-21 15:29:29 CST

Status: diagnosed; fixed by `fix-0009-burst-snapshot-retention.md`.

## Why

A running `koshell` instance (PID 11217, the tmux pane `notes:1.1`, 148×73,
started 2026-07-13) was observed holding ~400 MB RSS while its inner shell was a
completely idle `zsh`. This is after fix-0007 (timeline retention) shipped — the
process runs the 2026-07-09 binary, which includes the compaction — so the
question was why a bounded store still holds hundreds of MB.

## Findings

`heap 11217` shows the memory is a live heap, not fragmentation or stale pages:

- **28,132 allocations of ~14 KB each ≈ 393 MB** — uniform full-screen snapshot
  strings (a full 148×73 screen of content is the same size every repaint).
- 85,026 × 16-byte allocations (~1.4 MB) and one 4.6 MB block (the `entries`
  Vec at its burst-peak capacity; `Vec::retain` never shrinks capacity) — noise.
- Physical footprint 429 MB, peak 476 MB — the store has barely shrunk since
  its peak.

Two sibling long-lived instances show the same burst signature but _recovered_:

- PID 77281: footprint now 21.7 MB, **peak 282 MB**.
- PID 75695: footprint now 145.7 MB, **peak 2.3 GB** (and a 36 MB `entries`
  Vec capacity left over from that peak).

The difference: their terminals kept producing output, so compaction kept
running and thinned the burst away. PID 11217's pane went silent right after
its burst and never recovered.

## Root cause

The composition of three behaviors, the first two already documented:

1. `TriggerDetector::record_output` (`trigger.rs`) records a **full-screen
   snapshot per PTY read chunk**, unconditionally. Fast output means hundreds
   of snapshots per second.
2. The freshest retention band in `SNAPSHOT_TIERS` (`timeline.rs`) keeps
   **every** snapshot ≤ 1 minute old (`min_spacing_ms: 0`). During a fast
   burst, tens of thousands of snapshots are all "≤ 1 min old" at each
   compaction pass and are all kept. This is the "freshest-band burst" open
   issue recorded in fix-0007.
3. **Compaction only runs inside `record()`** (every `COMPACT_EVERY` = 256
   records). fix-0007 called the freshest-band burst "transient and
   self-limiting" — that holds only while output continues. If the terminal
   goes idle immediately after the burst, no further events are recorded,
   `compact()` never runs again, and the entire burst (28k × 14 KB here) is
   retained **indefinitely**. Age-based retention with append-driven
   compaction is not self-limiting on an idle session.

## Candidate fixes (not implemented)

- Give the freshest band a floor spacing (e.g. 100–250 ms) or a hard snapshot
  count cap, bounding the worst-case burst to a few hundred snapshots.
- Trigger compaction from a timer or from an existing idle path (e.g. the
  stabilization timer or context queries), not only from `record()`, so an
  idle store still ages out.
- Skip the snapshot when the screen text is unchanged from the previous one
  (cheap equality check against `previous_snapshot`).
- `shrink_to_fit` (or re-collect) the `entries` Vec after a compaction that
  drops a large fraction, so peak capacity is not pinned (36 MB observed).

## Resolution conditions

The retained memory in an affected running instance is only released by more
output in that pane (256 events re-triggers compaction — even keystrokes and
prompt redraws count) or by restarting the pane's koshell. The code-level fix
can land any time; the first two candidate fixes together remove both the
burst magnitude and the idle-retention half of the problem.

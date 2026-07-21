# Fix 0009 — burst snapshot retention and idle compaction

Date: 2026-07-21 16:17:10 CST

Status: implemented.

## Why

`investigation-0002-burst-snapshot-retention-on-idle.md`: a koshell instance
(a tmux pane whose shell had gone idle) sat at ~400 MB — 28k live full-screen
snapshot strings — a week after fix-0007 shipped. A sibling instance peaked at
2.3 GB before recovering. Two gaps composed:

- **Policy gap.** The freshest `SNAPSHOT_TIERS` band (≤ 1 min) kept _every_
  snapshot, and a snapshot is recorded per PTY read chunk, so fast output
  accrued hundreds of full-screen copies per second that no compaction pass
  would touch for their first minute. This was fix-0007's known
  "freshest-band burst" open issue; the observed 2.3 GB peak showed it is not
  a theoretical spike.
- **Trigger gap.** Compaction ran only inside `record()`. fix-0007 called the
  burst "transient and self-limiting", which holds only while output
  continues; a burst followed by silence was never compacted again and stayed
  resident indefinitely.

Additionally, `Vec::retain` never returns capacity, so a compacted-away burst
left the `entries` Vec's peak allocation pinned (36 MB observed).

## How

Four changes, all in `timeline.rs` / `trigger.rs` / `session.rs`:

- **Freshest-band floor spacing** (`timeline.rs`). The ≤ 1-minute tier's
  `min_spacing_ms` goes from 0 to 200 — at most ~5 snapshots per second, ~300
  in the band. Bounds the burst _peak_; no current consumer needs more density
  (`context.rs` reads the latest snapshot and the last ~20 changed ones).
- **Hard snapshot byte cap** (`timeline.rs`, `SNAPSHOT_RETENTION_BYTES` =
  32 MB). Compaction's newest-first walk draws snapshot screen bytes from a
  saturating budget after the age tiers; past it, older snapshots are dropped
  regardless of tier. The straddling snapshot is kept, so the newest snapshot
  always survives. Defense-in-depth: with the floor spacing the cap is not
  normally reached, but it holds unconditionally against future policy changes
  and pathological workloads. Enforced at the existing `COMPACT_EVERY` (256)
  cadence — individual events are small (a screen, a PTY chunk), so overshoot
  between passes is a few MB; a separate byte-threshold _trigger_ would add
  nothing beyond that cadence.
- **Idle maintenance tick** (`trigger.rs`, `session.rs`,
  `TIMELINE_MAINTENANCE_INTERVAL` = 60 s). The session loop's channel-wait
  timeout now also considers `SessionState::next_maintenance_delay`, and
  `poll()` re-applies retention via `InMemoryTimelineStore::maintain()` when
  the tick is due — before the pending/alt-screen early return, since
  full-screen programs are the snapshot-heaviest and idleness is exactly when
  the tick matters. The tick disarms (returns `None`) once the timeline is
  empty, so a fully aged-out session blocks indefinitely again. Cost: one
  wake-up per minute plus an O(entries) pass over a bounded store.
- **Capacity shrink** (`timeline.rs`, `SHRINK_MIN_CAPACITY` = 1024).
  After the retain, compaction calls `shrink_to_fit` when the Vec is at least
  the threshold and at most a quarter full.

Combined bounds: realistic worst case ~10 MB of retained snapshots (floor
spacing × tier table), 32 MB hard cap, and an idle session decays to the aged
steady state within a minute of going quiet instead of never.

## Verification

- New `timeline.rs` unit tests: `thins_a_fast_burst_in_the_freshest_band`
  (2000 snapshots at 20 ms intervals compact to ~200),
  `caps_retained_snapshot_bytes_even_when_the_tiers_keep_them` (80 MB of
  tier-kept fresh snapshots retain ≤ the cap, newest kept, oldest dropped),
  `shrinks_pathological_entry_capacity_after_a_purge` (a purge that empties a
  ≥ 1024-capacity Vec releases the allocation).
- New `trigger.rs` unit test:
  `maintenance_ticks_only_while_the_timeline_has_entries_and_reschedules_on_poll`.
- Existing retention tests unchanged and passing (the 1-per-second density
  assertion in `downsamples_old_snapshots_by_age_tier` clears the 200 ms
  floor).
- `cargo test -p koshell-rs` (175 unit + integration suites), `cargo fmt
--check`, `cargo clippy --all-targets -- -D warnings`.

## Open issues

- Already-running instances only pick this up on restart of their koshell
  process; until then an affected idle instance releases its memory only if
  its pane produces ~256 more events (any keystrokes/output) on a pre-fix
  binary — the pre-fix compaction _will_ thin an aged-out burst once it runs.
- The screen-unchanged snapshot dedup considered in the investigation was
  deliberately not included (it does not bound bursts and would mix an
  unrelated behavior change into this fix). Revisit if steady-state snapshot
  churn ever matters.
- `MAINTENANCE` ticks use the session's monotonic clock while retention ages
  use wall-clock epoch ms; after a laptop sleep the first tick on wake applies
  the full elapsed wall time, which is the desired behavior.

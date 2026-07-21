# Fix 0007 — timeline memory retention

Date: 2026-07-07 14:07:25 CST

Status: implemented.

## Why

Three long-lived `koshell` instances were reported as running out of memory.
Investigation (`ps` said their RSS was tiny — 4–116 MB — but `vmmap` reported
physical footprints of 5.0 G, 96.8 G, and 12.9 G, almost entirely
compressed/swapped) traced the growth to `MALLOC_SMALL`: the worst process held
**35.5 million live small allocations** totalling 98 GB. This was an unbounded
heap accumulation, not an OOM kill (the processes were never reaped).

The three instances mapped to Ghostty windows whose shells had each attached a
tmux session (`personal`, `btop`, `phi-ai`). The footprint tracked output
volume, not uptime: the `btop` session — a full-screen monitor repainting every
second — was the 96.8 G outlier.

### Root cause

`InMemoryTimelineStore` (`timeline.rs`) was append-only with no cap and no
production caller of `reset()`. It sits on the PTY-read hot path: every burst of
terminal output runs `TriggerDetector::record_output` (`trigger.rs`), which
appends **two** entries — a `PtyOutput` holding a copy of the raw bytes and a
`ScreenSnapshot` holding a full copy of the visible screen (≈ up to 20 KB at
298×74) plus a diff summary. Every presentation write (`record_presentation_output`)
appends another full snapshot. Over ~26 hours under a redraw-heavy inner program
this grew without bound.

The terminal mirror itself (`mirror.rs`, `alacritty_terminal`) was _not_ the
leak: it is bounded by a 2000-line scrollback. Only the timeline "ledger" grew.

All timeline queries are recent-window bounded — `context.rs` reads the last
N characters of PTY/visible/input text (≤ 8000), the latest snapshot, and the
last ~20 changed snapshots — so nothing ever read the ancient tail.

## How

A tiered, in-memory retention policy on `InMemoryTimelineStore`, applied by a
`compact()` pass that runs every `COMPACT_EVERY` (256) records. Nothing is ever
written to disk, so the privacy invariant of
`design-0007-dogfooding-event-log.md` is preserved.

### Screen snapshots — age-tiered downsampling (GFS-style)

Instead of retaining every per-second repaint, snapshots are thinned by age:
recent detail is dense, older detail is coarse, giving a browsable multi-scale
visual history across a week at a bounded snapshot count.

| Age band | Retained resolution |
| -------- | ------------------- |
| ≤ 1 min  | every snapshot      |
| ≤ 5 min  | ≤ 1 per 5 s         |
| ≤ 30 min | ≤ 1 per 30 s        |
| ≤ 1 h    | ≤ 1 per 1 min       |
| ≤ 12 h   | ≤ 1 per 5 min       |
| ≤ 24 h   | ≤ 1 per 15 min      |
| ≤ 7 days | ≤ 1 per 1 h         |
| > 7 days | dropped             |

Compaction walks newest-to-oldest and keeps a snapshot only when it sits at
least its band's spacing before the last snapshot it kept. The old bands are
bounded to a few hundred snapshots total (~450); steady-state memory is a few MB.

### Raw text — recent-character budget

`PtyOutput`, `VisibleOutput`, and visible `HumanInput` feed only the
recent-window queries, so they are not tiered: compaction keeps the most recent
events of each kind until their combined text reaches `RAW_TEXT_RETENTION_CHARS`
(32 000, well above the largest 8000-char query budget), then drops the rest.

### Everything else

`CommandStart`/`CommandEnd`/`AiRequest`/`AiResponse`/`ControlModeChange` and
hidden `HumanInput` are tiny and low-frequency; they age out at `MAX_AGE_MS`
(7 days).

### Cost

`compact()` is O(n) but throttled to every 256 records, and retains entries in
place via an index-keyed `Vec::retain` (no snapshot-string clones). Between
compactions the store overshoots by at most a few hundred entries.

## Audit — other unbounded-growth points

With the timeline fixed, the rest of the per-session state was swept for
activity-proportional growth that is never released:

- **`MarkerScanner.buf` (`shell_integration.rs`) — hardened here.** The
  chunk-boundary scanner held everything after a `MARKER_PREFIX` until its BEL
  terminator arrived, with no cap. A spurious prefix in program output (binary
  data, a file containing the OSC 777 `koshell;` sequence, an unterminated
  marker) could grow the buffer with all following bytes. The 14-byte prefix
  makes this low-probability, but it was a genuine unbounded path (and an
  O(n²) rescan). Capped at `MAX_PENDING_MARKER_BYTES` (64 KB): past the cap the
  bytes are flushed as literal output — a real marker, even one carrying a long
  command line, is far under it.
- **`Presentation.dispatched` / `aborted`, `ActiveResponse.accumulated` /
  `buffered_pty` — already bounded.** The maps are keyed by request id and
  removed on finish/interrupt (paired inserts/removes); the per-response buffers
  die with the response and `buffered_pty` has a 256 KB fuse.
- **`TriggerDetector.pending`, `TerminalMirror` — already bounded.** `pending`
  is drained on fire/cancel; the mirror is a fixed 2000-line scrollback.
- **AI daemon pi conversation (`agent-runtime.ts`) — bounded by session
  lifetime, by design.** History is not compacted, so it grows per `#?` turn
  (not per output byte) and is discarded when the terminal disconnects. This is
  a documented limitation; compaction / `#? /new` is a separate, unspecified
  stage. The daemon keeps no server-level session registry, and its per-request
  `cancelled` set is per-connection and paired.

## Verification

- `timeline.rs` unit tests: `downsamples_old_snapshots_by_age_tier` (2400
  once-per-second snapshots collapse to < 300 with a dense last minute and
  ≥ 30 s spacing beyond the 5-minute band), `keeps_recent_raw_text_within_budget_and_drops_older`
  (100 × 1000-char bursts collapse to the budget's worth while
  `get_recent_pty_output(8000)` stays full), and
  `small_store_is_left_untouched_by_compaction`.
- `shell_integration.rs` unit test:
  `incomplete_marker_buffer_is_capped_against_a_spurious_prefix` (an unterminated
  prefix plus a > 64 KB blob is flushed as visible output, no marker fabricated,
  scanner buffer stays bounded).
- `cargo test -p koshell-rs`, `cargo fmt --check`,
  `cargo clippy --all-targets -- -D warnings`.
- The three leaking processes recover their memory on restart of the affected
  Ghostty windows; the tmux servers/sessions survive (only the koshell wrapper
  and its tmux client die), so re-attaching loses no session state.

## Open issues

- **Freshest-band burst.** The ≤ 1-minute band keeps every snapshot, so a
  program emitting output at a very high read rate can momentarily hold tens of
  thousands of snapshots (tens of MB) before they age out and thin. This is
  transient and self-limiting; realistic workloads (a ~1 Hz `btop` repaint) sit
  at ~60. Add a floor spacing to the freshest band only if a real workload shows
  a problematic spike. **Resolved by fix-0009**: a real workload showed a 2.3 GB
  spike, and "transient" proved false for a burst followed by silence (record-
  driven compaction never ran again). See
  `fix-0009-burst-snapshot-retention.md`.
- **The coarse multi-day tail has no consumer yet.** Context assembly
  (`context.rs`) only reads the recent window, so the ≤ 7-day snapshot history
  is currently forward-looking — retained for a future "what happened across
  this session" view. It costs only a few hundred snapshots, so it is kept.
- **On-demand snapshot diff across the tiers.** `diff_screen_snapshots` by id
  still works for retained snapshots; a diff referencing a downsampled-away id
  degrades to the existing "not found" error.

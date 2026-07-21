//! Bounded in-memory terminal timeline, ported from `reference/src/timeline.ts`. Pure
//! data plus queries; no I/O. Events are appended in order, but a long-lived session is
//! kept bounded by age-tiered snapshot downsampling, a hard snapshot byte cap, and a
//! recent-character budget for raw text (see [`InMemoryTimelineStore`],
//! `fix-0007-timeline-memory-retention.md`, and `fix-0009-burst-snapshot-retention.md`).

use std::time::{SystemTime, UNIX_EPOCH};

use crate::screen_diff::{ScreenDiffSummary, ScreenTextDiff, diff_screen_text};

/// Terminal control mode. Reserved for the policy/control-mode work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlMode {
    Human,
    Shared,
    Ai,
}

/// An append-only terminal fact. Timestamps live on [`TimelineEntry`], not the event.
#[derive(Debug, Clone, PartialEq)]
pub enum TerminalEvent {
    HumanInput {
        data: String,
        /// `false` marks hidden input (e.g. passwords), excluded from text context.
        visible: bool,
    },
    PtyOutput {
        data: String,
    },
    VisibleOutput {
        text: String,
    },
    CommandStart {
        command_id: String,
        command: String,
        cwd: Option<String>,
    },
    CommandEnd {
        command_id: String,
        command: String,
        exit_code: Option<i32>,
        duration_ms: Option<u64>,
    },
    ScreenSnapshot {
        snapshot_id: String,
        rows: u16,
        columns: u16,
        alt_screen: bool,
        screen: Option<String>,
        previous_snapshot_id: Option<String>,
        diff: Option<ScreenDiffSummary>,
    },
    AiRequest {
        request_id: String,
        question: String,
    },
    AiResponse {
        request_id: String,
        text: String,
    },
    ControlModeChange {
        from: ControlMode,
        to: ControlMode,
        reason: String,
    },
}

/// A recorded event with its assigned id and timestamp.
#[derive(Debug, Clone, PartialEq)]
pub struct TimelineEntry {
    pub id: String,
    pub ts: i64,
    pub event: TerminalEvent,
}

/// On-demand detailed diff between two screen snapshots.
#[derive(Debug, Clone, PartialEq)]
pub struct ScreenSnapshotDiff {
    pub from_snapshot_id: String,
    pub to_snapshot_id: String,
    pub diff: ScreenTextDiff,
}

type Clock = Box<dyn Fn() -> i64 + Send + Sync>;

/// One age band of the screen-snapshot retention policy: within `max_age_ms` of now,
/// keep at most one snapshot per `min_spacing_ms`. Bands are consulted newest-first, so
/// a snapshot lands in the first band whose `max_age_ms` covers its age.
struct RetentionTier {
    max_age_ms: i64,
    min_spacing_ms: i64,
}

/// Age-tiered downsampling for [`TerminalEvent::ScreenSnapshot`], a GFS-style rotation:
/// dense for the last minute, thinning out to hourly across a week, then dropped.
///
/// Without it the store leaked gigabytes — a tmux session running `btop` repaints the
/// whole screen every second, so an unbounded store accrued millions of full-screen
/// snapshot strings over a day (tens of GB of compressed/swapped memory). Keeping a
/// coarse multi-scale history instead of every per-second repaint bounds memory to a
/// few MB while preserving a browsable visual timeline.
///
/// The freshest band has a floor spacing rather than "keep all": a snapshot is recorded
/// per PTY read chunk, so fast output produces hundreds of full-screen snapshots per
/// second and an uncapped fresh band ballooned to gigabytes within its first minute
/// (observed at a 2.3 GB peak; see `fix-0009-burst-snapshot-retention.md`). 200 ms
/// keeps the last minute far denser than any current consumer reads (`context.rs` uses
/// the latest snapshot and the last ~20 changed ones) while bounding a burst.
///
/// In-memory only — nothing here is ever written to disk, upholding the privacy
/// invariant of `design-0007-dogfooding-event-log.md`.
const SNAPSHOT_TIERS: &[RetentionTier] = &[
    RetentionTier {
        max_age_ms: 60_000,
        min_spacing_ms: 200,
    }, // <= 1 min: 1 / 200ms
    RetentionTier {
        max_age_ms: 300_000,
        min_spacing_ms: 5_000,
    }, // <= 5 min: 1 / 5s
    RetentionTier {
        max_age_ms: 1_800_000,
        min_spacing_ms: 30_000,
    }, // <= 30 min: 1 / 30s
    RetentionTier {
        max_age_ms: 3_600_000,
        min_spacing_ms: 60_000,
    }, // <= 1 h: 1 / min
    RetentionTier {
        max_age_ms: 43_200_000,
        min_spacing_ms: 300_000,
    }, // <= 12 h: 1 / 5min
    RetentionTier {
        max_age_ms: 86_400_000,
        min_spacing_ms: 900_000,
    }, // <= 24 h: 1 / 15min
    RetentionTier {
        max_age_ms: 604_800_000,
        min_spacing_ms: 3_600_000,
    }, // <= 7 d: 1 / h
];

/// The horizon past which any event is dropped: the oldest snapshot tier's `max_age_ms`
/// (7 days). Reused as the cap for the low-volume bookkeeping events (command/AI/mode
/// changes) that are neither snapshots nor recent-window text.
const MAX_AGE_MS: i64 = 604_800_000;

/// Retained-character budget, per raw-text event kind, for the events that feed the
/// recent-window context queries (`PtyOutput`, `VisibleOutput`, visible `HumanInput`;
/// see `context.rs`). Compaction keeps the most recent events of each kind until their
/// combined text reaches this many characters, then drops the older ones. Set well
/// above the largest query budget (8000 chars) so recent context is always intact.
const RAW_TEXT_RETENTION_CHARS: usize = 32_000;

/// Run compaction once this many events have been recorded since the last pass. Bounds
/// the between-compaction overshoot to a handful of entries while amortizing the O(n)
/// pass so it does not run on every `record`.
const COMPACT_EVERY: usize = 256;

/// Hard cap on the total retained snapshot screen text, enforced unconditionally by
/// compaction — newest snapshots first, dropping the oldest past the budget even when
/// their age tier would keep them. The age tiers bound realistic retention to a few MB;
/// this budget is the defense-in-depth guarantee that no tier-policy change or
/// pathological workload can hold more than a fixed number of bytes. At the observed
/// ~14 KB per full-screen snapshot it admits roughly 2 300 snapshots. Overshoot between
/// compaction passes is at most [`COMPACT_EVERY`] events' worth (a few MB).
const SNAPSHOT_RETENTION_BYTES: usize = 32 * 1024 * 1024;

/// `entries` keeps its peak capacity across `Vec::retain`, so a compacted-away burst
/// left the Vec's allocation pinned at burst size (36 MB observed). Compaction shrinks
/// the Vec when it is at least this large and at most a quarter full; the threshold
/// keeps the steady state free of realloc churn.
const SHRINK_MIN_CAPACITY: usize = 1024;

/// In-memory, bounded terminal timeline store with an injectable clock. Screen
/// snapshots are downsampled by age ([`SNAPSHOT_TIERS`]) under a hard byte cap
/// ([`SNAPSHOT_RETENTION_BYTES`]); raw-text events are held to a recent-character
/// budget ([`RAW_TEXT_RETENTION_CHARS`]); everything else ages out at [`MAX_AGE_MS`].
/// Compaction runs every [`COMPACT_EVERY`] records, plus on the session loop's idle
/// [`Self::maintain`] tick.
pub struct InMemoryTimelineStore {
    entries: Vec<TimelineEntry>,
    now: Clock,
    next_id: u64,
    records_since_compaction: usize,
}

impl Default for InMemoryTimelineStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryTimelineStore {
    /// Creates a store using the system wall clock (milliseconds since epoch).
    pub fn new() -> Self {
        Self::with_clock(default_now)
    }

    /// Creates a store with an injectable clock (used by tests).
    pub fn with_clock<F: Fn() -> i64 + Send + Sync + 'static>(now: F) -> Self {
        Self {
            entries: Vec::new(),
            now: Box::new(now),
            next_id: 1,
            records_since_compaction: 0,
        }
    }

    /// Records an event, assigning it a deterministic id and the current timestamp.
    /// Returns the created entry. Compaction (age-tiered snapshot downsampling plus the
    /// recent-character budget) runs every [`COMPACT_EVERY`] records, so the store stays
    /// bounded across a long-lived session. Ids keep climbing monotonically, so a
    /// compacted-away snapshot's id simply becomes unknown to the on-demand snapshot
    /// lookups (which already degrade to "not found").
    pub fn record(&mut self, event: TerminalEvent) -> TimelineEntry {
        let id = format!("event-{}", self.next_id);
        self.next_id += 1;
        let entry = TimelineEntry {
            id,
            ts: (self.now)(),
            event,
        };
        self.entries.push(entry.clone());
        self.records_since_compaction += 1;
        if self.records_since_compaction >= COMPACT_EVERY {
            self.compact();
        }
        entry
    }

    /// Applies the retention policy, keeping the newest entries and discarding the rest.
    /// Walks newest-to-oldest so "keep the most recent" decisions are a single pass:
    /// snapshots thin against the last one kept ([`SNAPSHOT_TIERS`]), raw-text events
    /// draw down a per-kind character budget ([`RAW_TEXT_RETENTION_CHARS`]), and the
    /// low-volume bookkeeping events simply age out at [`MAX_AGE_MS`]. Retains entries in
    /// place (no snapshot-string clones) via an index-keyed `retain`.
    fn compact(&mut self) {
        self.records_since_compaction = 0;
        let now = (self.now)();

        // Decide keep/drop newest-first; `keep[i]` mirrors `entries[i]`.
        let mut keep = vec![false; self.entries.len()];
        let mut last_snapshot_ts: Option<i64> = None;
        let mut snapshot_budget = SNAPSHOT_RETENTION_BYTES;
        let mut pty_budget = RAW_TEXT_RETENTION_CHARS;
        let mut visible_budget = RAW_TEXT_RETENTION_CHARS;
        let mut input_budget = RAW_TEXT_RETENTION_CHARS;

        for (idx, entry) in self.entries.iter().enumerate().rev() {
            let age = now.saturating_sub(entry.ts);
            keep[idx] = match &entry.event {
                TerminalEvent::ScreenSnapshot { screen, .. } => {
                    // The byte budget applies after the age tiers and saturates like the
                    // raw-text budgets (the straddling snapshot is kept, so the newest
                    // snapshot always survives); once it runs out, every older snapshot
                    // is dropped regardless of tier.
                    keep_snapshot(age, entry.ts, &mut last_snapshot_ts)
                        && take_from_budget(
                            &mut snapshot_budget,
                            screen.as_ref().map_or(0, |s| s.len()),
                        )
                }
                TerminalEvent::PtyOutput { data } => {
                    take_from_budget(&mut pty_budget, data.chars().count())
                }
                TerminalEvent::VisibleOutput { text } => {
                    take_from_budget(&mut visible_budget, text.chars().count())
                }
                TerminalEvent::HumanInput { data, visible } => {
                    if *visible {
                        take_from_budget(&mut input_budget, data.chars().count())
                    } else {
                        // Hidden input carries no context text; keep it only as history.
                        age <= MAX_AGE_MS
                    }
                }
                _ => age <= MAX_AGE_MS,
            };
        }

        let mut idx = 0;
        self.entries.retain(|_| {
            let keep_this = keep[idx];
            idx += 1;
            keep_this
        });

        // `retain` never returns capacity, so a burst's peak allocation would stay
        // pinned for the session's lifetime once the entries age out.
        if self.entries.capacity() >= SHRINK_MIN_CAPACITY
            && self.entries.len() <= self.entries.capacity() / 4
        {
            self.entries.shrink_to_fit();
        }
    }

    /// Re-applies the retention policy outside the record-driven cadence. Compaction
    /// otherwise runs only inside [`Self::record`], so a burst followed by silence kept
    /// its freshest-band snapshots forever (the ~400 MB idle instance of
    /// `investigation-0002-burst-snapshot-retention-on-idle.md`). The session loop calls
    /// this from a periodic idle tick so retained entries keep aging out.
    pub fn maintain(&mut self) {
        self.compact();
    }

    pub fn list_entries(&self) -> &[TimelineEntry] {
        &self.entries
    }

    pub fn list_events(&self) -> impl Iterator<Item = &TerminalEvent> {
        self.entries.iter().map(|entry| &entry.event)
    }

    /// Recent human-visible terminal text across event kinds, trimmed to `max` chars.
    pub fn get_recent_text(&self, max: usize) -> String {
        let text: String = self
            .entries
            .iter()
            .map(|e| event_to_text(&e.event))
            .collect();
        trim_start_to_max_characters(&text, max)
    }

    /// Recent raw PTY output, trimmed to `max` chars.
    pub fn get_recent_pty_output(&self, max: usize) -> String {
        let text: String = self
            .entries
            .iter()
            .filter_map(|e| match &e.event {
                TerminalEvent::PtyOutput { data } => Some(data.as_str()),
                _ => None,
            })
            .collect();
        trim_start_to_max_characters(&text, max)
    }

    pub fn list_screen_snapshots(&self) -> Vec<&TimelineEntry> {
        self.entries
            .iter()
            .filter(|e| matches!(e.event, TerminalEvent::ScreenSnapshot { .. }))
            .collect()
    }

    pub fn get_screen_snapshot(&self, snapshot_id: &str) -> Option<&TimelineEntry> {
        self.entries.iter().find(|e| match &e.event {
            TerminalEvent::ScreenSnapshot {
                snapshot_id: id, ..
            } => id == snapshot_id,
            _ => false,
        })
    }

    pub fn get_latest_screen_snapshot(&self) -> Option<&TimelineEntry> {
        self.find_latest_screen_snapshot(|_| true)
    }

    pub fn get_latest_alternate_screen_snapshot(&self) -> Option<&TimelineEntry> {
        self.find_latest_screen_snapshot(|alt_screen| alt_screen)
    }

    fn find_latest_screen_snapshot(
        &self,
        predicate: impl Fn(bool) -> bool,
    ) -> Option<&TimelineEntry> {
        self.entries.iter().rev().find(|e| match &e.event {
            TerminalEvent::ScreenSnapshot { alt_screen, .. } => predicate(*alt_screen),
            _ => false,
        })
    }

    /// Diffs two recorded snapshots by id. Errors if either id is missing.
    pub fn diff_screen_snapshots(
        &self,
        from_snapshot_id: &str,
        to_snapshot_id: &str,
    ) -> anyhow::Result<ScreenSnapshotDiff> {
        let from = self.required_snapshot_screen(from_snapshot_id)?;
        let to = self.required_snapshot_screen(to_snapshot_id)?;
        Ok(ScreenSnapshotDiff {
            from_snapshot_id: from_snapshot_id.to_string(),
            to_snapshot_id: to_snapshot_id.to_string(),
            diff: diff_screen_text(&from, &to),
        })
    }

    fn required_snapshot_screen(&self, snapshot_id: &str) -> anyhow::Result<String> {
        match self.get_screen_snapshot(snapshot_id) {
            Some(TimelineEntry {
                event: TerminalEvent::ScreenSnapshot { screen, .. },
                ..
            }) => Ok(screen.clone().unwrap_or_default()),
            _ => anyhow::bail!("Screen snapshot {snapshot_id:?} was not found."),
        }
    }

    pub fn reset(&mut self) {
        self.entries.clear();
    }
}

fn default_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The retention spacing for a snapshot of the given age, or `None` when it is older
/// than the last tier and must be dropped. Tiers are ordered youngest-first, so the
/// first match is the tightest band that still covers the age.
fn snapshot_spacing_for_age(age_ms: i64) -> Option<i64> {
    SNAPSHOT_TIERS
        .iter()
        .find(|tier| age_ms <= tier.max_age_ms)
        .map(|tier| tier.min_spacing_ms)
}

/// Downsampling decision for one snapshot, called newest-first. `last_kept_ts` is the
/// timestamp of the most recent snapshot already kept; this one is kept when it sits at
/// least its tier's spacing before that neighbour (and always when it is the newest, or
/// its band keeps all). Updates `last_kept_ts` on a keep.
fn keep_snapshot(age_ms: i64, ts: i64, last_kept_ts: &mut Option<i64>) -> bool {
    let Some(spacing) = snapshot_spacing_for_age(age_ms) else {
        return false;
    };
    let keep = match *last_kept_ts {
        None => true,
        Some(newer_ts) => newer_ts.saturating_sub(ts) >= spacing,
    };
    if keep {
        *last_kept_ts = Some(ts);
    }
    keep
}

/// Draws `cost` characters from a retained-text budget, returning whether the entry is
/// kept. The entry that straddles the budget boundary is kept (the budget saturates at
/// zero), so retention never cuts an event in half; everything older is dropped.
fn take_from_budget(budget: &mut usize, cost: usize) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget = budget.saturating_sub(cost);
    true
}

/// Maps an event to its contribution to recent human-visible text.
fn event_to_text(event: &TerminalEvent) -> &str {
    match event {
        TerminalEvent::HumanInput { data, visible } => {
            if *visible {
                data
            } else {
                ""
            }
        }
        TerminalEvent::PtyOutput { data } => data,
        TerminalEvent::VisibleOutput { text } => text,
        TerminalEvent::ScreenSnapshot { screen, .. } => screen.as_deref().unwrap_or(""),
        TerminalEvent::AiRequest { question, .. } => question,
        TerminalEvent::AiResponse { text, .. } => text,
        TerminalEvent::CommandStart { .. }
        | TerminalEvent::CommandEnd { .. }
        | TerminalEvent::ControlModeChange { .. } => "",
    }
}

/// Keeps only the last `max` characters of `text`.
pub(crate) fn trim_start_to_max_characters(text: &str, max: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max {
        text.to_string()
    } else {
        text.chars().skip(char_count - max).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    use super::*;

    fn snapshot(snapshot_id: &str, rows: u16, alt_screen: bool, screen: &str) -> TerminalEvent {
        TerminalEvent::ScreenSnapshot {
            snapshot_id: snapshot_id.to_string(),
            rows,
            columns: 80,
            alt_screen,
            screen: Some(screen.to_string()),
            previous_snapshot_id: None,
            diff: None,
        }
    }

    /// A store whose clock is a settable `AtomicI64` (epoch ms), so tests can advance
    /// wall time and exercise the age-tiered retention.
    fn clocked() -> (InMemoryTimelineStore, Arc<AtomicI64>) {
        let clock = Arc::new(AtomicI64::new(0));
        let handle = clock.clone();
        let store = InMemoryTimelineStore::with_clock(move || handle.load(Ordering::SeqCst));
        (store, clock)
    }

    fn snapshot_count(timeline: &InMemoryTimelineStore) -> usize {
        timeline.list_screen_snapshots().len()
    }

    #[test]
    fn downsamples_old_snapshots_by_age_tier() {
        let (mut timeline, clock) = clocked();
        // 40 minutes of one snapshot per second: without retention this is 2400 entries.
        let total_seconds = 40 * 60;
        for sec in 0..total_seconds {
            clock.store(sec as i64 * 1_000, Ordering::SeqCst);
            timeline.record(snapshot(&format!("s{sec}"), 24, false, "x"));
        }
        // "Now" is the last recorded timestamp; compact to apply age-based thinning.
        timeline.compact();

        let now = (total_seconds - 1) as i64 * 1_000;
        let snapshots = timeline.list_screen_snapshots();

        // The last minute stays dense (one per second), older bands thin out.
        let last_minute = snapshots
            .iter()
            .filter(|e| now.saturating_sub(e.ts) <= 60_000)
            .count();
        assert!(last_minute >= 55, "dense last minute, got {last_minute}");

        // 2400 raw snapshots collapse to a couple hundred across the tiers.
        assert!(
            snapshots.len() < 300,
            "downsampled well below the raw count, got {}",
            snapshots.len()
        );

        // Retained snapshots get sparser with age: no two beyond the 5-minute band sit
        // closer than that band's 30s spacing.
        let mut older_than_5min: Vec<i64> = snapshots
            .iter()
            .map(|e| e.ts)
            .filter(|ts| now.saturating_sub(*ts) > 300_000)
            .collect();
        older_than_5min.sort_unstable();
        for pair in older_than_5min.windows(2) {
            assert!(
                pair[1] - pair[0] >= 30_000,
                "aged snapshots respect >= 30s spacing, got gap {}",
                pair[1] - pair[0]
            );
        }
    }

    #[test]
    fn keeps_recent_raw_text_within_budget_and_drops_older() {
        let (mut timeline, clock) = clocked();
        // 100 bursts of 1000 chars each; only the most recent ~32000 chars survive.
        for i in 0..100 {
            clock.store(i as i64 * 1_000, Ordering::SeqCst);
            timeline.record(TerminalEvent::PtyOutput {
                data: "a".repeat(1_000),
            });
        }
        timeline.compact();

        let kept = timeline
            .list_events()
            .filter(|e| matches!(e, TerminalEvent::PtyOutput { .. }))
            .count();
        assert_eq!(
            kept,
            RAW_TEXT_RETENTION_CHARS / 1_000,
            "keeps exactly the budget's worth of recent bursts"
        );
        // The recent-window query is still fully served.
        assert_eq!(timeline.get_recent_pty_output(8_000).chars().count(), 8_000);
    }

    #[test]
    fn thins_a_fast_burst_in_the_freshest_band() {
        let (mut timeline, clock) = clocked();
        // 40 seconds of one snapshot per 20 ms — a fast-output burst, 2000 raw
        // snapshots, all inside the freshest band at compaction time.
        for i in 0..2_000_i64 {
            clock.store(i * 20, Ordering::SeqCst);
            timeline.record(snapshot(&format!("s{i}"), 24, false, "x"));
        }
        timeline.compact();

        // The 200 ms floor spacing bounds the burst to ~5 snapshots per second.
        let count = snapshot_count(&timeline);
        assert!(
            (150..=210).contains(&count),
            "a 2000-snapshot burst thins to ~200, got {count}"
        );
    }

    #[test]
    fn caps_retained_snapshot_bytes_even_when_the_tiers_keep_them() {
        let (mut timeline, clock) = clocked();
        // 50 seconds of 400 KB snapshots spaced 250 ms apart: the freshest band keeps
        // all 200 (spacing clears the floor), but their 80 MB exceeds the byte cap.
        let screen = "x".repeat(400_000);
        for i in 0..200_i64 {
            clock.store(i * 250, Ordering::SeqCst);
            timeline.record(snapshot(&format!("s{i}"), 24, false, &screen));
        }
        timeline.compact();

        let snapshots = timeline.list_screen_snapshots();
        let ids: Vec<&str> = snapshots
            .iter()
            .filter_map(|e| match &e.event {
                TerminalEvent::ScreenSnapshot { snapshot_id, .. } => Some(snapshot_id.as_str()),
                _ => None,
            })
            .collect();
        let retained_bytes: usize = snapshots
            .iter()
            .map(|e| match &e.event {
                TerminalEvent::ScreenSnapshot { screen, .. } => {
                    screen.as_ref().map_or(0, |s| s.len())
                }
                _ => 0,
            })
            .sum();

        // At most the budget plus the one straddling snapshot; newest kept, oldest gone.
        assert!(
            retained_bytes <= SNAPSHOT_RETENTION_BYTES + 400_000,
            "retained {retained_bytes} bytes"
        );
        assert!(snapshots.len() < 200, "dropped some, kept {}", ids.len());
        assert!(ids.contains(&"s199"), "newest snapshot survives");
        assert!(!ids.contains(&"s0"), "oldest snapshot is dropped");
    }

    #[test]
    fn shrinks_pathological_entry_capacity_after_a_purge() {
        let (mut timeline, clock) = clocked();
        clock.store(1_000, Ordering::SeqCst);
        // 3000 one-char output events all fit the raw-text char budget, so the entries
        // Vec legitimately grows past the shrink threshold.
        for _ in 0..3_000 {
            timeline.record(TerminalEvent::PtyOutput {
                data: "a".to_string(),
            });
        }
        assert!(timeline.entries.capacity() >= SHRINK_MIN_CAPACITY);

        // One budget-sized burst obsoletes the tail; the purge must return capacity.
        timeline.record(TerminalEvent::PtyOutput {
            data: "b".repeat(RAW_TEXT_RETENTION_CHARS),
        });
        timeline.compact();
        assert!(timeline.list_entries().len() < 100);
        assert!(
            timeline.entries.capacity() < SHRINK_MIN_CAPACITY,
            "capacity released after the purge, got {}",
            timeline.entries.capacity()
        );
    }

    #[test]
    fn small_store_is_left_untouched_by_compaction() {
        let (mut timeline, clock) = clocked();
        clock.store(1_000, Ordering::SeqCst);
        timeline.record(snapshot("s1", 24, false, "one"));
        timeline.record(TerminalEvent::PtyOutput {
            data: "hello".to_string(),
        });
        timeline.compact();
        assert_eq!(timeline.list_entries().len(), 2);
        assert_eq!(snapshot_count(&timeline), 1);
    }

    #[test]
    fn records_events_with_deterministic_ids_and_timestamps() {
        let mut timeline = InMemoryTimelineStore::with_clock(|| 1_000);
        let first = timeline.record(TerminalEvent::HumanInput {
            data: "ls\r".to_string(),
            visible: true,
        });
        assert_eq!(first.id, "event-1");
        assert_eq!(first.ts, 1_000);
        let second = timeline.record(TerminalEvent::PtyOutput {
            data: "file.txt\r\n".to_string(),
        });
        assert_eq!(second.id, "event-2");
        assert_eq!(timeline.list_entries().len(), 2);
    }

    #[test]
    fn returns_recent_text_and_pty_output_with_limits() {
        let mut timeline = InMemoryTimelineStore::with_clock(|| 1);
        timeline.record(TerminalEvent::HumanInput {
            data: "echo hello\r".to_string(),
            visible: true,
        });
        timeline.record(TerminalEvent::PtyOutput {
            data: "hello\r\n".to_string(),
        });
        timeline.record(TerminalEvent::VisibleOutput {
            text: "visible".to_string(),
        });
        timeline.record(TerminalEvent::HumanInput {
            data: "secret".to_string(),
            visible: false,
        });

        assert_eq!(
            timeline.get_recent_text(8_000),
            "echo hello\rhello\r\nvisible"
        );
        assert_eq!(timeline.get_recent_text(5), "sible");
        assert_eq!(timeline.get_recent_pty_output(8_000), "hello\r\n");
    }

    #[test]
    fn tracks_snapshots_latest_alternate_and_reset() {
        let mut timeline = InMemoryTimelineStore::with_clock(|| 1);
        timeline.record(snapshot("snapshot-1", 24, false, "first"));
        timeline.record(snapshot("snapshot-2", 30, true, "second"));

        let ids: Vec<&str> = timeline
            .list_screen_snapshots()
            .iter()
            .map(|e| match &e.event {
                TerminalEvent::ScreenSnapshot { snapshot_id, .. } => snapshot_id.as_str(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(ids, vec!["snapshot-1", "snapshot-2"]);

        assert!(timeline.get_screen_snapshot("snapshot-1").is_some());
        let latest = timeline.get_latest_screen_snapshot().unwrap();
        assert!(
            matches!(&latest.event, TerminalEvent::ScreenSnapshot { snapshot_id, .. } if snapshot_id == "snapshot-2")
        );
        let alt = timeline.get_latest_alternate_screen_snapshot().unwrap();
        assert!(
            matches!(&alt.event, TerminalEvent::ScreenSnapshot { snapshot_id, .. } if snapshot_id == "snapshot-2")
        );

        timeline.reset();
        assert!(timeline.list_entries().is_empty());
        assert!(timeline.get_latest_screen_snapshot().is_none());
    }

    #[test]
    fn diffs_screen_snapshots_on_demand() {
        let mut timeline = InMemoryTimelineStore::with_clock(|| 1);
        timeline.record(snapshot("snapshot-1", 24, false, "one\ntwo"));
        timeline.record(snapshot("snapshot-2", 24, false, "one\nTWO\nthree"));

        let diff = timeline
            .diff_screen_snapshots("snapshot-1", "snapshot-2")
            .unwrap();
        assert_eq!(diff.from_snapshot_id, "snapshot-1");
        assert_eq!(diff.to_snapshot_id, "snapshot-2");
        assert_eq!(diff.diff.added_lines, 2);
        assert_eq!(diff.diff.removed_lines, 1);
        assert_eq!(diff.diff.changed_lines, 3);

        let err = timeline
            .diff_screen_snapshots("missing", "snapshot-2")
            .unwrap_err();
        assert!(err.to_string().contains("\"missing\" was not found"));
    }
}

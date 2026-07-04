//! Local dogfooding event log: append-only JSONL under the XDG data directory
//! (design 0007). A dogfooding instrument, not product telemetry — nothing is
//! ever uploaded.
//!
//! Privacy invariant: the event structs have no field that could carry screen
//! content or PTY bytes. The only free-text fields in the schema are the
//! question the user typed after `#?`, the request id, and the shell program
//! path; everything else is an enum tag, a flag, a count, or a duration.
//!
//! Writes are fail-silent: if the file cannot be opened or a write fails, the
//! log degrades to inert (one warning in the debug log) and the shell is never
//! disturbed. Serialization happens on the emitting thread; a dedicated writer
//! thread owns the file, so `emit` never touches the render path.

use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Escape hatch: a non-empty value disables the event log entirely (same
/// convention as `KOSHELL_NO_AUTO`).
const DISABLE_ENV_KEY: &str = "KOSHELL_NO_EVENT_LOG";

/// One dogfooding event. Duration fields are milliseconds; `&'static str`
/// fields are closed vocabularies, never runtime text.
#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    SessionStart {
        shell: String,
        integrated: bool,
        cols: u16,
        rows: u16,
        version: &'static str,
    },
    SessionEnd {
        exit_code: i32,
        duration_ms: u64,
    },
    QuestionSubmitted {
        question: String,
        origin: &'static str,
        form: &'static str,
    },
    QuestionCancelled {
        question: String,
        pending_for_ms: u64,
    },
    Dispatched {
        request_id: String,
        question: String,
        fire_reason: &'static str,
        still_running: bool,
        submit_to_dispatch_ms: u64,
    },
    DispatchFailed {
        request_id: String,
        question: String,
        fire_reason: &'static str,
        reason: &'static str,
    },
    FirstDelta {
        request_id: String,
        dispatch_to_first_delta_ms: u64,
        mode: &'static str,
        anchored: bool,
    },
    DegradeToBlock {
        request_id: String,
        reason: &'static str,
        ms_since_dispatch: u64,
        deltas_so_far: u32,
    },
    ResponseEnd {
        request_id: String,
        status: &'static str,
        total_ms: u64,
        first_delta_ms: Option<u64>,
        delta_count: u32,
        began_anchored: bool,
        degraded_to_block: bool,
        mid_stream_input_chunks: u32,
    },
}

/// The envelope every line carries: wall-clock epoch milliseconds stamped at
/// emit time, and the `koshell-<pid>` session id shared with the IPC hello.
#[derive(Serialize)]
struct Envelope<'a> {
    ts: u64,
    session: &'a str,
    #[serde(flatten)]
    event: &'a Event,
}

/// A cheap-to-clone emitter handle. A default-constructed handle is inert:
/// `emit` is a no-op, so state owners can hold one unconditionally.
#[derive(Clone, Default)]
pub struct EventLog {
    inner: Option<Inner>,
}

#[derive(Clone)]
struct Inner {
    tx: mpsc::Sender<String>,
    session: Arc<str>,
}

/// Owns the writer thread. Joining after all `EventLog` clones are dropped
/// drains the channel, so the final `session_end` is on disk before the
/// process exits.
pub struct EventLogWriter {
    handle: JoinHandle<()>,
}

impl EventLogWriter {
    pub fn join(self) {
        let _ = self.handle.join();
    }
}

/// The event log path under the XDG data directory.
pub fn event_log_path() -> PathBuf {
    let base = match std::env::var("XDG_DATA_HOME") {
        Ok(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
        _ => {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home).join(".local").join("share")
        }
    };
    base.join("koshell").join("events.jsonl")
}

/// Opens the session's event log. Any failure (or the `KOSHELL_NO_EVENT_LOG`
/// escape hatch) yields an inert handle instead of an error: the log must
/// never cost a shell session.
pub fn open() -> (EventLog, Option<EventLogWriter>) {
    if std::env::var(DISABLE_ENV_KEY).is_ok_and(|value| !value.trim().is_empty()) {
        return (EventLog::default(), None);
    }
    let path = event_log_path();
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        log::warn!("event log disabled: cannot create {}", parent.display());
        return (EventLog::default(), None);
    }
    let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) else {
        log::warn!("event log disabled: cannot open {}", path.display());
        return (EventLog::default(), None);
    };
    let (tx, rx) = mpsc::channel::<String>();
    let handle = thread::spawn(move || {
        let mut writer = BufWriter::new(file);
        for line in rx {
            if writer.write_all(line.as_bytes()).is_err()
                || writer.write_all(b"\n").is_err()
                || writer.flush().is_err()
            {
                // Fail silent: stop writing, keep draining so senders never
                // notice (their sends already cannot block or fail loudly).
                break;
            }
        }
    });
    let session: Arc<str> = format!("koshell-{}", std::process::id()).into();
    (
        EventLog {
            inner: Some(Inner { tx, session }),
        },
        Some(EventLogWriter { handle }),
    )
}

impl EventLog {
    /// Serializes and queues one event. A no-op on an inert handle; a send
    /// failure (writer gone) is silently dropped.
    pub fn emit(&self, event: Event) {
        let Some(inner) = &self.inner else {
            return;
        };
        let envelope = Envelope {
            ts: epoch_ms(),
            session: &inner.session,
            event: &event,
        };
        if let Ok(line) = serde_json::to_string(&envelope) {
            let _ = inner.tx.send(line);
        }
    }

    /// A handle whose emitted lines land on a channel instead of a file, for
    /// unit tests of the emit points.
    #[cfg(test)]
    pub(crate) fn capture() -> (EventLog, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel();
        (
            EventLog {
                inner: Some(Inner {
                    tx,
                    session: "koshell-test".into(),
                }),
            },
            rx,
        )
    }
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_log_path_is_under_a_koshell_data_directory() {
        let path = event_log_path();
        assert!(path.ends_with("koshell/events.jsonl"));
    }

    #[test]
    fn inert_handle_emits_nothing_and_does_not_panic() {
        let log = EventLog::default();
        log.emit(Event::SessionEnd {
            exit_code: 0,
            duration_ms: 1,
        });
    }

    #[test]
    fn emitted_lines_carry_the_envelope_and_snake_case_tag() {
        let (log, rx) = EventLog::capture();
        log.emit(Event::QuestionSubmitted {
            question: "why did this fail".to_string(),
            origin: "in_program",
            form: "standalone",
        });
        let line = rx.recv().expect("one line emitted");
        let value: serde_json::Value = serde_json::from_str(&line).expect("valid JSON");
        assert_eq!(value["event"], "question_submitted");
        assert_eq!(value["session"], "koshell-test");
        assert_eq!(value["question"], "why did this fail");
        assert_eq!(value["origin"], "in_program");
        assert_eq!(value["form"], "standalone");
        assert!(value["ts"].as_u64().is_some_and(|ts| ts > 0));
    }

    #[test]
    fn response_end_serializes_every_metric_field() {
        let (log, rx) = EventLog::capture();
        log.emit(Event::ResponseEnd {
            request_id: "koshell-req-1".to_string(),
            status: "ok",
            total_ms: 1200,
            first_delta_ms: Some(300),
            delta_count: 7,
            began_anchored: true,
            degraded_to_block: false,
            mid_stream_input_chunks: 2,
        });
        let value: serde_json::Value =
            serde_json::from_str(&rx.recv().expect("one line")).expect("valid JSON");
        assert_eq!(value["event"], "response_end");
        assert_eq!(value["request_id"], "koshell-req-1");
        assert_eq!(value["status"], "ok");
        assert_eq!(value["total_ms"], 1200);
        assert_eq!(value["first_delta_ms"], 300);
        assert_eq!(value["delta_count"], 7);
        assert_eq!(value["began_anchored"], true);
        assert_eq!(value["degraded_to_block"], false);
        assert_eq!(value["mid_stream_input_chunks"], 2);
    }
}

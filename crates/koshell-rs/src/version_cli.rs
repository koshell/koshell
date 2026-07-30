//! `koshell version` — the three koshell versions that can differ on one machine
//! (design 0024).
//!
//! `--version` answers "what is this binary?", which is the one question a user upgrading
//! koshell does *not* have. Their shell was `exec`-ed into a koshell process minutes or
//! days ago and keeps running the build it started with; the daemon is a separate
//! long-lived process that outlives terminals and is replaced independently. So three
//! answers are reported side by side:
//!
//! - **koshell** — this binary, the build a *new* terminal would get.
//! - **this tty** — the koshell actually wrapping this terminal, read from the version file
//!   beside its liveness marker (`shell::tty_version_path`). That file is the only channel
//!   that works with no daemon and needs no cooperation from the wrapper: a child process
//!   cannot ask the process above it anything.
//! - **koshell-ai-daemon** — the *running* daemon, over the same additive
//!   `status_request`/`status` pair `koshell daemon status` uses.
//!
//! Deliberately read-only. A stopped daemon is reported as stopped rather than started to
//! be interrogated: `koshell version` must be safe to run anywhere, and starting a
//! background process to print a string would be a surprising thing for it to do (worse,
//! a daemon predating this feature would not recognize `--version` and would simply become
//! the daemon). The version of an installed-but-stopped daemon is therefore not reported;
//! the command that would start it is named instead.
//!
//! Every outcome is a successful report, so the exit code is always 0 — unlike
//! `koshell status` and `koshell daemon status`, which are probes whose non-zero exit
//! *is* the answer. "The daemon is not running" is a fact this command reports, not a
//! failure to report it.

use std::collections::HashMap;
use std::path::Path;

use koshell_proto::{PROTOCOL_VERSION, ServerMessage};

use crate::daemon_cli::{self, Probe};
use crate::{VERSION, daemon_spawn, ipc, shell};

/// Column the version values start at, so the three answers line up under each other.
const LABEL_WIDTH: usize = 20;

/// Which koshell, if any, wraps this terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TtyKoshell {
    /// A live koshell owns this terminal and recorded its version.
    Known {
        version: String,
        tty: String,
        pid: libc::pid_t,
    },
    /// A live koshell owns this terminal but recorded no version — it predates design 0024.
    Unversioned { tty: String, pid: libc::pid_t },
    /// `KOSHELL` says we are inside koshell, but no live marker names this terminal: the
    /// coarse fallback (koshell could not brand the child pts), or a stale brand inherited
    /// onto a recycled pts whose koshell has died.
    Unmarked,
    /// No koshell wraps this terminal.
    Outside,
}

/// What the AI daemon reports about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DaemonKoshell {
    Running {
        version: String,
        pid: u32,
        protocol_version: u32,
    },
    /// Reachable but silent: a daemon old enough not to know `status_request`.
    Silent,
    /// Not running, with the command that would start it (design 0008's resolution chain).
    Stopped { would_start: Option<String> },
}

/// Runs `koshell version`, returning the process exit code (always 0; see the module docs).
pub fn run() -> i32 {
    let env: HashMap<String, String> = std::env::vars().collect();
    let tty = inspect_tty(&env, shell::controlling_tty().as_deref());
    let daemon = inspect_daemon(&ipc::default_socket_path());
    for line in format_lines(VERSION, &tty, &daemon) {
        println!("{line}");
    }
    0
}

/// Resolves which koshell wraps this terminal.
///
/// The liveness marker for *this* process's controlling tty is the authority, not the
/// inherited `KOSHELL` value: the brand can outlive the koshell that wrote it, and it is
/// inherited across tty boundaries. `KOSHELL` is consulted only to tell the two negative
/// answers apart — a brand naming a different terminal (a new tmux pane inherits one) means
/// this terminal is simply not wrapped, while a brand naming this one with no live marker
/// means we are inside koshell but cannot identify it.
pub(crate) fn inspect_tty(env: &HashMap<String, String>, current_tty: Option<&str>) -> TtyKoshell {
    let Some(value) = env
        .get(shell::KOSHELL_ENV_KEY)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
    else {
        return TtyKoshell::Outside;
    };
    // A brand for another terminal is that terminal's koshell, not ours; and without a tty
    // of our own we cannot claim any brand as ours either.
    if let Some(branded) = shell::koshell_tty(value)
        && current_tty != Some(branded)
    {
        return TtyKoshell::Outside;
    }
    let Some(tty) = current_tty else {
        return TtyKoshell::Unmarked;
    };
    let Some(pid) = shell::tty_owner_pid(tty) else {
        return TtyKoshell::Unmarked;
    };
    match shell::tty_owner_version(tty) {
        Some(version) => TtyKoshell::Known {
            version,
            tty: tty.to_string(),
            pid,
        },
        None => TtyKoshell::Unversioned {
            tty: tty.to_string(),
            pid,
        },
    }
}

/// Asks the running daemon for its identity, or reports how one would be started.
fn inspect_daemon(socket_path: &Path) -> DaemonKoshell {
    match daemon_cli::probe(socket_path) {
        Probe::Alive => match daemon_cli::query_status(socket_path) {
            Some(ServerMessage::Status {
                pid,
                version,
                protocol_version,
                ..
            }) => DaemonKoshell::Running {
                version,
                pid,
                protocol_version,
            },
            _ => DaemonKoshell::Silent,
        },
        Probe::Stale | Probe::Absent => DaemonKoshell::Stopped {
            would_start: daemon_spawn::resolve_plan_from_env()
                .map(|plan| format!("{} ({})", plan.command_line, plan.source)),
        },
    }
}

/// One `label: value` row, with the value column aligned across all three answers.
fn row(label: &str, value: &str) -> String {
    format!("{label:<LABEL_WIDTH$}{value}")
}

/// A note under a row. Indented two spaces rather than aligned under the value column:
/// these sentences carry the actionable half of the report, and hanging them off column 20
/// would push several of them past an 80-column terminal.
fn note(text: &str) -> String {
    format!("  ({text})")
}

/// A sub-row under a row (`  would start:  ...`), in `koshell daemon status`'s shape.
fn sub_row(label: &str, value: &str) -> String {
    format!("  {label:<width$}{value}", width = LABEL_WIDTH - 2)
}

/// Renders the whole report. Pure, so every combination is a unit test rather than a
/// session to reproduce by hand.
pub(crate) fn format_lines(binary: &str, tty: &TtyKoshell, daemon: &DaemonKoshell) -> Vec<String> {
    let mut lines = vec![row(
        "koshell:",
        &format!("{binary}  (this binary, protocol v{PROTOCOL_VERSION})"),
    )];

    match tty {
        TtyKoshell::Known { version, tty, pid } => {
            lines.push(row("this tty:", &format!("{version}  ({tty}, pid {pid})")));
            // The whole point of the row: an upgrade does not reach a terminal that is
            // already running, and nothing else on the system says so.
            if version != binary {
                lines.push(note(&format!(
                    "a different build than this binary — a new terminal runs {binary}"
                )));
            }
        }
        TtyKoshell::Unversioned { tty, pid } => {
            lines.push(row("this tty:", &format!("unknown  ({tty}, pid {pid})")));
            lines.push(note(
                "that koshell predates `koshell version`, so it recorded none",
            ));
        }
        TtyKoshell::Unmarked => {
            lines.push(row("this tty:", "unknown"));
            lines.push(note(
                "inside koshell, but no live marker names this terminal",
            ));
        }
        TtyKoshell::Outside => {
            lines.push(row("this tty:", "not wrapped by koshell"));
            lines.push(note(
                "start koshell, or install the auto-wrap: `koshell shell-init zsh`",
            ));
        }
    }

    match daemon {
        DaemonKoshell::Running {
            version,
            pid,
            protocol_version,
        } => {
            lines.push(row(
                "koshell-ai-daemon:",
                &format!("{version}  (pid {pid}, protocol v{protocol_version})"),
            ));
            // A protocol mismatch is the one version difference that stops `#?` working,
            // so it is named here with its fix rather than left to be inferred.
            if *protocol_version != PROTOCOL_VERSION {
                lines.push(note(&format!(
                    "this binary speaks protocol v{PROTOCOL_VERSION} — `koshell daemon restart`"
                )));
            }
        }
        DaemonKoshell::Silent => {
            lines.push(row("koshell-ai-daemon:", "unknown"));
            lines.push(note(
                "running, but it did not answer — an older daemon; `koshell daemon restart`",
            ));
        }
        DaemonKoshell::Stopped { would_start } => {
            lines.push(row("koshell-ai-daemon:", "not running"));
            lines.push(sub_row(
                "would start:",
                would_start
                    .as_deref()
                    .unwrap_or("no launch command resolved"),
            ));
            lines.push(note(
                "a stopped daemon has no version; `koshell daemon start` or a `#?` starts one",
            ));
        }
    }

    lines
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;

    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn stopped() -> DaemonKoshell {
        DaemonKoshell::Stopped {
            would_start: Some("'/usr/local/bin/koshell-ai-daemon' (adjacent)".to_string()),
        }
    }

    fn line_starting_with<'a>(lines: &'a [String], prefix: &str) -> &'a str {
        lines
            .iter()
            .find(|line| line.starts_with(prefix))
            .unwrap_or_else(|| panic!("no line starting with {prefix:?} in {lines:#?}"))
    }

    #[test]
    fn outside_koshell_there_is_no_terminal_version_to_report() {
        assert_eq!(
            inspect_tty(&env_of(&[]), Some("/dev/pts/3")),
            TtyKoshell::Outside
        );
        // A brand naming another terminal (the tmux-pane shape) belongs to that pane's
        // koshell; this terminal is simply not wrapped.
        assert_eq!(
            inspect_tty(
                &env_of(&[("KOSHELL", "koshell-1,/dev/pts/999")]),
                Some("/dev/pts/3")
            ),
            TtyKoshell::Outside
        );
        // Without a tty of our own, no brand can be claimed as ours.
        assert_eq!(
            inspect_tty(&env_of(&[("KOSHELL", "koshell-1,/dev/pts/3")]), None),
            TtyKoshell::Outside
        );
    }

    #[test]
    fn a_brand_with_no_live_marker_is_unknown_rather_than_absent() {
        // The coarse fallback: inside koshell, but koshell could not brand the child pts,
        // so there is no marker to read a version from.
        assert_eq!(
            inspect_tty(&env_of(&[("KOSHELL", "koshell-1")]), None),
            TtyKoshell::Unmarked
        );
        // Branded to this tty, but no marker file exists for it (a stale brand on a
        // recycled pts, or a koshell that could not write one).
        assert_eq!(
            inspect_tty(
                &env_of(&[("KOSHELL", "koshell-1,/dev/pts/koshell-version-test")]),
                Some("/dev/pts/koshell-version-test")
            ),
            TtyKoshell::Unmarked
        );
    }

    // The daemon is read, never started: the three shapes it can be found in map to the
    // three things reported, and an absent socket is one of them rather than an error.
    #[test]
    fn the_daemon_is_read_from_its_socket_in_each_of_its_three_states() {
        let dir = tempfile::tempdir().expect("temp dir");

        let missing = dir.path().join("absent.sock");
        assert!(matches!(
            inspect_daemon(&missing),
            DaemonKoshell::Stopped { .. }
        ));

        let answering = dir.path().join("answering.sock");
        let handle = serve(
            UnixListener::bind(&answering).expect("bind"),
            Some(
                serde_json::to_string(&ServerMessage::Status {
                    pid: 5120,
                    version: "20260730.074101".to_string(),
                    protocol_version: PROTOCOL_VERSION,
                    uptime_ms: 9000,
                    connections: 1,
                })
                .expect("serialize"),
            ),
        );
        let reported = inspect_daemon(&answering);
        handle.join().expect("join");
        assert_eq!(
            reported,
            DaemonKoshell::Running {
                version: "20260730.074101".to_string(),
                pid: 5120,
                protocol_version: PROTOCOL_VERSION,
            }
        );

        // Reachable but hanging up without a reply: an older daemon, which is a different
        // answer from "not running" — one needs restarting, the other starting.
        let silent = dir.path().join("silent.sock");
        let handle = serve(UnixListener::bind(&silent).expect("bind"), None);
        assert_eq!(inspect_daemon(&silent), DaemonKoshell::Silent);
        handle.join().expect("join");
    }

    /// A stub daemon socket that answers the first request line with `reply` (or hangs up,
    /// for the older-daemon case) and then stops. It serves *connections* in a loop rather
    /// than accepting once because `inspect_daemon` connects twice: the reachability probe
    /// hangs up without writing, and the status request follows on a second connection.
    fn serve(listener: UnixListener, reply: Option<String>) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { break };
                let mut reader = BufReader::new(conn.try_clone().expect("clone"));
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    continue; // the probe, which never writes
                }
                if let Some(reply) = &reply {
                    conn.write_all(reply.as_bytes()).expect("write reply");
                    conn.write_all(b"\n").expect("newline");
                    conn.flush().expect("flush");
                }
                break;
            }
        })
    }

    #[test]
    fn the_binary_row_names_the_build_and_its_protocol() {
        let lines = format_lines("20260730.074058", &TtyKoshell::Outside, &stopped());
        let first = &lines[0];
        assert!(first.starts_with("koshell:"));
        assert!(first.contains("20260730.074058"));
        assert!(first.contains(&format!("protocol v{PROTOCOL_VERSION}")));
    }

    // The reason the command exists: an upgraded binary does not reach a terminal that is
    // already running, and only this comparison says so.
    #[test]
    fn a_terminal_running_another_build_is_called_out() {
        let tty = TtyKoshell::Known {
            version: "20260729.183304".to_string(),
            tty: "/dev/ttys003".to_string(),
            pid: 4821,
        };
        let lines = format_lines("20260730.074058", &tty, &stopped());
        let row = line_starting_with(&lines, "this tty:");
        assert!(row.contains("20260729.183304"));
        assert!(row.contains("/dev/ttys003"));
        assert!(row.contains("pid 4821"));
        let joined = lines.join("\n");
        assert!(
            joined.contains("a different build than this binary"),
            "the mismatch is stated, not left to be spotted: {joined}"
        );
        assert!(joined.contains("a new terminal runs 20260730.074058"));
    }

    #[test]
    fn a_terminal_running_this_build_gets_no_note() {
        let tty = TtyKoshell::Known {
            version: "20260730.074058".to_string(),
            tty: "/dev/ttys003".to_string(),
            pid: 4821,
        };
        let lines = format_lines("20260730.074058", &tty, &stopped());
        assert!(!lines.join("\n").contains("a different build"));
    }

    #[test]
    fn an_older_wrapper_is_unknown_rather_than_missing() {
        let tty = TtyKoshell::Unversioned {
            tty: "/dev/ttys003".to_string(),
            pid: 4821,
        };
        let lines = format_lines("20260730.074058", &tty, &stopped());
        let row = line_starting_with(&lines, "this tty:");
        assert!(row.contains("unknown"));
        assert!(row.contains("pid 4821"), "the koshell is still identified");
        assert!(lines.join("\n").contains("predates `koshell version`"));
    }

    #[test]
    fn a_running_daemon_reports_its_version_pid_and_protocol() {
        let daemon = DaemonKoshell::Running {
            version: "20260730.074101".to_string(),
            pid: 5120,
            protocol_version: PROTOCOL_VERSION,
        };
        let lines = format_lines("20260730.074058", &TtyKoshell::Outside, &daemon);
        let row = line_starting_with(&lines, "koshell-ai-daemon:");
        assert!(row.contains("20260730.074101"));
        assert!(row.contains("pid 5120"));
        assert!(row.contains(&format!("protocol v{PROTOCOL_VERSION}")));
        assert!(!lines.join("\n").contains("daemon restart"));
    }

    // A protocol mismatch is the version difference that actually breaks `#?`, so it comes
    // with its fix instead of leaving the user to compare two `v` numbers.
    #[test]
    fn a_protocol_mismatch_names_the_fix() {
        let daemon = DaemonKoshell::Running {
            version: "20250101.000000".to_string(),
            pid: 5120,
            protocol_version: PROTOCOL_VERSION + 1,
        };
        let lines = format_lines("20260730.074058", &TtyKoshell::Outside, &daemon);
        assert!(lines.join("\n").contains("koshell daemon restart"));
    }

    #[test]
    fn a_stopped_daemon_names_what_would_start_it_instead_of_a_version() {
        let lines = format_lines("20260730.074058", &TtyKoshell::Outside, &stopped());
        let joined = lines.join("\n");
        assert!(line_starting_with(&lines, "koshell-ai-daemon:").contains("not running"));
        assert!(joined.contains("would start:"));
        assert!(joined.contains("koshell-ai-daemon' (adjacent)"));
        assert!(joined.contains("koshell daemon start"));
    }

    #[test]
    fn a_stopped_daemon_with_no_resolvable_command_says_so() {
        let lines = format_lines(
            "20260730.074058",
            &TtyKoshell::Outside,
            &DaemonKoshell::Stopped { would_start: None },
        );
        assert!(lines.join("\n").contains("no launch command resolved"));
    }

    #[test]
    fn a_silent_daemon_is_unknown_and_not_mistaken_for_stopped() {
        let lines = format_lines(
            "20260730.074058",
            &TtyKoshell::Outside,
            &DaemonKoshell::Silent,
        );
        let row = line_starting_with(&lines, "koshell-ai-daemon:");
        assert!(row.contains("unknown"));
        assert!(!row.contains("not running"));
        assert!(lines.join("\n").contains("older daemon"));
    }

    // The three answers are read as a column; misaligned values would make comparing them
    // the user's job.
    #[test]
    fn the_three_values_share_one_column() {
        let tty = TtyKoshell::Known {
            version: "20260730.074058".to_string(),
            tty: "/dev/ttys003".to_string(),
            pid: 4821,
        };
        let daemon = DaemonKoshell::Running {
            version: "20260730.074101".to_string(),
            pid: 5120,
            protocol_version: PROTOCOL_VERSION,
        };
        let lines = format_lines("20260730.074058", &tty, &daemon);
        for prefix in ["koshell:", "this tty:", "koshell-ai-daemon:"] {
            let row = line_starting_with(&lines, prefix);
            assert_eq!(
                row.find("2026"),
                Some(LABEL_WIDTH),
                "{prefix} value starts off the column: {row}"
            );
        }
    }
}

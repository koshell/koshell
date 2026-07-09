//! `koshell daemon <status|start|stop|restart>` — manual daemon lifecycle control
//! (design 0008). No PTY and no session: it probes the socket, and talks to the
//! daemon over IPC (the additive `status_request`/`status` pair) where needed.
//! It shares command resolution and the detached spawn with `daemon_spawn`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use koshell_proto::{ClientMessage, ServerMessage};

use crate::cli::DaemonAction;
use crate::daemon_spawn;
use crate::ipc;

/// How long to wait for a `status` reply before giving up on a running daemon.
const STATUS_TIMEOUT: Duration = Duration::from_secs(1);
/// How long `start`/`stop` wait for the socket to appear or disappear.
const TRANSITION_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll interval while waiting for a transition.
const POLL_STEP: Duration = Duration::from_millis(50);

/// The daemon's reachability, mirroring the daemon-side `probeSocket`.
/// Shared with `auth_cli`, which needs the same connect-or-spawn dance.
pub(crate) enum Probe {
    Alive,
    Stale,
    Absent,
}

pub(crate) fn probe(path: &Path) -> Probe {
    if !path.exists() {
        return Probe::Absent;
    }
    match UnixStream::connect(path) {
        Ok(_) => Probe::Alive,
        Err(_) => Probe::Stale,
    }
}

/// Asks the running daemon for its status, returning the `status` message or
/// `None` if it does not reply in time (an older daemon that ignores the request).
fn query_status(path: &Path) -> Option<ServerMessage> {
    let mut stream = UnixStream::connect(path).ok()?;
    stream.set_read_timeout(Some(STATUS_TIMEOUT)).ok()?;
    let request = serde_json::to_string(&ClientMessage::StatusRequest {}).ok()?;
    stream.write_all(request.as_bytes()).ok()?;
    stream.write_all(b"\n").ok()?;
    stream.flush().ok()?;
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if let Ok(message @ ServerMessage::Status { .. }) =
            serde_json::from_str::<ServerMessage>(line.trim_end())
        {
            return Some(message);
        }
    }
}

/// Renders an uptime in milliseconds as `1h 2m 3s` / `2m 3s` / `3s`.
pub(crate) fn format_uptime(ms: u64) -> String {
    let total = ms / 1000;
    let (hours, minutes, seconds) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// Prints the running daemon's details (queried over IPC), plus the socket and log
/// paths. Used by both `status` and the "already running" / "started" reports.
fn print_running_details(socket_path: &Path) {
    match query_status(socket_path) {
        Some(ServerMessage::Status {
            pid,
            version,
            protocol_version,
            uptime_ms,
            connections,
        }) => {
            println!("  pid:          {pid}");
            println!("  version:      {version}");
            println!("  protocol:     v{protocol_version}");
            println!("  uptime:       {}", format_uptime(uptime_ms));
            println!("  connections:  {connections}");
        }
        _ => println!("  (no status reply — an older daemon?)"),
    }
    println!("  socket:       {}", socket_path.display());
    println!(
        "  log:          {}",
        daemon_spawn::daemon_log_path().display()
    );
}

/// Polls until the daemon is reachable, or the transition timeout elapses.
pub(crate) fn wait_until_alive(path: &Path) -> bool {
    let deadline = Instant::now() + TRANSITION_TIMEOUT;
    loop {
        if matches!(probe(path), Probe::Alive) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL_STEP);
    }
}

/// Polls until the daemon is no longer reachable, or the transition timeout elapses.
fn wait_until_gone(path: &Path) -> bool {
    let deadline = Instant::now() + TRANSITION_TIMEOUT;
    loop {
        if !matches!(probe(path), Probe::Alive) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL_STEP);
    }
}

fn status(socket_path: &Path) -> i32 {
    match probe(socket_path) {
        Probe::Alive => {
            println!("AI daemon: running");
            print_running_details(socket_path);
            0
        }
        Probe::Stale | Probe::Absent => {
            println!("AI daemon: not running");
            println!("  socket:       {}", socket_path.display());
            match daemon_spawn::resolve_plan_from_env() {
                Some(plan) => {
                    println!("  would start:  {} ({})", plan.command_line, plan.source);
                }
                None => println!("  would start:  no launch command resolved"),
            }
            1
        }
    }
}

fn start(socket_path: &Path) -> i32 {
    if matches!(probe(socket_path), Probe::Alive) {
        println!("AI daemon: already running");
        print_running_details(socket_path);
        return 0;
    }
    let Some(plan) = daemon_spawn::resolve_plan_from_env() else {
        eprintln!("cannot start the AI daemon: no launch command resolved.");
        eprintln!(
            "  set KOSHELL_DAEMON_CMD, or install the koshell-ai-daemon binary next to koshell."
        );
        return 1;
    };
    if let Err(error) = daemon_spawn::spawn(&plan) {
        eprintln!("failed to start the AI daemon: {error}");
        return 1;
    }
    if wait_until_alive(socket_path) {
        println!("AI daemon: started ({})", plan.source);
        print_running_details(socket_path);
        0
    } else {
        eprintln!("started the AI daemon, but it did not become reachable in time.");
        eprintln!(
            "  check the log: {}",
            daemon_spawn::daemon_log_path().display()
        );
        1
    }
}

fn stop(socket_path: &Path) -> i32 {
    match probe(socket_path) {
        Probe::Absent => {
            println!("AI daemon: not running");
            0
        }
        Probe::Stale => {
            let _ = std::fs::remove_file(socket_path);
            println!("AI daemon: not running (removed a stale socket)");
            0
        }
        Probe::Alive => {
            let Some(ServerMessage::Status { pid, .. }) = query_status(socket_path) else {
                eprintln!("the AI daemon is running but did not report its pid; stop it manually.");
                return 1;
            };
            // SIGTERM: the daemon's handler closes the server and removes the socket.
            let signalled = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } == 0;
            if !signalled {
                eprintln!("failed to signal the AI daemon (pid {pid}).");
                return 1;
            }
            if wait_until_gone(socket_path) {
                println!("AI daemon: stopped (pid {pid})");
                0
            } else {
                eprintln!("signalled the AI daemon (pid {pid}), but its socket is still present.");
                1
            }
        }
    }
}

fn restart(socket_path: &Path) -> i32 {
    if matches!(probe(socket_path), Probe::Alive) {
        let code = stop(socket_path);
        if code != 0 {
            return code;
        }
    }
    start(socket_path)
}

/// Runs a `koshell daemon` action, returning the process exit code.
pub fn run(action: DaemonAction) -> i32 {
    let socket_path = ipc::default_socket_path();
    match action {
        DaemonAction::Status => status(&socket_path),
        DaemonAction::Start => start(&socket_path),
        DaemonAction::Stop => stop(&socket_path),
        DaemonAction::Restart => restart(&socket_path),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;
    use std::thread;

    use super::*;

    #[test]
    fn probe_reports_absent_stale_and_alive() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("daemon.sock");
        assert!(matches!(probe(&path), Probe::Absent));

        std::fs::write(&path, "not a socket").expect("write file");
        assert!(matches!(probe(&path), Probe::Stale));

        std::fs::remove_file(&path).expect("remove file");
        let listener = UnixListener::bind(&path).expect("bind");
        assert!(matches!(probe(&path), Probe::Alive));
        drop(listener);
    }

    #[test]
    fn query_status_reads_the_reply() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        let handle = thread::spawn(move || {
            let (mut conn, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(conn.try_clone().expect("clone"));
            let mut line = String::new();
            reader.read_line(&mut line).expect("read request");
            let reply = serde_json::to_string(&ServerMessage::Status {
                pid: 4321,
                version: "9.9.9".to_string(),
                protocol_version: 1,
                uptime_ms: 5,
                connections: 1,
            })
            .expect("serialize");
            conn.write_all(reply.as_bytes()).expect("write");
            conn.write_all(b"\n").expect("write newline");
            conn.flush().expect("flush");
        });

        let status = query_status(&path);
        handle.join().expect("join");
        match status {
            Some(ServerMessage::Status { pid, version, .. }) => {
                assert_eq!(pid, 4321);
                assert_eq!(version, "9.9.9");
            }
            other => panic!("unexpected status: {other:?}"),
        }
    }

    #[test]
    fn format_uptime_scales_units() {
        assert_eq!(format_uptime(3_000), "3s");
        assert_eq!(format_uptime(125_000), "2m 5s");
        assert_eq!(format_uptime(3_723_000), "1h 2m 3s");
    }
}

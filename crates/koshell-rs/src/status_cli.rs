//! `koshell status` — reports the current koshell instance's state (design
//! 0015): its daemon connection, conversation, and active model, plus a daemon
//! summary. Routed by the session id in `KOSHELL` (field 0) over the additive
//! `instance_status_request`/`instance_status` pair (no hello, no ack), so it
//! must be run from inside a koshell shell.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use koshell_proto::{ClientMessage, ServerMessage};

use crate::daemon_cli::{self, Probe, format_uptime};
use crate::{ipc, shell};

/// How long to wait for the daemon's `instance_status` reply.
const STATUS_TIMEOUT: Duration = Duration::from_secs(1);

/// Runs `koshell status`, returning the process exit code.
pub fn run() -> i32 {
    let Some(session_id) = ipc::current_session_id() else {
        eprintln!(
            "not inside a koshell session; run `koshell daemon status` for the daemon itself."
        );
        return 1;
    };

    let socket_path = ipc::default_socket_path();
    println!("koshell instance: {session_id}");
    // The wrapped tty is field 1 of KOSHELL; purely a local display detail.
    if let Ok(value) = std::env::var(shell::KOSHELL_ENV_KEY)
        && let Some(tty) = shell::koshell_tty(&value)
    {
        println!("  tty:          {tty}");
    }

    match daemon_cli::probe(&socket_path) {
        Probe::Alive => match request_status(&socket_path, &session_id) {
            Some(ServerMessage::InstanceStatus {
                known,
                cwd,
                shell,
                model,
                conversation,
                daemon_pid,
                uptime_ms,
                version,
                protocol_version,
                connections,
                ..
            }) => {
                if let Some(cwd) = cwd {
                    println!("  cwd:          {cwd}");
                }
                if let Some(shell) = shell {
                    println!("  shell:        {shell}");
                }
                println!("  model:        {}", model.as_deref().unwrap_or("—"));
                println!(
                    "  conversation: {}",
                    if conversation { "active" } else { "none" }
                );
                if !known {
                    println!("  (no conversation yet on this instance — run a `#?` to start one)");
                }
                println!("  daemon:");
                println!("    pid:          {daemon_pid}");
                println!("    version:      {version}");
                println!("    protocol:     v{protocol_version}");
                println!("    uptime:       {}", format_uptime(uptime_ms));
                println!("    connections:  {connections}");
                0
            }
            _ => {
                eprintln!(
                    "  the AI daemon did not answer the status request — it likely predates \
                     `koshell status`. Restart it with `koshell daemon restart`."
                );
                1
            }
        },
        Probe::Stale | Probe::Absent => {
            println!("  daemon:       not running");
            1
        }
    }
}

/// Sends one `instance_status_request` and returns the `instance_status` reply,
/// or `None` if the daemon hung up without answering (an older daemon).
fn request_status(path: &Path, session_id: &str) -> Option<ServerMessage> {
    let mut stream = UnixStream::connect(path).ok()?;
    let _ = stream.set_read_timeout(Some(STATUS_TIMEOUT));
    let request = serde_json::to_string(&ClientMessage::InstanceStatusRequest {
        session_id: session_id.to_string(),
    })
    .ok()?;
    stream.write_all(request.as_bytes()).ok()?;
    stream.write_all(b"\n").ok()?;
    stream.flush().ok()?;
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if let Ok(message @ ServerMessage::InstanceStatus { .. }) =
            serde_json::from_str::<ServerMessage>(line.trim_end())
        {
            return Some(message);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;
    use std::thread;

    use super::*;

    #[test]
    fn request_status_sends_the_session_id_and_reads_the_reply() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        let handle = thread::spawn(move || {
            let (mut conn, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(conn.try_clone().expect("clone"));
            let mut line = String::new();
            reader.read_line(&mut line).expect("read request");
            let reply = serde_json::to_string(&ServerMessage::InstanceStatus {
                known: true,
                session_id: "koshell-42".to_string(),
                cwd: Some("/home/u/proj".to_string()),
                shell: Some("/bin/zsh".to_string()),
                model: Some("anthropic/claude-sonnet-4-5".to_string()),
                conversation: true,
                daemon_pid: 1234,
                uptime_ms: 9000,
                version: "0.1.0".to_string(),
                protocol_version: 1,
                connections: 2,
            })
            .expect("serialize");
            conn.write_all(reply.as_bytes()).expect("write");
            conn.write_all(b"\n").expect("newline");
            conn.flush().expect("flush");
            line.trim_end().to_string()
        });

        let reply = request_status(&path, "koshell-42");
        let request = handle.join().expect("join");

        let parsed: ClientMessage = serde_json::from_str(&request).expect("parse request");
        match parsed {
            ClientMessage::InstanceStatusRequest { session_id } => {
                assert_eq!(session_id, "koshell-42");
            }
            other => panic!("unexpected request: {other:?}"),
        }
        match reply {
            Some(ServerMessage::InstanceStatus { known, model, .. }) => {
                assert!(known);
                assert_eq!(model.as_deref(), Some("anthropic/claude-sonnet-4-5"));
            }
            other => panic!("unexpected reply: {other:?}"),
        }
    }

    #[test]
    fn request_status_returns_none_when_the_daemon_hangs_up() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        let handle = thread::spawn(move || {
            let (conn, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(conn.try_clone().expect("clone"));
            let mut line = String::new();
            reader.read_line(&mut line).expect("read request");
            // Hang up without replying.
        });

        let reply = request_status(&path, "koshell-42");
        handle.join().expect("join");

        assert!(reply.is_none());
    }
}

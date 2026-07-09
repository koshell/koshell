//! `koshell reload` — asks the running AI daemon to re-read config.toml and
//! apply it to live terminal sessions (design 0015). A daemon-global IPC round
//! trip on the additive `reload_request`/`reload` pair (no hello, no ack), like
//! `daemon status`. By default it targets the current instance (via
//! `KOSHELL_SESSION_ID`); `--all` targets every active instance.
//!
//! It does NOT auto-spawn the daemon: a freshly started daemon already reads the
//! current config, so "not running" is nothing to reload.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use koshell_proto::{ClientMessage, ServerMessage};

use crate::daemon_cli::{self, Probe};
use crate::ipc;

/// How long to wait for the daemon's `reload` reply before treating silence as
/// an older daemon that ignored the request.
const RELOAD_TIMEOUT: Duration = Duration::from_secs(2);

/// Runs `koshell reload [--all]`, returning the process exit code.
pub fn run(all: bool) -> i32 {
    let socket_path = ipc::default_socket_path();
    match daemon_cli::probe(&socket_path) {
        Probe::Alive => {
            let session_id = if all {
                None
            } else {
                match ipc::current_session_id() {
                    Some(id) => Some(id),
                    None => {
                        eprintln!(
                            "not inside a koshell session; run `koshell reload --all` \
                             to reload every active instance."
                        );
                        return 1;
                    }
                }
            };
            match request_reload(&socket_path, session_id) {
                Some((ok, message)) => {
                    match message {
                        Some(message) => println!("{message}"),
                        None => println!("AI daemon: configuration reloaded"),
                    }
                    if ok { 0 } else { 1 }
                }
                None => {
                    eprintln!(
                        "the AI daemon did not answer the reload request — it likely \
                         predates reload support. Restart it with `koshell daemon restart`."
                    );
                    1
                }
            }
        }
        // No auto-spawn: a fresh daemon reads config.toml on its first #?.
        Probe::Stale | Probe::Absent => {
            println!("AI daemon: not running; nothing to reload");
            println!("  a freshly started daemon reads the current config.toml.");
            0
        }
    }
}

/// Sends one `reload_request` and returns `(ok, message)` from the `reload`
/// reply, or `None` if the daemon hung up without answering (an older daemon).
fn request_reload(path: &Path, session_id: Option<String>) -> Option<(bool, Option<String>)> {
    let mut stream = UnixStream::connect(path).ok()?;
    // Best-effort: macOS can reject SO_RCVTIMEO once the peer hung up; a failure
    // here just means we fall back to a blocking read, which the daemon ends by
    // closing the socket.
    let _ = stream.set_read_timeout(Some(RELOAD_TIMEOUT));
    let request = serde_json::to_string(&ClientMessage::ReloadRequest { session_id }).ok()?;
    stream.write_all(request.as_bytes()).ok()?;
    stream.write_all(b"\n").ok()?;
    stream.flush().ok()?;
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None; // EOF before a reply: an older daemon dropped it.
        }
        if let Ok(ServerMessage::Reload { ok, message }) =
            serde_json::from_str::<ServerMessage>(line.trim_end())
        {
            return Some((ok, message));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::BufReader;
    use std::os::unix::net::UnixListener;
    use std::thread;

    use super::*;

    // Scripts a one-shot daemon that reads one request line and replies with
    // `reply`, returning the request line it saw for assertion.
    fn scripted_daemon(path: &Path, reply: Option<ServerMessage>) -> thread::JoinHandle<String> {
        let listener = UnixListener::bind(path).expect("bind");
        thread::spawn(move || {
            let (mut conn, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(conn.try_clone().expect("clone"));
            let mut line = String::new();
            reader.read_line(&mut line).expect("read request");
            if let Some(reply) = reply {
                let encoded = serde_json::to_string(&reply).expect("serialize");
                conn.write_all(encoded.as_bytes()).expect("write");
                conn.write_all(b"\n").expect("newline");
                conn.flush().expect("flush");
            }
            line.trim_end().to_string()
        })
    }

    #[test]
    fn request_reload_sends_the_session_id_and_reads_the_reply() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("daemon.sock");
        let handle = scripted_daemon(
            &path,
            Some(ServerMessage::Reload {
                ok: true,
                message: Some("reloaded 1 session".to_string()),
            }),
        );

        let result = request_reload(&path, Some("koshell-42".to_string()));
        let request = handle.join().expect("join");

        assert_eq!(result, Some((true, Some("reloaded 1 session".to_string()))));
        let parsed: ClientMessage = serde_json::from_str(&request).expect("parse request");
        match parsed {
            ClientMessage::ReloadRequest { session_id } => {
                assert_eq!(session_id.as_deref(), Some("koshell-42"));
            }
            other => panic!("unexpected request: {other:?}"),
        }
    }

    #[test]
    fn request_reload_all_omits_the_session_id() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("daemon.sock");
        let handle = scripted_daemon(
            &path,
            Some(ServerMessage::Reload {
                ok: true,
                message: None,
            }),
        );

        let result = request_reload(&path, None);
        let request = handle.join().expect("join");

        assert_eq!(result, Some((true, None)));
        assert_eq!(request, r#"{"type":"reload_request"}"#);
    }

    #[test]
    fn request_reload_returns_none_when_the_daemon_hangs_up() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("daemon.sock");
        let handle = scripted_daemon(&path, None);

        let result = request_reload(&path, Some("koshell-42".to_string()));
        handle.join().expect("join");

        assert_eq!(result, None);
    }
}

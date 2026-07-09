//! IPC client to the koshell AI daemon: newline-delimited JSON over a Unix domain socket.
//!
//! The terminal connects lazily and degrades gracefully — if the daemon is unavailable the
//! terminal keeps working and `#?` is acknowledged as unavailable rather than blocking.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use koshell_proto::{ClientMessage, PROTOCOL_VERSION, ServerMessage};

/// A connected client to the AI daemon (the write half).
pub struct IpcClient {
    stream: UnixStream,
}

impl IpcClient {
    /// Connects to the daemon socket at `path`.
    pub fn connect(path: &PathBuf) -> anyhow::Result<Self> {
        let stream = UnixStream::connect(path)?;
        Ok(Self { stream })
    }

    /// Sends one message as a JSONL line.
    pub fn send(&mut self, message: &ClientMessage) -> anyhow::Result<()> {
        let line = serde_json::to_string(message)?;
        self.stream.write_all(line.as_bytes())?;
        self.stream.write_all(b"\n")?;
        self.stream.flush()?;
        Ok(())
    }

    /// Clones the read half of the connection for a dedicated reader thread.
    pub fn reader(&self) -> anyhow::Result<IpcReader> {
        Ok(IpcReader {
            reader: BufReader::new(self.stream.try_clone()?),
        })
    }
}

/// The read half of a daemon connection, owned by a dedicated reader thread.
pub struct IpcReader {
    reader: BufReader<UnixStream>,
}

impl IpcReader {
    /// Reads one server message (blocking). Returns `None` on clean EOF.
    ///
    /// Lines that are valid JSON but do not decode as a known [`ServerMessage`] are
    /// skipped (logged at debug), per the protocol's additive-evolution rule: a newer
    /// daemon may send message types this terminal does not know yet, and they must
    /// not kill the reader thread. Non-JSON lines are still hard errors — that is a
    /// framing bug, not evolution.
    pub fn recv(&mut self) -> anyhow::Result<Option<ServerMessage>> {
        loop {
            let mut line = String::new();
            if self.reader.read_line(&mut line)? == 0 {
                return Ok(None);
            }
            let trimmed = line.trim_end();
            match serde_json::from_str(trimmed) {
                Ok(message) => return Ok(Some(message)),
                Err(error) => {
                    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
                        log::debug!("ignoring unknown daemon message: {trimmed}");
                        continue;
                    }
                    return Err(error.into());
                }
            }
        }
    }
}

/// The per-user koshell runtime directory, following XDG conventions and deliberately
/// avoiding a world-writable `/tmp`: `$XDG_RUNTIME_DIR/koshell`, then
/// `$XDG_CACHE_HOME/koshell`, falling back to `~/.cache/koshell`.
pub fn runtime_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join("koshell");
    }
    if let Ok(dir) = std::env::var("XDG_CACHE_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join("koshell");
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".cache").join("koshell")
}

/// The default daemon socket path: `<runtime_dir>/daemon.sock`.
pub fn default_socket_path() -> PathBuf {
    runtime_dir().join("daemon.sock")
}

/// This wrapper's session id, `koshell-<pid>` — the same value sent in `hello`
/// and branded as field 0 of the `KOSHELL` environment variable.
pub fn session_id() -> String {
    format!("koshell-{}", std::process::id())
}

/// The current instance's session id, read from field 0 of the inherited `KOSHELL`
/// variable, or `None` when the process is not running inside a koshell wrapper. Child
/// processes (`koshell status`/`reload`) use it to address the current instance; the
/// value is the fixed wrapper pid, so it never goes stale for the life of the session.
pub fn current_session_id() -> Option<String> {
    let value = std::env::var(crate::shell::KOSHELL_ENV_KEY).ok()?;
    crate::shell::koshell_session_id(&value).map(str::to_string)
}

/// Builds a `hello` handshake for a new connection.
pub fn hello(cwd: String, shell: String, rows: u16, cols: u16) -> ClientMessage {
    ClientMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        terminal_session_id: session_id(),
        cwd,
        shell,
        rows,
        cols,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reader_of(lines: &str) -> IpcReader {
        let (mut writer, reader) = UnixStream::pair().expect("socketpair");
        writer.write_all(lines.as_bytes()).expect("write lines");
        drop(writer);
        IpcReader {
            reader: BufReader::new(reader),
        }
    }

    #[test]
    fn recv_skips_unknown_message_types_and_reads_eof() {
        let mut reader = reader_of(
            "{\"type\":\"brand_new_thing\",\"payload\":1}\n\
             {\"type\":\"ack\",\"request_id\":\"r1\"}\n",
        );
        match reader.recv().expect("recv known message") {
            Some(ServerMessage::Ack { request_id }) => assert_eq!(request_id, "r1"),
            other => panic!("unexpected message: {other:?}"),
        }
        assert!(reader.recv().expect("clean EOF").is_none());
    }

    #[test]
    fn recv_rejects_non_json_lines() {
        let mut reader = reader_of("not json at all\n");
        assert!(reader.recv().is_err());
    }
}

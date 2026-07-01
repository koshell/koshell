//! IPC client to the koshell AI daemon: newline-delimited JSON over a Unix domain socket.
//!
//! The terminal connects lazily and degrades gracefully — if the daemon is unavailable the
//! terminal keeps working and `#?` is acknowledged as unavailable rather than blocking.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use koshell_proto::{ClientMessage, PROTOCOL_VERSION, ServerMessage};

/// A connected client to the AI daemon.
pub struct IpcClient {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl IpcClient {
    /// Connects to the daemon socket at `path`.
    pub fn connect(path: &PathBuf) -> anyhow::Result<Self> {
        let stream = UnixStream::connect(path)?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Self { stream, reader })
    }

    /// Sends one message as a JSONL line.
    pub fn send(&mut self, message: &ClientMessage) -> anyhow::Result<()> {
        let line = serde_json::to_string(message)?;
        self.stream.write_all(line.as_bytes())?;
        self.stream.write_all(b"\n")?;
        self.stream.flush()?;
        Ok(())
    }

    /// Reads one server message (blocking). Returns `None` on clean EOF.
    pub fn recv(&mut self) -> anyhow::Result<Option<ServerMessage>> {
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        Ok(Some(serde_json::from_str(line.trim_end())?))
    }
}

/// The default daemon socket path, following XDG conventions:
/// `$XDG_RUNTIME_DIR/koshell/daemon.sock`, then `$XDG_CACHE_HOME/koshell/daemon.sock`,
/// falling back to `~/.cache/koshell/daemon.sock`.
pub fn default_socket_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join("koshell").join("daemon.sock");
    }
    if let Ok(dir) = std::env::var("XDG_CACHE_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join("koshell").join("daemon.sock");
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home)
        .join(".cache")
        .join("koshell")
        .join("daemon.sock")
}

/// Builds a `hello` handshake for a new connection.
pub fn hello(cwd: String, shell: String, rows: u16, cols: u16) -> ClientMessage {
    ClientMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        terminal_session_id: format!("koshell-{}", std::process::id()),
        cwd,
        shell,
        rows,
        cols,
    }
}

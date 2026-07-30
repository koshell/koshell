//! IPC client to the koshell AI daemon: newline-delimited JSON over a Unix domain socket.
//!
//! The terminal connects lazily and degrades gracefully — if the daemon is unavailable the
//! terminal keeps working and `#?` is acknowledged as unavailable rather than blocking.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use koshell_proto::{
    CAPABILITY_COMMAND_OUTPUT_TOOLS_V1, ClientMessage, PROTOCOL_VERSION, ServerMessage,
};

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
///
/// Always absolute. An empty `HOME` used to make the fallback *relative* (`.cache/koshell`),
/// which resolves against the current working directory — and koshell's cwd is not its own:
/// it follows the inner shell's `cd` (design 0005 working-directory mirroring). The socket
/// and the per-tty liveness markers would then be looked for in whatever directory the
/// reader happened to be in, so two processes that must agree on those paths would not. An
/// empty `HOME` now behaves exactly as the shell auto-wrap snippet's literal `$HOME/.cache`
/// already did — rooted at `/` — which is the sync the snippet's duplicated precedence
/// requires (design 0017). `/.cache/koshell` is normally unwritable, so the daemon simply
/// stays unreachable and the terminal degrades to a transparent wrapper; that is the
/// designed failure, unlike a path that silently moves.
pub fn runtime_dir() -> PathBuf {
    resolve_runtime_dir(|key| std::env::var(key).ok())
}

/// The pure resolution behind [`runtime_dir`], taking an environment reader so the
/// fallback chain is testable without mutating process-global environment (the same shape
/// [`crate::daemon_spawn::resolve_plan`] uses).
fn resolve_runtime_dir(var: impl Fn(&str) -> Option<String>) -> PathBuf {
    for key in ["XDG_RUNTIME_DIR", "XDG_CACHE_HOME"] {
        if let Some(dir) = var(key).filter(|dir| !dir.is_empty()) {
            return PathBuf::from(dir).join("koshell");
        }
    }
    let home = var("HOME")
        .filter(|home| !home.trim().is_empty())
        .unwrap_or_else(|| "/".to_string());
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

/// Builds a `hello` handshake for a new connection, advertising what this terminal
/// can serve. The daemon registers terminal-backed tools only for advertised
/// capabilities, so this list is what keeps a new daemon from calling into an old
/// terminal.
pub fn hello(cwd: String, shell: String, rows: u16, cols: u16) -> ClientMessage {
    ClientMessage::Hello {
        protocol_version: PROTOCOL_VERSION,
        terminal_session_id: session_id(),
        cwd,
        shell,
        rows,
        cols,
        capabilities: vec![CAPABILITY_COMMAND_OUTPUT_TOOLS_V1.to_string()],
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

    /// An environment reader over explicit pairs; anything unlisted reads as unset.
    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_string())
        }
    }

    #[test]
    fn the_runtime_directory_follows_the_xdg_precedence() {
        assert_eq!(
            resolve_runtime_dir(env_of(&[
                ("XDG_RUNTIME_DIR", "/run/user/1000"),
                ("XDG_CACHE_HOME", "/cache"),
                ("HOME", "/home/user"),
            ])),
            PathBuf::from("/run/user/1000/koshell")
        );
        // Empty is treated as unset at every step, so the chain keeps falling through.
        assert_eq!(
            resolve_runtime_dir(env_of(&[
                ("XDG_RUNTIME_DIR", ""),
                ("XDG_CACHE_HOME", "/cache"),
                ("HOME", "/home/user"),
            ])),
            PathBuf::from("/cache/koshell")
        );
        assert_eq!(
            resolve_runtime_dir(env_of(&[("HOME", "/home/user")])),
            PathBuf::from("/home/user/.cache/koshell")
        );
    }

    // The socket and the per-tty liveness markers hang off this directory and must mean the
    // same thing in two processes whose cwd moves independently, so a relative path is never
    // an acceptable answer — see `runtime_dir`.
    #[test]
    fn the_runtime_directory_is_always_absolute() {
        assert!(
            runtime_dir().is_absolute(),
            "absolute under the ambient environment too"
        );
        // A missing or blank HOME with no XDG override is the case that used to go relative;
        // it now matches the shell snippet's literal `$HOME/.cache`, rooted at `/`.
        for env in [
            env_of(&[]),
            env_of(&[("HOME", "")]),
            env_of(&[("HOME", "   ")]),
        ] {
            let path = resolve_runtime_dir(env);
            assert!(path.is_absolute(), "{path:?} must be absolute");
            assert_eq!(path, PathBuf::from("/.cache/koshell"));
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

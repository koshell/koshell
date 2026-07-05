//! End-to-end tests for `koshell daemon <status|start|stop>` (design 0008),
//! without a PTY: the binary is run directly and its exit code inspected. A
//! long-lived Python stub stands in for the daemon — it answers `status_request`
//! with its own pid and removes its socket on SIGTERM, so the start → status →
//! stop round-trip exercises the real IPC and signalling paths with no provider.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A long-lived stub daemon: binds the socket koshell resolves, answers
/// `status_request` with its pid, and cleans up on SIGTERM (or a safety alarm).
const STATUS_STUB_PY: &str = r#"
import json, os, signal, socket, sys

runtime = os.environ["XDG_RUNTIME_DIR"]
sock_dir = os.path.join(runtime, "koshell")
os.makedirs(sock_dir, exist_ok=True)
sock_path = os.path.join(sock_dir, "daemon.sock")

def cleanup(*_):
    try:
        os.unlink(sock_path)
    except FileNotFoundError:
        pass
    sys.exit(0)

signal.signal(signal.SIGTERM, cleanup)
signal.signal(signal.SIGALRM, cleanup)
signal.alarm(60)  # safety net so a failed test never leaks the daemon for long

try:
    os.unlink(sock_path)
except FileNotFoundError:
    pass

srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(sock_path)
srv.listen(5)
while True:
    conn, _ = srv.accept()
    f = conn.makefile("rwb", buffering=0)
    for raw in f:
        try:
            msg = json.loads(raw)
        except Exception:
            continue
        if msg.get("type") == "status_request":
            reply = {
                "type": "status",
                "pid": os.getpid(),
                "version": "stub-1.0",
                "protocol_version": 1,
                "uptime_ms": 42,
                "connections": 1,
            }
            f.write((json.dumps(reply) + "\n").encode())
    conn.close()
"#;

fn resolve(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// Runs `koshell daemon <action>` against the given runtime/state dirs and daemon
/// command, returning the exit code.
fn koshell_daemon(action: &str, runtime: &Path, state: &Path, daemon_cmd: &str) -> i32 {
    Command::new(env!("CARGO_BIN_EXE_koshell"))
        .args(["daemon", action])
        .env("XDG_RUNTIME_DIR", runtime)
        .env("XDG_STATE_HOME", state)
        .env("KOSHELL_DAEMON_CMD", daemon_cmd)
        .output()
        .expect("run koshell daemon")
        .status
        .code()
        .expect("koshell exited via a signal")
}

#[test]
fn status_reports_not_running_without_a_daemon() {
    let runtime = tempfile::tempdir().expect("runtime dir");
    let state = tempfile::tempdir().expect("state dir");
    let code = koshell_daemon("status", runtime.path(), state.path(), "false");
    assert_eq!(
        code, 1,
        "status must exit non-zero when no daemon is running"
    );
}

#[test]
fn start_status_stop_round_trip() {
    let Some(python) = resolve("python3") else {
        eprintln!("skipping daemon cli test: no python3");
        return;
    };

    let runtime = tempfile::tempdir().expect("runtime dir");
    let state = tempfile::tempdir().expect("state dir");
    let scripts = tempfile::tempdir().expect("scripts dir");
    let stub = scripts.path().join("status_stub.py");
    std::fs::write(&stub, STATUS_STUB_PY).expect("write stub");
    let daemon_cmd = format!("{} {}", python.display(), stub.display());

    let run = |action: &str| koshell_daemon(action, runtime.path(), state.path(), &daemon_cmd);

    // Not running yet.
    assert_eq!(run("status"), 1, "status before start");
    // Start spawns the stub and waits for it to become reachable.
    assert_eq!(run("start"), 0, "start should succeed");
    // Now running.
    assert_eq!(run("status"), 0, "status after start");
    // Stop signals it and waits for the socket to disappear.
    assert_eq!(run("stop"), 0, "stop should succeed");
    // Stopped again.
    assert_eq!(run("status"), 1, "status after stop");
}

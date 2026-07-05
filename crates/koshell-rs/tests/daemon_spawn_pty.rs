//! Real-PTY end-to-end test for daemon auto-spawn (design 0008): when no daemon
//! is reachable, a `#?` makes koshell spawn one via the resolved command and then
//! stream its answer. A stub daemon (a small Python script behind a shell wrapper
//! that records each launch) stands in for the real daemon, so the test needs no
//! provider or network — only that the spawn path fires exactly once and the
//! answer comes back. The kill switch must suppress the spawn entirely.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const OVERALL_TIMEOUT: Duration = Duration::from_secs(25);

/// The stub daemon: binds the same socket koshell resolves (from the inherited
/// `XDG_RUNTIME_DIR`), then answers one `ai_request` with a fixed streamed reply.
const STUB_DAEMON_PY: &str = r#"
import json, os, socket

runtime = os.environ["XDG_RUNTIME_DIR"]
sock_dir = os.path.join(runtime, "koshell")
os.makedirs(sock_dir, exist_ok=True)
sock_path = os.path.join(sock_dir, "daemon.sock")
try:
    os.unlink(sock_path)
except FileNotFoundError:
    pass

srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(sock_path)
srv.listen(1)
conn, _ = srv.accept()
f = conn.makefile("rwb", buffering=0)
for raw in f:
    try:
        msg = json.loads(raw)
    except Exception:
        continue
    if msg.get("type") == "ai_request":
        rid = msg["request_id"]
        for out in (
            {"type": "ack", "request_id": rid},
            {"type": "ai_delta", "request_id": rid, "delta": "STUB ANSWER"},
            {"type": "ai_response_end", "request_id": rid},
        ):
            f.write((json.dumps(out) + "\n").encode())
        break
"#;

/// Resolves an executable against the ambient `PATH`.
fn resolve(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// Runs koshell (wrapping bash) with `KOSHELL_DAEMON_CMD` pointing at the stub and
/// the event log / daemon log under temp dirs, drives `steps`, and returns the
/// captured output.
fn run_koshell_session(
    bash: &Path,
    python_dir: &Path,
    daemon_cmd: &str,
    no_spawn: bool,
    steps: &[(Duration, &[u8])],
) -> String {
    let home = tempfile::tempdir().expect("temp HOME");
    std::fs::write(home.path().join(".bashrc"), "HISTCONTROL=\n").expect("write .bashrc");
    let runtime = tempfile::tempdir().expect("temp XDG_RUNTIME_DIR");
    let state = tempfile::tempdir().expect("temp XDG_STATE_HOME");

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    let base_path = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";
    let path = format!("{}:{base_path}", python_dir.display());

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_koshell"));
    cmd.env_clear();
    cmd.env("SHELL", bash.as_os_str());
    cmd.env("HOME", home.path());
    cmd.env("XDG_RUNTIME_DIR", runtime.path());
    cmd.env("XDG_STATE_HOME", state.path());
    cmd.env("KOSHELL_DAEMON_CMD", daemon_cmd);
    if no_spawn {
        cmd.env("KOSHELL_NO_DAEMON_SPAWN", "1");
    }
    cmd.env("PATH", path);
    cmd.env("TERM", "xterm-256color");
    cmd.env("HISTFILE", home.path().join(".shell_history"));

    let mut child = pair.slave.spawn_command(cmd).expect("spawn koshell");
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().expect("clone reader");
    let mut writer = pair.master.take_writer().expect("take writer");

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let reader_handle = thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    for (delay, bytes) in steps {
        thread::sleep(*delay);
        let _ = writer.write_all(bytes);
        let _ = writer.flush();
    }
    drop(writer);

    let mut output = Vec::new();
    let deadline = Instant::now() + OVERALL_TIMEOUT;
    loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(chunk) => output.extend_from_slice(&chunk),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if Instant::now() >= deadline {
                    break;
                }
            }
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    let _ = reader_handle.join();
    String::from_utf8_lossy(&output).into_owned()
}

/// Writes the stub daemon and its counting shell wrapper into `dir`, returning the
/// `KOSHELL_DAEMON_CMD` command line and the spawn-count file path.
fn write_stub(dir: &Path) -> (String, PathBuf) {
    let stub_daemon = dir.join("stub_daemon.py");
    let stub_sh = dir.join("stub.sh");
    let count = dir.join("spawn-count");
    std::fs::write(&stub_daemon, STUB_DAEMON_PY).expect("write stub daemon");
    std::fs::write(
        &stub_sh,
        format!(
            "#!/bin/sh\necho spawned >> {count}\nexec python3 {py}\n",
            count = count.display(),
            py = stub_daemon.display(),
        ),
    )
    .expect("write stub wrapper");
    (format!("/bin/sh {}", stub_sh.display()), count)
}

fn spawn_count(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|contents| contents.lines().count())
        .unwrap_or(0)
}

#[test]
fn auto_spawn_serves_the_first_question() {
    let Some(bash) = resolve("bash") else {
        eprintln!("skipping: no bash");
        return;
    };
    let Some(python) = resolve("python3") else {
        eprintln!("skipping daemon spawn test: no python3");
        return;
    };
    let python_dir = python.parent().expect("python dir");

    let scripts = tempfile::tempdir().expect("scripts dir");
    let (daemon_cmd, count) = write_stub(scripts.path());

    let output = run_koshell_session(
        &bash,
        python_dir,
        &daemon_cmd,
        false,
        &[
            (Duration::from_millis(900), b"#? does spawn work"),
            (Duration::from_millis(300), b"\n"),
            // Room for the spawn (~200ms python start) and the streamed reply.
            (Duration::from_millis(2500), b"exit\n"),
        ],
    );

    assert!(
        output.contains("STUB ANSWER"),
        "the auto-spawned daemon's answer should stream back:\n{output}"
    );
    assert!(
        !output.contains("AI daemon unavailable"),
        "auto-spawn should have avoided the unavailable degrade:\n{output}"
    );
    assert_eq!(
        spawn_count(&count),
        1,
        "the daemon should have been spawned exactly once (cooldown holds)"
    );
}

#[test]
fn kill_switch_prevents_the_spawn() {
    let Some(bash) = resolve("bash") else {
        eprintln!("skipping: no bash");
        return;
    };
    let Some(python) = resolve("python3") else {
        eprintln!("skipping daemon spawn test: no python3");
        return;
    };
    let python_dir = python.parent().expect("python dir");

    let scripts = tempfile::tempdir().expect("scripts dir");
    let (daemon_cmd, count) = write_stub(scripts.path());

    let output = run_koshell_session(
        &bash,
        python_dir,
        &daemon_cmd,
        true,
        &[
            (Duration::from_millis(900), b"#? does spawn work"),
            (Duration::from_millis(300), b"\n"),
            (Duration::from_millis(900), b"exit\n"),
        ],
    );

    assert!(
        output.contains("AI daemon unavailable"),
        "with the kill switch set the question must degrade:\n{output}"
    );
    assert!(
        !count.exists(),
        "the kill switch must prevent any spawn (no count file)"
    );
}

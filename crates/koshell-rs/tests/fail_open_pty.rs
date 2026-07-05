//! Real-PTY end-to-end test for fail-open startup safety (audit gap #16).
//!
//! Under the `exec koshell` auto-wrap, a koshell that fails to start must fall through to
//! the user's real shell instead of exiting: an exited `exec`-ed koshell closes the
//! terminal, and on a Linux login TTY that can be the user's only way in. This drives
//! koshell into a guaranteed pre-takeover failure (the nested-koshell guard) and asserts
//! a live shell ends up on the terminal, carrying `KOSHELL_NO_AUTO=1` so its rc cannot
//! re-`exec` koshell straight back into the same crash.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const OVERALL_TIMEOUT: Duration = Duration::from_secs(20);

#[test]
fn failed_startup_falls_open_to_a_bare_shell() {
    let koshell_bin = Path::new(env!("CARGO_BIN_EXE_koshell"));
    let runtime = tempfile::tempdir().expect("create temp XDG_RUNTIME_DIR");

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    let mut cmd = CommandBuilder::new(koshell_bin);
    cmd.env_clear();
    // KOSHELL=1 trips the nested-koshell guard, a deterministic failure that happens
    // before koshell takes over the terminal — exactly the case fail-open must cover.
    cmd.env("KOSHELL", "1");
    cmd.env("KOSHELL_NO_DAEMON_SPAWN", "1");
    cmd.env("SHELL", "/bin/sh");
    cmd.env("XDG_RUNTIME_DIR", runtime.path());
    cmd.env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
    cmd.env("TERM", "xterm-256color");

    let mut child = pair.slave.spawn_command(cmd).expect("spawn koshell");
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().expect("clone pty reader");
    let mut writer = pair.master.take_writer().expect("take pty writer");

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

    // The fallback shell reads this at its prompt; `${KOSHELL_NO_AUTO:-none}` proves the
    // loop-guard env is set. The probe expands the value, so the echoed input line never
    // literally contains the answer.
    writer
        .write_all(b"echo FALLBACK-${KOSHELL_NO_AUTO:-none}\nexit\n")
        .expect("write probe");
    writer.flush().expect("flush probe");
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

    let output = String::from_utf8_lossy(&output);
    assert!(
        output.contains("FALLBACK-1"),
        "expected koshell to fail open to a live shell carrying KOSHELL_NO_AUTO=1.\n\
         --- captured PTY output ---\n{output}"
    );
}

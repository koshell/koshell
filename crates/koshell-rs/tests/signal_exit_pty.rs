//! Real-PTY end-to-end tests for faithful exit-code propagation (audit gap #7).
//!
//! koshell must report its inner program's fate the way a bare shell would: a normal
//! exit propagates the code verbatim, and a signal death propagates `128 + signo` (the
//! value `$?` reports for a signal-killed child). `portable_pty`'s own `ExitStatus`
//! collapses every signal death to code 1 (it renders the numeric signal to a localized
//! string and drops the number), so koshell reaps the pid via `waitpid` and reads
//! `WIFSIGNALED`/`WTERMSIG` itself; these tests pin that behavior end to end.
//!
//! koshell exits normally with the computed code, so the outer `ExitStatus` observed
//! here carries it verbatim — the collapse only afflicts a process that dies *by* a
//! signal, which koshell never does here.

use std::io::Read;
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use portable_pty::{CommandBuilder, ExitStatus, PtySize, native_pty_system};

/// Upper bound on a single run; koshell normally exits as soon as its inner program does.
const OVERALL_TIMEOUT: Duration = Duration::from_secs(20);

/// Runs `koshell <args>` inside a PTY (koshell refuses to start without a TTY on stdin
/// and stdout) and returns koshell's own exit code once the inner program and koshell
/// have both exited.
fn koshell_exit_code(args: &[&str]) -> u32 {
    let koshell_bin = Path::new(env!("CARGO_BIN_EXE_koshell"));
    // Empty XDG_RUNTIME_DIR => daemon socket absent; exit propagation is daemon-agnostic.
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
    // Keep the run hermetic: never auto-spawn an installed koshell-ai-daemon.
    cmd.env("KOSHELL_NO_DAEMON_SPAWN", "1");
    cmd.env("XDG_RUNTIME_DIR", runtime.path());
    cmd.env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
    cmd.env("TERM", "xterm-256color");
    for arg in args {
        cmd.arg(arg);
    }

    let mut child = pair.slave.spawn_command(cmd).expect("spawn koshell");
    drop(pair.slave);

    // Drain the master so the inner program never blocks on a full PTY buffer.
    let mut reader = pair.master.try_clone_reader().expect("clone pty reader");
    let reader_handle = thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
        }
    });

    // Wait for koshell in a helper thread so a hang trips the timeout instead of blocking
    // the test forever; a killer clone lets us tear a stuck run down.
    let mut killer = child.clone_killer();
    let (tx, rx) = mpsc::channel::<std::io::Result<ExitStatus>>();
    let waiter = thread::spawn(move || {
        let _ = tx.send(child.wait());
    });
    let status = match rx.recv_timeout(OVERALL_TIMEOUT) {
        Ok(status) => status.expect("wait for koshell"),
        Err(_) => {
            let _ = killer.kill();
            panic!("koshell did not exit within {OVERALL_TIMEOUT:?}");
        }
    };
    let _ = waiter.join();
    let _ = reader_handle.join();

    status.exit_code()
}

#[test]
fn normal_exit_code_is_propagated_verbatim() {
    // A plain exit code must pass through untouched (the WIFEXITED path).
    assert_eq!(
        koshell_exit_code(&["/bin/sh", "-c", "exit 42"]),
        42,
        "koshell should propagate the inner program's exit code verbatim"
    );
}

#[test]
fn signal_death_is_reported_as_128_plus_signo() {
    // The inner shell kills itself with SIGTERM (15); a bare shell reports 143. Without
    // the waitpid bypass, portable-pty would collapse this to 1.
    assert_eq!(
        koshell_exit_code(&["/bin/sh", "-c", "kill -TERM $$"]),
        143,
        "a SIGTERM death should surface as 128 + 15"
    );
}

#[test]
fn sigkill_death_is_reported_as_128_plus_signo() {
    // SIGKILL (9) cannot be caught, so this isolates the reaping path from any trap
    // behavior: a bare shell reports 137.
    assert_eq!(
        koshell_exit_code(&["/bin/sh", "-c", "kill -KILL $$"]),
        137,
        "a SIGKILL death should surface as 128 + 9"
    );
}

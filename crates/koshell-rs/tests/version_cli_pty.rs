//! Real-PTY acceptance for `koshell version` (design 0024).
//!
//! The claim worth testing is the one only a real session can make: the koshell wrapping a
//! terminal records its version where a child process can find it, and `koshell version`
//! run inside that terminal reports *that* koshell rather than the binary it happens to be.
//! Everything in between — the wrapper writing the file beside its liveness marker under
//! the runtime directory, the child resolving its own controlling tty, and the two agreeing
//! on the conventional path — is exercised end to end here and nowhere else.
//!
//! No daemon is reachable (the runtime directory is a fresh temp dir and auto-spawn is off),
//! so the daemon row reports "not running"; that is the same shape a user sees before their
//! first `#?`, and it proves the terminal rows do not depend on daemon availability.

use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const OVERALL_TIMEOUT: Duration = Duration::from_secs(20);

const ZSH_CANDIDATES: [&str; 3] = ["/bin/zsh", "/usr/bin/zsh", "/opt/homebrew/bin/zsh"];

fn find_shell(candidates: &[&'static str]) -> Option<&'static str> {
    candidates
        .iter()
        .copied()
        .find(|candidate| Path::new(candidate).exists())
}

/// Spawns `koshell` wrapping `shell` in a PTY with an isolated HOME and runtime directory
/// and no reachable daemon, with the built binary's own directory first on `PATH` so the
/// inner shell's `koshell version` runs the binary under test. Drives `script` and returns
/// everything printed back to the PTY.
fn drive_koshell(shell: &str, script: &[u8]) -> String {
    let koshell_bin = Path::new(env!("CARGO_BIN_EXE_koshell"));
    let bin_dir = koshell_bin.parent().expect("binary directory");
    let home = tempfile::tempdir().expect("create temp HOME");
    let runtime = tempfile::tempdir().expect("create temp XDG_RUNTIME_DIR");

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    let mut cmd = CommandBuilder::new(koshell_bin);
    cmd.env_clear();
    cmd.env("KOSHELL_NO_DAEMON_SPAWN", "1");
    cmd.env("SHELL", shell);
    cmd.env("HOME", home.path());
    cmd.env("XDG_RUNTIME_DIR", runtime.path());
    cmd.env(
        "PATH",
        format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", bin_dir.display()),
    );
    cmd.env("TERM", "xterm-256color");
    cmd.env("HISTFILE", home.path().join(".shell_history"));

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

    writer.write_all(script).expect("write driver script");
    writer.flush().expect("flush driver script");

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
    drop(writer);

    let _ = child.kill();
    let _ = child.wait();
    let _ = reader_handle.join();

    String::from_utf8_lossy(&output).into_owned()
}

/// The reported row for `label`, with the terminal's carriage returns removed. Panics with
/// the whole transcript when the row is missing, which is the failure worth reading.
fn row<'a>(output: &'a str, label: &str) -> &'a str {
    output
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .find(|line| line.starts_with(label))
        .unwrap_or_else(|| panic!("no {label:?} row in the session transcript:\n{output}"))
}

#[test]
fn inside_a_session_the_tty_row_reports_the_koshell_that_wraps_it() {
    let Some(zsh) = find_shell(&ZSH_CANDIDATES) else {
        eprintln!("skipping zsh test: no zsh interpreter found");
        return;
    };

    let output = drive_koshell(zsh, b"koshell version\nexit\n");

    // The wrapper is this build, so both rows name the same version — the equality that
    // proves the version file was written and found, not that two constants match.
    let binary = row(&output, "koshell:");
    assert!(
        binary.contains(koshell_rs::VERSION),
        "the binary row names this build: {binary}"
    );
    let tty = row(&output, "this tty:");
    assert!(
        tty.contains(koshell_rs::VERSION),
        "the wrapping koshell is identified by version, not reported unknown: {tty}"
    );
    assert!(
        tty.contains("pid "),
        "the wrapping koshell is identified by pid too: {tty}"
    );
    assert!(
        !output.contains("not wrapped by koshell"),
        "a wrapped terminal is never reported as unwrapped:\n{output}"
    );
    assert!(
        !output.contains("no live marker names this terminal"),
        "the liveness marker written at startup is found:\n{output}"
    );
    // Same build on both rows, so the "different build" note has nothing to say.
    assert!(
        !output.contains("a different build than this binary"),
        "no mismatch is claimed between a wrapper and the binary that spawned it:\n{output}"
    );

    // The daemon row degrades to a fact plus the way to get one, never to a version it
    // could not have observed.
    let daemon = row(&output, "koshell-ai-daemon:");
    assert!(daemon.contains("not running"), "{daemon}");
    assert!(output.contains("would start:"), "{output}");
}

// The case the command exists for, which cannot be staged with two builds in one test run:
// a terminal wrapped by a *different* koshell than the binary being typed. Rewriting the
// version file in place is exactly the state an older wrapper leaves behind, and it also
// pins the path convention the file is found by — the shell reconstructs it from `$(tty)`
// the same way `shell::tty_version_path` does.
#[test]
fn a_terminal_wrapped_by_another_build_is_reported_as_such() {
    let Some(zsh) = find_shell(&ZSH_CANDIDATES) else {
        eprintln!("skipping zsh test: no zsh interpreter found");
        return;
    };

    let output = drive_koshell(
        zsh,
        b"echo 00000000.000000 > \"$XDG_RUNTIME_DIR/koshell/tty/$(tty | tr / _).version\"\n\
          koshell version\n\
          exit\n",
    );

    let tty = row(&output, "this tty:");
    assert!(
        tty.contains("00000000.000000"),
        "the wrapper's recorded version is reported, not the running binary's: {tty}"
    );
    assert!(
        output.contains("a different build than this binary"),
        "the mismatch is stated:\n{output}"
    );
    assert!(
        output.contains(koshell_rs::VERSION),
        "and names what a new terminal would run:\n{output}"
    );
}

// Outside any session the command still answers — the binary version is always knowable —
// and says plainly that no koshell owns this terminal.
#[test]
fn outside_a_session_only_the_binary_version_is_claimed() {
    let runtime = tempfile::tempdir().expect("create temp XDG_RUNTIME_DIR");
    let output = Command::new(env!("CARGO_BIN_EXE_koshell"))
        .arg("version")
        .env_remove("KOSHELL")
        .env("KOSHELL_NO_DAEMON_SPAWN", "1")
        .env("XDG_RUNTIME_DIR", runtime.path())
        .output()
        .expect("run koshell version");

    assert!(
        output.status.success(),
        "reporting is not a probe: it succeeds"
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(row(&text, "koshell:").contains(koshell_rs::VERSION));
    assert!(row(&text, "this tty:").contains("not wrapped by koshell"));
    assert!(row(&text, "koshell-ai-daemon:").contains("not running"));
}

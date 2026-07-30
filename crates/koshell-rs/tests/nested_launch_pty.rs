//! Real-PTY regression tests for a refused nested launch (fix 0011).
//!
//! Launching `koshell` from inside a live koshell on the same terminal is refused. The
//! refusal used to be treated as an ordinary startup failure, so the generic fail-open
//! `exec`-ed a bare shell over the refusing process — handing the user the very nesting the
//! message said it was preventing: an extra shell layer with no koshell integration, one
//! more `exit` to leave, and the wrapping koshell left holding a command span that never
//! ends.
//!
//! The discriminator is whether this process replaced the shell that ran it. These tests
//! drive both sides of it against the real binary by writing the conventional tty liveness
//! marker with a chosen pid:
//!
//! - marker pid = a live *third* process => a plain `koshell` typed at a prompt: the caller's
//!   shell is still there, so refuse and exit, and start no shell.
//! - marker pid = koshell's own parent => an `exec koshell` (the auto-wrap snippet, or a
//!   hand-typed one): no shell is left, so the fail-open must still fire.
//!
//! `fail_open_pty.rs` covers the third case, a brand with no tty field at all: nothing can
//! be proven about who is above us, so the terminal-preserving fail-open stays.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const OVERALL_TIMEOUT: Duration = Duration::from_secs(20);

/// What a refused-but-recovered session's shell answers, proving a live shell owns the
/// terminal and carries the re-`exec` loop guard.
const PROBE: &[u8] = b"echo FALLBACK-${KOSHELL_NO_AUTO:-none}\nexit\n";

/// A long-lived process whose pid stands in for "some other live koshell". Killed on drop
/// so a failing assertion never leaks it.
struct LiveProcess(Child);

impl LiveProcess {
    fn spawn() -> Self {
        LiveProcess(
            Command::new("/bin/sh")
                .arg("-c")
                .arg("sleep 60")
                .spawn()
                .expect("spawn a placeholder live process"),
        )
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for LiveProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Outcome of driving one nested launch.
struct Run {
    output: String,
    exit_code: i32,
}

/// Spawns `koshell` on a fresh PTY already branded as wrapped by a live koshell: `KOSHELL`
/// carries the pts koshell is about to run on, and the conventional liveness marker for that
/// pts holds `owner_pid`. `owner_pid` is resolved from the spawned child's own parent (this
/// test process) when `owner_is_parent`, which is what an `exec koshell` looks like.
fn drive_nested_launch(owner_pid: u32) -> Run {
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

    // The pts koshell will run on: the same spelling `KOSHELL`'s tty field, the shell's
    // `$(tty)`, and koshell's own `ttyname(0)` use (design 0009).
    let tty = pair
        .master
        .tty_name()
        .expect("resolve the slave pts path")
        .to_string_lossy()
        .into_owned();
    let marker = runtime
        .path()
        .join("koshell")
        .join("tty")
        .join(tty.replace('/', "_"));
    std::fs::create_dir_all(marker.parent().expect("marker parent")).expect("marker dir");
    std::fs::write(&marker, owner_pid.to_string()).expect("write liveness marker");

    let mut cmd = CommandBuilder::new(koshell_bin);
    cmd.env_clear();
    cmd.env("KOSHELL", format!("koshell-1,{tty}"));
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

    // Queued in the PTY buffer: only a shell that ends up on this terminal ever reads it.
    writer.write_all(PROBE).expect("write probe");
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

    let exit_code = child
        .wait()
        .map(|status| status.exit_code() as i32)
        .unwrap_or(-1);
    let _ = reader_handle.join();

    Run {
        output: String::from_utf8_lossy(&output).into_owned(),
        exit_code,
    }
}

#[test]
fn a_nested_launch_at_a_prompt_refuses_without_starting_a_shell() {
    let owner = LiveProcess::spawn();
    let run = drive_nested_launch(owner.pid());

    assert!(
        run.output.contains("already wraps this terminal"),
        "expected the refusal to name the koshell that already owns the terminal.\n\
         --- captured PTY output ---\n{}",
        run.output
    );
    // The probe expands `${KOSHELL_NO_AUTO:-none}`, so only a shell that actually ran it
    // can produce either expansion; the line discipline's echo of the queued input keeps
    // the unexpanded form.
    assert!(
        !run.output.contains("FALLBACK-1") && !run.output.contains("FALLBACK-none"),
        "expected NO shell on the terminal: the caller's shell is still waiting, so \
         failing open would nest an extra shell inside it.\n\
         --- captured PTY output ---\n{}",
        run.output
    );
    assert_eq!(
        run.exit_code, 1,
        "the refusal is the faithful outcome: a non-zero exit back to the caller's shell.\n\
         --- captured PTY output ---\n{}",
        run.output
    );
}

#[test]
fn a_nested_launch_that_replaced_the_shell_still_falls_open() {
    // koshell's parent is this test process, so an owner pid of ours is exactly the
    // `exec koshell` shape: the shell's process image is gone and exiting would close the
    // terminal.
    let run = drive_nested_launch(std::process::id());

    assert!(
        run.output.contains("FALLBACK-1"),
        "expected the fail-open to keep a live shell (carrying KOSHELL_NO_AUTO=1) on a \
         terminal whose shell koshell replaced.\n--- captured PTY output ---\n{}",
        run.output
    );
}

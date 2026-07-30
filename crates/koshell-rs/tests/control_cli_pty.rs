//! Real-PTY acceptance for `koshell new` and `koshell clear` (design 0023).
//!
//! Both commands run as an ordinary child of the inner shell and address the wrapper with
//! an OSC 777 control marker written to `/dev/tty`. Only a real session exercises that
//! whole loop: the child resolving "am I inside a live koshell", the marker crossing the
//! pts, the wrapper's scanner stripping it out of the byte stream, and the notice coming
//! back. What these assert is the user-visible contract — the marker never renders as
//! garbage, the notice appears, and `clear` really wipes the screen.
//!
//! No daemon is reachable here, so the conversation half reports "no AI conversation to
//! reset"; that is exactly the wording a user gets before their first `#?`, and it proves
//! the terminal half runs independently of daemon availability.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const OVERALL_TIMEOUT: Duration = Duration::from_secs(20);

fn find_shell(candidates: &[&'static str]) -> Option<&'static str> {
    candidates
        .iter()
        .copied()
        .find(|candidate| Path::new(candidate).exists())
}

const ZSH_CANDIDATES: [&str; 3] = ["/bin/zsh", "/usr/bin/zsh", "/opt/homebrew/bin/zsh"];

/// Spawns `koshell` wrapping `shell` in a PTY with an isolated HOME and no reachable
/// daemon, with the built `koshell` binary's own directory first on `PATH` so the inner
/// shell's `koshell new` / `koshell clear` run the binary under test. Drives `script` and
/// returns everything printed back to the PTY.
fn drive_koshell(shell: &str, script: &[u8]) -> String {
    let koshell_bin = Path::new(env!("CARGO_BIN_EXE_koshell"));
    let bin_dir = koshell_bin.parent().expect("binary directory");
    let home = tempfile::tempdir().expect("create temp HOME");
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

#[test]
fn new_resets_the_conversation_and_leaves_the_screen_alone() {
    let Some(zsh) = find_shell(&ZSH_CANDIDATES) else {
        eprintln!("skipping zsh test: no zsh interpreter found");
        return;
    };

    let output = drive_koshell(zsh, b"echo BEFORE-NEW-OUTPUT\nkoshell new\nexit\n");

    assert!(
        output.contains("no AI conversation to reset"),
        "expected the wrapper's notice for `koshell new` with no daemon reachable.\n\
         --- captured PTY output ---\n{output}"
    );
    // The control marker is stripped before the terminal sees it: no OSC 777 payload and
    // no stray `koshell;` fragment survives into the visible stream.
    assert!(
        !output.contains("\x1b]777;koshell;"),
        "the control marker must never render on the terminal.\n\
         --- captured PTY output ---\n{output}"
    );
    // `new` is about the conversation only; earlier output stays on screen.
    assert!(
        output.contains("BEFORE-NEW-OUTPUT"),
        "`koshell new` must not touch the screen.\n\
         --- captured PTY output ---\n{output}"
    );
}

#[test]
fn clear_wipes_the_screen_and_says_what_it_dropped() {
    let Some(zsh) = find_shell(&ZSH_CANDIDATES) else {
        eprintln!("skipping zsh test: no zsh interpreter found");
        return;
    };

    let output = drive_koshell(zsh, b"echo BEFORE-CLEAR-OUTPUT\nkoshell clear\nexit\n");

    assert!(
        output.contains("screen and terminal context cleared"),
        "expected the wrapper's notice for `koshell clear`.\n\
         --- captured PTY output ---\n{output}"
    );
    assert!(
        !output.contains("\x1b]777;koshell;"),
        "the control marker must never render on the terminal.\n\
         --- captured PTY output ---\n{output}"
    );
    // The screen erase and the scrollback erase both reach the terminal, so what the user
    // asked to forget is not one scroll away.
    assert!(
        output.contains("\x1b[2J") && output.contains("\x1b[3J"),
        "expected the screen and scrollback erase to reach the terminal.\n\
         --- captured PTY output ---\n{output}"
    );
}

#[test]
fn outside_a_koshell_session_the_commands_refuse() {
    // No `KOSHELL` in the environment: nothing is wrapping this terminal, so there is no
    // wrapper to address and the commands must say so rather than write a marker nobody
    // strips (which would print garbage on a plain terminal).
    // An isolated HOME/state dir so the log koshell opens on startup lands somewhere
    // disposable instead of in the developer's own state directory.
    let home = tempfile::tempdir().expect("create temp HOME");
    for subcommand in ["new", "clear"] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_koshell"))
            .arg(subcommand)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", home.path())
            .env("XDG_STATE_HOME", home.path().join("state"))
            .output()
            .expect("run koshell outside a session");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "`koshell {subcommand}` outside a session must fail: {stderr}"
        );
        assert!(
            stderr.contains("needs a koshell-wrapped terminal"),
            "expected the out-of-session guidance from `koshell {subcommand}`, got: {stderr}"
        );
        assert!(
            output.stdout.is_empty(),
            "`koshell {subcommand}` writes nothing to stdout"
        );
    }
}

//! Real-PTY regression tests for the pending-trigger interaction rules and the arming /
//! suppression conditions of `#?` (`docs/design-0001-repl-command-completion.md`):
//!
//! - echo arming: input that is never echoed must not trigger;
//! - quote parity: `echo "#? ..."` must not trigger;
//! - stabilization on a non-terminating command: an inline `#?` on a still-running command
//!   fires at the output-stabilization point, annotated, and exactly once;
//! - Esc is forwarded untouched and cancels nothing (the bare-Esc cancel path was
//!   removed by design 0006; Ctrl+C is the only cancel key);
//! - a user-typed Ctrl+C cancels the pending question (no fire at the failure marker).
//!
//! All tests run the real `koshell` binary wrapping bash in a PTY with no reachable AI
//! daemon, so a fire prints the graceful-degrade `AI daemon unavailable` feedback line.
//! Question lines are written in two steps — text first (echo renders into the mirror),
//! then the newline — the same shape as human typing.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const OVERALL_TIMEOUT: Duration = Duration::from_secs(25);

/// Resolves an executable against the ambient `PATH`.
fn resolve(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// Runs koshell (wrapping bash) and drives `steps` into it, each after its delay, then
/// drains output until koshell exits or the safety deadline hits.
fn run_koshell_session(bash: &Path, steps: &[(Duration, &[u8])]) -> String {
    let home = tempfile::tempdir().expect("temp HOME");
    std::fs::write(home.path().join(".bashrc"), "HISTCONTROL=\n").expect("write .bashrc");
    let runtime = tempfile::tempdir().expect("temp XDG_RUNTIME_DIR");

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_koshell"));
    cmd.env_clear();
    // Disable daemon auto-spawn so the no-daemon scenarios stay hermetic (an
    // installed koshell-ai-daemon on PATH would otherwise be launched).
    cmd.env("KOSHELL_NO_DAEMON_SPAWN", "1");
    cmd.env("SHELL", bash.as_os_str());
    cmd.env("HOME", home.path());
    cmd.env("XDG_RUNTIME_DIR", runtime.path());
    cmd.env(
        "PATH",
        "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
    );
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

#[test]
fn non_echoed_input_does_not_trigger() {
    let Some(bash) = resolve("bash") else {
        eprintln!("skipping: no bash");
        return;
    };

    // `read -s` reads a line without echoing it; the typed `#?` never reaches the mirror,
    // so the trigger stays disarmed by construction.
    let output = run_koshell_session(
        &bash,
        &[
            (Duration::from_millis(900), b"read -s ans\n"),
            (Duration::from_millis(600), b"#? is this armed"),
            (Duration::from_millis(300), b"\n"),
            (Duration::from_millis(900), b"exit\n"),
        ],
    );

    assert!(
        !output.contains("[koshell] #?"),
        "`#?` typed into a non-echoing program must not trigger.\n--- output ---\n{output}"
    );
}

#[test]
fn quoted_trigger_does_not_fire() {
    let Some(bash) = resolve("bash") else {
        eprintln!("skipping: no bash");
        return;
    };

    let output = run_koshell_session(
        &bash,
        &[
            (Duration::from_millis(900), b"echo \"#? not a question\""),
            (Duration::from_millis(300), b"\n"),
            (Duration::from_millis(900), b"exit\n"),
        ],
    );

    assert!(
        !output.contains("[koshell] #?"),
        "a `#?` inside quotes must not trigger.\n--- output ---\n{output}"
    );
}

#[test]
fn still_running_command_fires_at_stabilization_exactly_once() {
    let Some(bash) = resolve("bash") else {
        eprintln!("skipping: no bash");
        return;
    };

    // The printf leaves a prompt-shaped resting line, selecting the fast stabilization
    // tier while the sleep keeps the command running. The sentinel is split (`''`) so it
    // appears only in output, after the fire.
    let output = run_koshell_session(
        &bash,
        &[
            (
                Duration::from_millis(900),
                b"printf 'remote$ ' && sleep 4 && echo COMPLETION''MARK  #? what is this",
            ),
            (Duration::from_millis(300), b"\n"),
            (Duration::from_millis(5500), b"exit\n"),
        ],
    );

    let fire = output
        .find("[koshell] #?")
        .unwrap_or_else(|| panic!("the inline `#?` never fired.\n--- output ---\n{output}"));
    let sentinel = output
        .find("COMPLETIONMARK")
        .unwrap_or_else(|| panic!("command output sentinel missing.\n--- output ---\n{output}"));
    assert!(
        fire < sentinel,
        "stabilization should fire before the command finishes (fire at {fire}, sentinel \
         at {sentinel}).\n--- output ---\n{output}"
    );
    assert!(
        output.contains("command may still be running"),
        "the still-running annotation is missing.\n--- output ---\n{output}"
    );
    let fires = output.matches("AI daemon unavailable").count();
    assert_eq!(
        fires, 1,
        "the question must fire exactly once (command_end must not re-fire it).\n\
         --- output ---\n{output}"
    );
}

#[test]
fn esc_is_forwarded_untouched_and_cancels_nothing() {
    // Regression guard for the bare-Esc cancel removal (design 0006): Ctrl+C is
    // the only cancel key. An Esc pressed while a question is pending goes to the
    // foreground program like any other byte, and the question still fires.
    let Some(bash) = resolve("bash") else {
        eprintln!("skipping: no bash");
        return;
    };

    let output = run_koshell_session(
        &bash,
        &[
            (
                Duration::from_millis(900),
                b"cat > /dev/null  #? still fires",
            ),
            (Duration::from_millis(300), b"\n"),
            // Into cat's stdin: previously this cancelled the pending question.
            (Duration::from_millis(1500), b"\x1b"),
            // First ^D flushes the partial line to cat, second is EOF; the
            // command ends and the authoritative marker fires the question.
            (Duration::from_millis(800), b"\x04\x04"),
            (Duration::from_millis(600), b"exit\n"),
        ],
    );

    assert!(
        !output.contains("#? cancelled"),
        "Esc must not cancel anything.\n--- output ---\n{output}"
    );
    assert!(
        output.contains("AI daemon unavailable"),
        "the question must still fire after an Esc keypress.\n--- output ---\n{output}"
    );
}

#[test]
fn ctrl_c_cancels_pending_question() {
    let Some(bash) = resolve("bash") else {
        eprintln!("skipping: no bash");
        return;
    };

    let output = run_koshell_session(
        &bash,
        &[
            (Duration::from_millis(900), b"sleep 3  #? why slow"),
            (Duration::from_millis(300), b"\n"),
            (Duration::from_millis(1500), b"\x03"),
            (Duration::from_millis(1200), b"exit\n"),
        ],
    );

    assert!(
        output.contains("#? cancelled (^C): why slow"),
        "the Ctrl+C cancel notice is missing.\n--- output ---\n{output}"
    );
    assert!(
        !output.contains("AI daemon unavailable"),
        "a cancelled question must not fire at the interrupt's command_end.\n\
         --- output ---\n{output}"
    );
}

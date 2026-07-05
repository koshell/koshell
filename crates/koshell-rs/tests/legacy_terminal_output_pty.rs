//! Real-PTY regression tests for Koshell-owned output in legacy terminal environments.
//!
//! These tests intentionally run with a low-capability terminal environment (`TERM=dumb`,
//! `NO_COLOR=1`, and no `COLORTERM`) and verify that Koshell's own feedback remains
//! readable without relying on True Color, 256-color, or hyperlink escape sequences. The
//! current placeholder style may still use basic SGR dim/reset; the compatibility guard is
//! that richer terminal features do not become required for understanding the output.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const OVERALL_TIMEOUT: Duration = Duration::from_secs(20);

fn resolve(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

fn run_koshell_with_legacy_terminal_env(bash: &Path, script: &[u8]) -> String {
    let home = tempfile::tempdir().expect("temp HOME");
    std::fs::write(home.path().join(".bashrc"), "HISTCONTROL=\nPS1='$ '\n").expect("write .bashrc");
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
    cmd.env("TERM", "dumb");
    cmd.env("NO_COLOR", "1");
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

fn assert_no_rich_terminal_sequences(output: &str) {
    for sequence in [
        "\x1b[38;2",
        "\x1b[48;2",
        "\x1b[38:2",
        "\x1b[48:2",
        "\x1b[38;5",
        "\x1b[48;5",
        "\x1b[38:5",
        "\x1b[48:5",
        "\x1b]8;",
    ] {
        assert!(
            !output.contains(sequence),
            "Koshell feedback must remain readable on legacy/no-color terminals; found \
             unsupported sequence {sequence:?}.\n--- output ---\n{output}"
        );
    }
}

#[test]
fn hash_question_feedback_is_readable_with_dumb_no_color_terminal() {
    let Some(bash) = resolve("bash") else {
        eprintln!("skipping: no bash");
        return;
    };

    let output =
        run_koshell_with_legacy_terminal_env(&bash, b"#? explain legacy terminal output\nexit\n");

    assert!(
        output.contains("[koshell] #? received"),
        "Koshell feedback label is missing.\n--- output ---\n{output}"
    );
    assert!(
        output.contains("AI daemon unavailable"),
        "graceful-degrade message is missing.\n--- output ---\n{output}"
    );
    assert!(
        output.contains("explain legacy terminal output"),
        "question text is missing from feedback.\n--- output ---\n{output}"
    );
    assert_no_rich_terminal_sequences(&output);
}

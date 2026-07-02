//! Real-PTY regression tests for `#?` typed inside a foreground CLI program (a REPL), where
//! shell integration is dormant and koshell must detect the stabilization point itself. See
//! `docs/design-0001-repl-command-completion.md`.
//!
//! Each test runs koshell wrapping bash, launches a REPL, and submits a line that both prints
//! a sentinel and carries a `#?`. The trigger must fire *after* the command's output (the
//! deferred-until-stabilization behavior). Both python and node go through the same
//! output-stabilization path: the REPL prompt returns, the resting line is prompt-shaped, and
//! the fast debounce tier fires.
//!
//! Capture is a mirror read of the rendered line at the Enter instant, so each question line
//! is written in two steps — text first (echo renders), then the newline — the same shape as
//! human typing or a bracketed paste followed by Enter.
//!
//! The sentinel is assembled at runtime (`'COMPLETION' + 'MARK'`) so the string
//! `COMPLETIONMARK` appears only in the command's *output*, never in the echoed input line —
//! letting us assert ordering against the `[koshell] #?` feedback.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const OVERALL_TIMEOUT: Duration = Duration::from_secs(25);

/// Resolves an executable against the ambient `PATH`, so version-managed interpreters (fnm,
/// pyenv, …) that live outside the standard directories are still found.
fn resolve(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// Runs koshell (wrapping bash) and drives `steps` into it, each after its delay, then drains
/// output until koshell exits or the safety deadline hits. `extra_path_dir` is prepended to
/// the child's `PATH` so a resolved interpreter is reachable inside the wrapped shell.
fn run_koshell_session(
    bash: &Path,
    extra_path_dir: Option<&Path>,
    steps: &[(Duration, &[u8])],
) -> String {
    let home = tempfile::tempdir().expect("temp HOME");
    // Keep bash history dedup off; irrelevant to the REPL path but keeps the shell quiet.
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

    let base_path = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";
    let path = match extra_path_dir {
        Some(dir) => format!("{}:{base_path}", dir.display()),
        None => base_path.to_string(),
    };

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_koshell"));
    cmd.env_clear();
    cmd.env("SHELL", bash.as_os_str());
    cmd.env("HOME", home.path());
    cmd.env("XDG_RUNTIME_DIR", runtime.path());
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

/// Asserts the `#?` fired, and fired after the command's output sentinel appeared.
fn assert_fired_after_output(output: &str) {
    let sentinel = output
        .find("COMPLETIONMARK")
        .unwrap_or_else(|| panic!("command output sentinel missing.\n--- output ---\n{output}"));
    let feedback = output.find("[koshell] #?").unwrap_or_else(|| {
        panic!("`#?` was never detected inside the REPL.\n--- output ---\n{output}")
    });
    assert!(
        feedback > sentinel,
        "`#?` fired before the command completed (feedback at {feedback}, output at \
         {sentinel}); it should be deferred until completion.\n--- output ---\n{output}"
    );
}

#[test]
fn python_repl_defers_hash_question_until_command_completes() {
    let Some(bash) = resolve("bash") else {
        eprintln!("skipping: no bash");
        return;
    };
    let Some(python) = resolve("python3") else {
        eprintln!("skipping python REPL test: no python3");
        return;
    };
    let python_dir = python.parent();

    let output = run_koshell_session(
        &bash,
        python_dir,
        &[
            (Duration::from_millis(900), b"python3 -q\n"),
            (
                Duration::from_millis(900),
                b"print('COMPLETION' + 'MARK')  #? did it run",
            ),
            (Duration::from_millis(400), b"\n"),
            (Duration::from_millis(900), b"exit()\n"),
            (Duration::from_millis(400), b"exit\n"),
        ],
    );

    assert_fired_after_output(&output);
}

#[test]
fn node_repl_defers_hash_question_via_quiescence() {
    let Some(bash) = resolve("bash") else {
        eprintln!("skipping: no bash");
        return;
    };
    let Some(node) = resolve("node") else {
        eprintln!("skipping node REPL test: no node");
        return;
    };
    let node_dir = node.parent();

    let output = run_koshell_session(
        &bash,
        node_dir,
        &[
            (Duration::from_millis(1200), b"node\n"),
            (
                Duration::from_millis(1000),
                b"console.log('COMPLETION' + 'MARK')  //#? did it run",
            ),
            (Duration::from_millis(400), b"\n"),
            (Duration::from_millis(900), b".exit\n"),
            (Duration::from_millis(400), b"exit\n"),
        ],
    );

    assert_fired_after_output(&output);
}

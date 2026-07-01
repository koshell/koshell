//! Real-PTY regression tests for `#?` detection through the bash/zsh shell-integration
//! hooks.
//!
//! The `#?` trigger relies on shell hooks (bash `PROMPT_COMMAND` + `DEBUG` trap; zsh
//! `preexec`/`precmd` + an `accept-line` widget) that only run in a real interactive shell
//! with a live line editor, so unit tests can't exercise them. These tests spawn the actual
//! `koshell` binary inside a PTY, drive an interactive shell, and assert that asking the
//! *same* `#?` question twice is detected both times.
//!
//! Regression guard: the hooks used to detect `#?` from shell history. bash deduped by
//! command *text* (fixed to dedup by history *number*); zsh's history-based detection was
//! defeated entirely by `hist_ignore_dups` (identical repeats collapse to one entry) and is
//! now driven by an `accept-line` widget that captures the submitted buffer directly.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

/// Upper bound on the whole session; a run normally finishes as soon as the shell exits.
const OVERALL_TIMEOUT: Duration = Duration::from_secs(20);

/// Locates a shell binary from a candidate list. Returns `None` (test skips) when none exist.
fn find_shell(candidates: &[&'static str]) -> Option<&'static str> {
    candidates
        .iter()
        .copied()
        .find(|candidate| Path::new(candidate).exists())
}

/// Spawns `koshell` in a PTY with the given `SHELL`, an isolated `HOME` (its `rc_name`
/// pre-populated with `rc_contents`), and no reachable AI daemon; drives `script` into the
/// interactive shell and returns everything printed back to the PTY.
fn drive_koshell(shell: &str, rc_name: &str, rc_contents: &str, script: &[u8]) -> String {
    // Isolated HOME: koshell's generated rc sources the user's rc from HOME, so writing one
    // here reproduces a specific shell configuration deterministically.
    let home = tempfile::tempdir().expect("create temp HOME");
    std::fs::write(home.path().join(rc_name), rc_contents).expect("write rc");
    // Empty XDG_RUNTIME_DIR => daemon socket absent => koshell prints its graceful-degrade
    // `[koshell] #?` feedback line, which is what we count.
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

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_koshell"));
    cmd.env_clear();
    cmd.env("SHELL", shell);
    cmd.env("HOME", home.path());
    cmd.env("XDG_RUNTIME_DIR", runtime.path());
    cmd.env("PATH", "/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin");
    cmd.env("TERM", "xterm-256color");
    cmd.env("HISTFILE", home.path().join(".shell_history"));

    let mut child = pair.slave.spawn_command(cmd).expect("spawn koshell");
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().expect("clone pty reader");
    let mut writer = pair.master.take_writer().expect("take pty writer");

    // Drain PTY output on a background thread until EOF (koshell exits when the shell does).
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

    // Writing the whole script at once is safe: the interactive shell reads and runs it line
    // by line through its line editor, firing the boundary hooks between lines.
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

/// Counts the `[koshell] #?` feedback lines koshell prints once per detected `#?`.
fn feedback_count(output: &str) -> usize {
    output.matches("[koshell] #?").count()
}

#[test]
fn bash_same_question_asked_twice_is_detected_both_times() {
    let Some(bash) = find_shell(&[
        "/opt/homebrew/bin/bash",
        "/usr/local/bin/bash",
        "/bin/bash",
        "/usr/bin/bash",
    ]) else {
        eprintln!("skipping bash test: no bash interpreter found");
        return;
    };

    // `#? ...` is a bash comment (interactive comments on by default), so it runs no command
    // and exercises the PROMPT_COMMAND fallback. Keep history dedup off so the repeat is a
    // distinct history entry.
    let output = drive_koshell(
        bash,
        ".bashrc",
        "HISTCONTROL=\n",
        b"#? explain this output\n#? explain this output\nexit\n",
    );

    let hits = feedback_count(&output);
    assert!(
        hits >= 2,
        "expected the repeated `#?` question to be detected twice under bash, saw {hits} \
         feedback line(s).\n--- captured PTY output ---\n{output}"
    );
}

#[test]
fn zsh_repeated_question_survives_hist_ignore_dups() {
    let Some(zsh) = find_shell(&["/bin/zsh", "/usr/bin/zsh", "/opt/homebrew/bin/zsh"]) else {
        eprintln!("skipping zsh test: no zsh interpreter found");
        return;
    };

    // Reproduce the reported failing configuration: interactive comments make `#? ...` a
    // comment (no command runs), and the history-dedup options collapse the two identical
    // questions to a single history entry. Detection must therefore not depend on history.
    let rc = "setopt interactive_comments\n\
              setopt hist_ignore_dups\n\
              setopt hist_save_no_dups\n\
              setopt hist_ignore_space\n\
              HISTSIZE=1000\n\
              SAVEHIST=1000\n";
    let output = drive_koshell(
        zsh,
        ".zshrc",
        rc,
        b"#? explain this output\n#? explain this output\nexit\n",
    );

    let hits = feedback_count(&output);
    assert!(
        hits >= 2,
        "expected the repeated `#?` question to be detected twice under zsh with \
         hist_ignore_dups, saw {hits} feedback line(s).\n--- captured PTY output ---\n{output}"
    );
}

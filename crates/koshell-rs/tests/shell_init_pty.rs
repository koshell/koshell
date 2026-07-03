//! Real-PTY end-to-end tests for the `koshell shell-init` auto-wrap snippet.
//!
//! The snippet's whole job happens during interactive shell startup: an rc file
//! `eval`s it and the shell `exec`s into koshell, which re-sources the same rc inside
//! its integration shell where the `KOSHELL=1` guard must skip the `exec`. That
//! exec-and-no-recursion loop only exists with a real TTY (the snippet checks
//! `-t 0 && -t 1`), so these tests spawn the actual shell in a PTY with the built
//! `koshell` binary on `PATH` and observe which shell ends up reading the terminal.

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

/// Spawns `shell` directly in a PTY with an isolated `HOME` whose `rc_name` installs the
/// shell-init snippet via `eval "$(koshell shell-init <dialect>)"`, the built `koshell`
/// binary on `PATH`, and `extra_env` applied last. Drives `script` into whichever shell
/// ends up at the terminal and returns everything printed back to the PTY.
fn drive_shell_init(
    shell: &str,
    rc_name: &str,
    dialect: &str,
    extra_env: &[(&str, &str)],
    script: &[u8],
) -> String {
    let home = tempfile::tempdir().expect("create temp HOME");
    let rc = format!("eval \"$(koshell shell-init {dialect})\"\n");
    std::fs::write(home.path().join(rc_name), rc).expect("write rc");
    // Empty XDG_RUNTIME_DIR => daemon socket absent; the wrap must work regardless.
    let runtime = tempfile::tempdir().expect("create temp XDG_RUNTIME_DIR");

    let koshell_bin = Path::new(env!("CARGO_BIN_EXE_koshell"));
    let bin_dir = koshell_bin.parent().expect("koshell binary directory");

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    let mut cmd = CommandBuilder::new(shell);
    cmd.env_clear();
    cmd.env("SHELL", shell);
    cmd.env("HOME", home.path());
    cmd.env("XDG_RUNTIME_DIR", runtime.path());
    cmd.env(
        "PATH",
        format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", bin_dir.display()),
    );
    cmd.env("TERM", "xterm-256color");
    cmd.env("HISTFILE", home.path().join(".shell_history"));
    for (key, value) in extra_env {
        cmd.env(key, value);
    }

    let mut child = pair.slave.spawn_command(cmd).expect("spawn shell");
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

    // The input sits in the PTY buffer until a shell reads it at a prompt: either the
    // koshell-wrapped inner shell (exec happened) or the outer shell (guard held).
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

/// Reports the wrap marker: `WRAP-STATE-1` when the koshell-wrapped shell answered
/// (it carries `KOSHELL=1`), `WRAP-STATE-none` when the original shell still owns the
/// terminal. The probe expands `${KOSHELL:-none}`, so the echoed input line never
/// contains either literal answer.
const PROBE: &[u8] = b"echo WRAP-STATE-${KOSHELL:-none}\nexit\n";

const BASH_CANDIDATES: [&str; 4] = [
    "/opt/homebrew/bin/bash",
    "/usr/local/bin/bash",
    "/bin/bash",
    "/usr/bin/bash",
];
const ZSH_CANDIDATES: [&str; 3] = ["/bin/zsh", "/usr/bin/zsh", "/opt/homebrew/bin/zsh"];

#[test]
fn bash_rc_snippet_execs_into_koshell_without_recursion() {
    let Some(bash) = find_shell(&BASH_CANDIDATES) else {
        eprintln!("skipping bash test: no bash interpreter found");
        return;
    };

    let output = drive_shell_init(bash, ".bashrc", "bash", &[], PROBE);
    assert!(
        output.contains("WRAP-STATE-1"),
        "expected the probe to run inside a koshell-wrapped shell (KOSHELL=1).\n\
         --- captured PTY output ---\n{output}"
    );
}

#[test]
fn zsh_rc_snippet_execs_into_koshell_without_recursion() {
    let Some(zsh) = find_shell(&ZSH_CANDIDATES) else {
        eprintln!("skipping zsh test: no zsh interpreter found");
        return;
    };

    let output = drive_shell_init(zsh, ".zshrc", "zsh", &[], PROBE);
    assert!(
        output.contains("WRAP-STATE-1"),
        "expected the probe to run inside a koshell-wrapped shell (KOSHELL=1).\n\
         --- captured PTY output ---\n{output}"
    );
}

#[test]
fn escape_hatch_env_keeps_the_original_shell() {
    let Some(zsh) = find_shell(&ZSH_CANDIDATES) else {
        eprintln!("skipping zsh test: no zsh interpreter found");
        return;
    };

    let output = drive_shell_init(zsh, ".zshrc", "zsh", &[("KOSHELL_NO_AUTO", "1")], PROBE);
    assert!(
        output.contains("WRAP-STATE-none") && !output.contains("WRAP-STATE-1"),
        "expected KOSHELL_NO_AUTO=1 to suppress the exec and leave the original shell.\n\
         --- captured PTY output ---\n{output}"
    );
}

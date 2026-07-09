//! Real-PTY end-to-end tests for the `koshell shell-init` auto-wrap snippet.
//!
//! The snippet's whole job happens during interactive shell startup: an rc file
//! `eval`s it and the shell `exec`s into koshell, which re-sources the same rc inside
//! its integration shell where the tty-scoped `KOSHELL` guard must skip the `exec`
//! (the inner shell's `$(tty)` equals the tty field branded into `KOSHELL`). That
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
    drive_shell_init_with_rc_prefix(shell, rc_name, dialect, "", extra_env, script)
}

/// Like [`drive_shell_init`] but prepends `rc_prefix` to the rc before the shell-init
/// `eval`. Used to simulate a shell that inherited a `KOSHELL` brand (and its conventional
/// liveness marker) naming its own tty (the recycled-pts case); guard such prefixes with
/// `[[ -z "$KOSHELL" ]]` so they run only in the outer, un-wrapped shell and do not clobber
/// koshell's fresh brand.
fn drive_shell_init_with_rc_prefix(
    shell: &str,
    rc_name: &str,
    dialect: &str,
    rc_prefix: &str,
    extra_env: &[(&str, &str)],
    script: &[u8],
) -> String {
    let home = tempfile::tempdir().expect("create temp HOME");
    let rc = format!("{rc_prefix}eval \"$(koshell shell-init {dialect})\"\n");
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
    // Disable daemon auto-spawn so the no-daemon scenarios stay hermetic (an
    // installed koshell-ai-daemon on PATH would otherwise be launched).
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

/// Reports the wrap marker: `WRAP-STATE-koshell-<pid>,<tty>` when the koshell-wrapped
/// shell answered (koshell brands `KOSHELL=<session-id>,<tty>`), `WRAP-STATE-none` when a
/// fresh original shell still owns the terminal. Tests that seed a sentinel `KOSHELL`
/// (`koshell-pane,...`) distinguish held-vs-wrapped by that sentinel's presence, since a
/// real wrap overwrites `KOSHELL` with its own session id. The probe expands
/// `${KOSHELL:-none}`, so the echoed input line never contains the answer.
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
        output.contains("WRAP-STATE-koshell-"),
        "expected the probe to run inside a koshell-wrapped shell (branded KOSHELL).\n\
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
        output.contains("WRAP-STATE-koshell-"),
        "expected the probe to run inside a koshell-wrapped shell (branded KOSHELL).\n\
         --- captured PTY output ---\n{output}"
    );
}

/// A tty path that can never equal a real controlling tty, standing in for the tty a tmux
/// pane inherits (branded into `KOSHELL`) from the koshell that started the server — a
/// *different* pts than the pane's own.
const FOREIGN_TTY: &str = "/dev/pts/koshell-pane-test-not-a-real-tty";

#[test]
fn foreign_koshell_tty_still_wraps_like_a_tmux_pane() {
    // A tmux pane inherits a KOSHELL whose tty field names another pts. Because it does not
    // match the pane's own `$(tty)`, the guard must not treat the pane as already-wrapped:
    // it wraps into its own koshell (which overwrites KOSHELL with its own value, so the
    // `${KOSHELL:-none}` probe cleanly distinguishes a real wrap from a held shell).
    let Some(zsh) = find_shell(&ZSH_CANDIDATES) else {
        eprintln!("skipping zsh test: no zsh interpreter found");
        return;
    };

    // Sentinel session id so a real wrap (which overwrites KOSHELL with its own session id)
    // is distinguishable from a held shell that keeps the inherited foreign brand.
    let inherited = format!("koshell-pane,{FOREIGN_TTY}");
    let output = drive_shell_init(
        zsh,
        ".zshrc",
        "zsh",
        &[("KOSHELL", inherited.as_str())],
        PROBE,
    );
    assert!(
        output.contains("WRAP-STATE-koshell-") && !output.contains(FOREIGN_TTY),
        "expected a foreign inherited KOSHELL tty field to still wrap the pane shell \
         (replacing the inherited brand).\n--- captured PTY output ---\n{output}"
    );
}

/// rc prefix that, in the outer (un-wrapped) shell only, brands `KOSHELL` with the shell's
/// *own* tty and writes the conventional liveness marker holding `pid` — simulating a shell
/// that inherited a brand for its own pts (a recycled-pts pane). It also points
/// `XDG_RUNTIME_DIR` at a writable temp dir so the marker lands where both the snippet and
/// `koshell preflight` look for it. The `[[ -z KOSHELL ]]` guard keeps it from clobbering
/// koshell's fresh brand in the wrapped inner shell.
fn self_brand_rc_prefix(pid: &str) -> String {
    format!(
        "if [[ -z \"${{KOSHELL-}}\" ]]; then \
           export XDG_RUNTIME_DIR=\"$HOME/xdgrt\"; \
           __t=\"$(tty)\"; \
           mkdir -p \"$XDG_RUNTIME_DIR/koshell/tty\"; \
           printf '%s' {pid} > \"$XDG_RUNTIME_DIR/koshell/tty/${{__t//\\//_}}\"; \
           export KOSHELL=\"koshell-pane,$__t\"; \
         fi\n"
    )
}

#[test]
fn stale_marker_on_matching_tty_still_wraps() {
    // KOSHELL's tty field equals the shell's own tty, but the liveness marker names a dead
    // pid (past pid_max) — the recycled-pts case. The guard must not treat this as wrapped.
    let Some(zsh) = find_shell(&ZSH_CANDIDATES) else {
        eprintln!("skipping zsh test: no zsh interpreter found");
        return;
    };

    let output = drive_shell_init_with_rc_prefix(
        zsh,
        ".zshrc",
        "zsh",
        &self_brand_rc_prefix("2147483646"),
        &[],
        PROBE,
    );
    assert!(
        output.contains("WRAP-STATE-koshell-") && !output.contains("koshell-pane"),
        "expected a stale (dead-pid) marker on a matching tty to still wrap \
         (replacing the sentinel brand).\n--- captured PTY output ---\n{output}"
    );
}

#[test]
fn live_marker_on_matching_tty_skips_wrap() {
    // KOSHELL's tty field equals the shell's own tty and the marker names a live pid ($$, the
    // shell itself) — a genuine already-wrapped tty. The guard must skip the exec.
    let Some(zsh) = find_shell(&ZSH_CANDIDATES) else {
        eprintln!("skipping zsh test: no zsh interpreter found");
        return;
    };

    let output = drive_shell_init_with_rc_prefix(
        zsh,
        ".zshrc",
        "zsh",
        &self_brand_rc_prefix("$$"),
        &[],
        PROBE,
    );
    assert!(
        output.contains("WRAP-STATE-koshell-pane"),
        "expected a live marker on a matching tty to skip the wrap, leaving the sentinel \
         brand in place.\n--- captured PTY output ---\n{output}"
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

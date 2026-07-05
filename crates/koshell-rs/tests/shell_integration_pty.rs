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

/// Spawns `koshell` in a PTY with the given `SHELL`, an isolated `HOME` pre-populated
/// with `files` (HOME-relative paths, parent directories created), and no reachable AI
/// daemon; drives `script` into the interactive shell and returns everything printed
/// back to the PTY.
fn drive_koshell(shell: &str, files: &[(&str, &str)], script: &[u8]) -> String {
    // Isolated HOME: koshell's generated rc sources the user's rc files from HOME (or a
    // custom ZDOTDIR under it), so writing them here reproduces a specific shell
    // configuration deterministically.
    let home = tempfile::tempdir().expect("create temp HOME");
    for (name, contents) in files {
        let path = home.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create rc parent dirs");
        }
        std::fs::write(path, contents).expect("write rc");
    }
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
    // Disable daemon auto-spawn so the no-daemon scenarios stay hermetic (an
    // installed koshell-ai-daemon on PATH would otherwise be launched).
    cmd.env("KOSHELL_NO_DAEMON_SPAWN", "1");
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

/// Reads a live process's working directory by pid, cross-platform. Returns `None` when
/// it cannot be determined (unsupported platform, missing `lsof`, or the process is gone),
/// in which case the caller skips rather than failing.
fn process_cwd(pid: u32) -> Option<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }
    #[cfg(target_os = "macos")]
    {
        // `-Fn` prints field-tagged records; the current-directory path is the line that
        // starts with the `n` (name) field tag.
        let output = std::process::Command::new("lsof")
            .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
            .output()
            .ok()?;
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.strip_prefix('n').map(std::path::PathBuf::from))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// Extracts the value printed after `tag` (e.g. `ZD:`). Terminal escape sequences may
/// share the line with the value (bracketed-paste toggles), so this matches the tag
/// anywhere in a line; the echoed `printf` command line itself is skipped because its
/// text after the tag starts with `%`.
fn tagged_value<'a>(output: &'a str, tag: &str) -> Option<&'a str> {
    output.lines().find_map(|line| {
        let idx = line.find(tag)?;
        let value = line[idx + tag.len()..].trim_end();
        (!value.starts_with('%')).then_some(value)
    })
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
        &[(".bashrc", "HISTCONTROL=\n")],
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
        &[(".zshrc", rc)],
        b"#? explain this output\n#? explain this output\nexit\n",
    );

    let hits = feedback_count(&output);
    assert!(
        hits >= 2,
        "expected the repeated `#?` question to be detected twice under zsh with \
         hist_ignore_dups, saw {hits} feedback line(s).\n--- captured PTY output ---\n{output}"
    );
}

#[test]
fn zsh_custom_zdotdir_config_is_loaded_and_restored() {
    let Some(zsh) = find_shell(&["/bin/zsh", "/usr/bin/zsh", "/opt/homebrew/bin/zsh"]) else {
        eprintln!("skipping zsh test: no zsh interpreter found");
        return;
    };

    // The common dotfiles layout: ~/.zshenv relocates ZDOTDIR and the real .zshrc lives
    // there. The integration must source that .zshrc (not $HOME/.zshrc), restore ZDOTDIR
    // to the user value for the session, and still detect `#?`.
    let output = drive_koshell(
        zsh,
        &[
            (".zshenv", "export ZDOTDIR=\"$HOME/cfg/zsh\"\n"),
            (
                "cfg/zsh/.zshrc",
                "setopt interactive_comments\necho USER_RC_LOADED\n",
            ),
        ],
        b"printf 'ZD:%s\\n' \"$ZDOTDIR\"\n#? explain this output\nexit\n",
    );

    assert!(
        output.contains("USER_RC_LOADED"),
        "the user's .zshrc under the custom ZDOTDIR was not sourced.\n\
         --- captured PTY output ---\n{output}"
    );
    let zdotdir_value = tagged_value(&output, "ZD:");
    assert!(
        zdotdir_value.is_some_and(|value| value.ends_with("cfg/zsh")),
        "session ZDOTDIR was not restored to the user value (saw {zdotdir_value:?}).\n\
         --- captured PTY output ---\n{output}"
    );
    let hits = feedback_count(&output);
    assert!(
        hits >= 1,
        "expected `#?` to be detected under a custom ZDOTDIR, saw {hits} feedback \
         line(s).\n--- captured PTY output ---\n{output}"
    );
}

#[test]
fn zsh_does_not_leak_integration_zdotdir_into_the_session() {
    let Some(zsh) = find_shell(&["/bin/zsh", "/usr/bin/zsh", "/opt/homebrew/bin/zsh"]) else {
        eprintln!("skipping zsh test: no zsh interpreter found");
        return;
    };

    let output = drive_koshell(
        zsh,
        &[(".zshrc", "setopt interactive_comments\n")],
        b"printf 'ZD:%s\\n' \"${ZDOTDIR:-empty}\"\n\
          printf 'KUZ:%s\\n' \"${KOSHELL_USER_ZDOTDIR:-unset}\"\n\
          exit\n",
    );

    // ZDOTDIR must end up as the user's real rc dir (HOME here), never koshell's temp
    // injection dir, so compdump/history/nested-zsh all land in the real location.
    let zdotdir_value = tagged_value(&output, "ZD:");
    assert!(
        zdotdir_value.is_some_and(|value| !value.contains("koshell-zsh")),
        "session ZDOTDIR leaked the temp injection dir (saw {zdotdir_value:?}).\n\
         --- captured PTY output ---\n{output}"
    );
    assert_eq!(
        tagged_value(&output, "KUZ:"),
        Some("unset"),
        "KOSHELL_USER_ZDOTDIR was not cleaned out of the session.\n\
         --- captured PTY output ---\n{output}"
    );
}

#[test]
fn zsh_cwd_mirrors_the_inner_shell_working_directory() {
    let Some(zsh) = find_shell(&["/bin/zsh", "/usr/bin/zsh", "/opt/homebrew/bin/zsh"]) else {
        eprintln!("skipping cwd test: no zsh interpreter found");
        return;
    };
    assert_cwd_mirrors(zsh, ".zshrc", "setopt interactive_comments\n");
}

#[test]
fn bash_cwd_mirrors_the_inner_shell_working_directory() {
    let Some(bash) = find_shell(&[
        "/opt/homebrew/bin/bash",
        "/usr/local/bin/bash",
        "/bin/bash",
        "/usr/bin/bash",
    ]) else {
        eprintln!("skipping cwd test: no bash interpreter found");
        return;
    };
    assert_cwd_mirrors(bash, ".bashrc", "HISTCONTROL=\n");
}

/// Drives an interactive shell to `cd` into a fresh directory and asserts koshell mirrors
/// it onto its own process cwd.
///
/// The reported tmux bug: after the inner shell `cd`s, koshell's own process cwd stayed
/// frozen at startup, so `pane_current_path` (which reads the pane process) split into the
/// wrong directory. The precmd cwd marker must move koshell's process cwd to match.
fn assert_cwd_mirrors(shell: &str, rc_name: &str, rc_contents: &str) {
    let home = tempfile::tempdir().expect("create temp HOME");
    std::fs::write(home.path().join(rc_name), rc_contents).expect("write rc");
    let runtime = tempfile::tempdir().expect("create temp XDG_RUNTIME_DIR");
    // A distinct destination directory, canonicalized because macOS resolves temp paths
    // through /private and lsof reports the resolved form.
    let target = tempfile::tempdir().expect("create target dir");
    let target_canonical = std::fs::canonicalize(target.path()).expect("canonicalize target");

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
    cmd.env("KOSHELL_NO_DAEMON_SPAWN", "1");
    cmd.env("SHELL", shell);
    cmd.env("HOME", home.path());
    cmd.env("XDG_RUNTIME_DIR", runtime.path());
    cmd.env("PATH", "/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin");
    cmd.env("TERM", "xterm-256color");
    cmd.env("HISTFILE", home.path().join(".shell_history"));

    let mut child = pair.slave.spawn_command(cmd).expect("spawn koshell");
    drop(pair.slave);
    let koshell_pid = child.process_id().expect("koshell pid");

    if process_cwd(koshell_pid).is_none() {
        eprintln!("skipping cwd test: cannot read a process cwd on this platform");
        let _ = child.kill();
        let _ = child.wait();
        return;
    }

    // Drain PTY output so the shell never blocks on a full pty buffer.
    let mut reader = pair.master.try_clone_reader().expect("clone pty reader");
    let drain = thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
        }
    });
    let mut writer = pair.master.take_writer().expect("take pty writer");

    let cd_line = format!("cd {}\n", target_canonical.display());
    writer.write_all(cd_line.as_bytes()).expect("write cd");
    writer.flush().expect("flush cd");

    // Poll koshell's process cwd until it mirrors the inner shell's `cd`, or time out.
    let deadline = Instant::now() + OVERALL_TIMEOUT;
    let mut mirrored = false;
    while Instant::now() < deadline {
        if process_cwd(koshell_pid).is_some_and(|cwd| cwd == target_canonical) {
            mirrored = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }

    let observed = process_cwd(koshell_pid);
    writer.write_all(b"exit\n").expect("write exit");
    writer.flush().expect("flush exit");
    drop(writer);
    let _ = child.kill();
    let _ = child.wait();
    let _ = drain.join();

    assert!(
        mirrored,
        "koshell did not mirror the inner shell's cwd: expected {target_canonical:?}, \
         observed {observed:?}"
    );
}

#[test]
fn bash_user_debug_trap_keeps_firing_alongside_koshell() {
    let Some(bash) = find_shell(&[
        "/opt/homebrew/bin/bash",
        "/usr/local/bin/bash",
        "/bin/bash",
        "/usr/bin/bash",
    ]) else {
        eprintln!("skipping bash test: no bash interpreter found");
        return;
    };

    // bash allows a single DEBUG trap; the integration must chain a trap installed by
    // the user rc (the bash-preexec / iTerm2 pattern) instead of clobbering it, while
    // `#?` detection keeps working.
    let rc = "HISTCONTROL=\n\
              __user_debug_hits=0\n\
              trap '__user_debug_hits=$((__user_debug_hits+1))' DEBUG\n";
    let output = drive_koshell(
        bash,
        &[(".bashrc", rc)],
        b"echo hi\nprintf 'USERHITS:%s\\n' \"$__user_debug_hits\"\n#? explain this\nexit\n",
    );

    let user_hits: Option<u64> =
        tagged_value(&output, "USERHITS:").and_then(|value| value.parse().ok());
    assert!(
        user_hits.is_some_and(|hits| hits >= 1),
        "the user's DEBUG trap stopped firing under koshell (saw {user_hits:?}).\n\
         --- captured PTY output ---\n{output}"
    );
    let hits = feedback_count(&output);
    assert!(
        hits >= 1,
        "expected `#?` to be detected with a chained DEBUG trap, saw {hits} feedback \
         line(s).\n--- captured PTY output ---\n{output}"
    );
}

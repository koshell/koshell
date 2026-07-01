//! Interactive terminal session: spawns the resolved shell in a PTY and forwards
//! stdin/stdout so koshell behaves as a transparent shell wrapper.
//!
//! Phase 1 scope: raw passthrough with resize and clean exit. The terminal mirror,
//! timeline, snapshots, and `#?` detection layer on in later phases.

use std::collections::HashMap;
use std::io::{IsTerminal, Read, Write};
use std::thread;

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use crate::shell;

/// Restores the terminal out of raw mode when dropped, including on panic.
struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Runs an interactive shell wrapped in a PTY. Returns the child's exit code.
pub fn run_interactive_shell() -> Result<i32> {
    let env: HashMap<String, String> = std::env::vars().collect();
    shell::assert_not_nested_koshell(&env)?;

    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        anyhow::bail!("koshell must be started from an interactive TTY.");
    }

    let shell_path = shell::resolve_shell(&env)?;
    let pty_env = shell::create_pty_env(&env);

    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new(&shell_path);
    if let Ok(cwd) = std::env::current_dir() {
        cmd.cwd(cwd);
    }
    cmd.env_clear();
    for (key, value) in &pty_env {
        cmd.env(key, value);
    }

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .with_context(|| format!("failed to spawn shell {shell_path:?}"))?;
    // The slave handle is only needed to spawn the child; releasing it lets the PTY
    // close cleanly (and the reader see EOF) once the child exits.
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;
    let master = pair.master;

    crossterm::terminal::enable_raw_mode()?;
    let _raw_guard = RawModeGuard;

    // PTY output -> real stdout.
    let reader_handle = thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut stdout = std::io::stdout();
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if stdout.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    let _ = stdout.flush();
                }
            }
        }
    });

    // Real stdin -> PTY input. Detached: it ends when the process exits.
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut stdin = std::io::stdin();
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if writer.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    let _ = writer.flush();
                }
            }
        }
    });

    // Propagate terminal resizes to the PTY.
    let mut signals = signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH])?;
    thread::spawn(move || {
        for _ in signals.forever() {
            if let Ok((c, r)) = crossterm::terminal::size() {
                let _ = master.resize(PtySize {
                    rows: r,
                    cols: c,
                    pixel_width: 0,
                    pixel_height: 0,
                });
            }
        }
    });

    let status = child.wait()?;
    // Drain any remaining PTY output before restoring the terminal.
    let _ = reader_handle.join();

    Ok(status.exit_code() as i32)
}

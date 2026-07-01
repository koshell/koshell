//! Interactive terminal session: spawns the shell (with shell integration) in a PTY and
//! forwards stdin/stdout while recording terminal facts and detecting `#?`.
//!
//! Threads communicate through a channel with a single processor thread owning the mirror
//! and timeline, so the terminal-emulator state is never shared across threads:
//! - reader thread: PTY output -> channel
//! - stdin thread: keystrokes -> PTY and -> channel (for recording)
//! - resize thread: SIGWINCH -> PTY resize and -> channel
//! - processor thread: applies events, writes visible output to stdout, handles `#?`

use std::collections::HashMap;
use std::io::{IsTerminal, Read, Write};
use std::sync::mpsc;
use std::thread;

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use crate::shell;
use crate::shell_integration::{MarkerScanner, Segment, create_shell_launch_config};
use crate::trigger::{SessionState, Trigger};

const DEFAULT_COLUMNS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

/// Messages from the I/O threads to the single processor thread.
enum Msg {
    Pty(Vec<u8>),
    Input(Vec<u8>),
    Resize(u16, u16),
    Exit,
}

/// Clamps a terminal size to at least 1x1, substituting defaults for a zero dimension.
/// A PTY without a configured window size reports 0x0, which would build a zero-row
/// terminal emulator and panic on snapshot.
fn sane_size((columns, rows): (u16, u16)) -> (u16, u16) {
    let columns = if columns == 0 {
        DEFAULT_COLUMNS
    } else {
        columns
    };
    let rows = if rows == 0 { DEFAULT_ROWS } else { rows };
    (columns, rows)
}

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
    let launch = create_shell_launch_config(&shell_path, pty_env)?;

    let (cols, rows) =
        sane_size(crossterm::terminal::size().unwrap_or((DEFAULT_COLUMNS, DEFAULT_ROWS)));
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new(&launch.command);
    if let Ok(cwd) = std::env::current_dir() {
        cmd.cwd(cwd);
    }
    cmd.env_clear();
    for (key, value) in &launch.env {
        cmd.env(key, value);
    }
    for arg in &launch.args {
        cmd.arg(arg);
    }

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .with_context(|| format!("failed to spawn shell {:?}", launch.command))?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;
    let master = pair.master;

    crossterm::terminal::enable_raw_mode()?;
    let _raw_guard = RawModeGuard;

    let (tx, rx) = mpsc::channel::<Msg>();

    // Processor thread owns the terminal-core state.
    let processor = thread::spawn(move || {
        let mut state = SessionState::new(cols, rows);
        let mut scanner = MarkerScanner::new();
        let mut stdout = std::io::stdout();
        while let Ok(msg) = rx.recv() {
            match msg {
                Msg::Pty(bytes) => {
                    for segment in scanner.feed(&bytes) {
                        match segment {
                            Segment::Visible(visible) => {
                                let _ = stdout.write_all(&visible);
                                let _ = stdout.flush();
                                state.record_output(&visible);
                            }
                            Segment::Marker(marker) => {
                                if let Some(trigger) = state.handle_marker(marker) {
                                    emit_trigger_placeholder(&mut stdout, &trigger);
                                }
                            }
                        }
                    }
                }
                Msg::Input(bytes) => state.record_input(&bytes),
                Msg::Resize(columns, lines) => state.resize(columns, lines),
                Msg::Exit => break,
            }
        }
    });

    // PTY output -> channel.
    let tx_reader = tx.clone();
    let reader_handle = thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx_reader.send(Msg::Pty(buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = tx_reader.send(Msg::Exit);
    });

    // stdin -> PTY (low latency) and -> channel (for recording).
    let tx_input = tx.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut stdin = std::io::stdin();
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let _ = tx_input.send(Msg::Input(buf[..n].to_vec()));
                    if writer.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    let _ = writer.flush();
                }
            }
        }
    });

    // SIGWINCH -> PTY resize and -> channel.
    let tx_resize = tx.clone();
    let mut signals = signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH])?;
    thread::spawn(move || {
        for _ in signals.forever() {
            if let Ok(size) = crossterm::terminal::size() {
                let (columns, lines) = sane_size(size);
                let _ = master.resize(PtySize {
                    rows: lines,
                    cols: columns,
                    pixel_width: 0,
                    pixel_height: 0,
                });
                let _ = tx_resize.send(Msg::Resize(columns, lines));
            }
        }
    });

    drop(tx);

    let status = child.wait()?;
    let _ = reader_handle.join();
    let _ = processor.join();

    Ok(status.exit_code() as i32)
}

/// Phase 3 placeholder: acknowledge a `#?` inline. Replaced by streamed AI output once the
/// daemon and pi are connected.
fn emit_trigger_placeholder(stdout: &mut std::io::Stdout, trigger: &Trigger) {
    let _ = write!(
        stdout,
        "\r\n[koshell] #? received (AI not connected): {}\r\n",
        trigger.question
    );
    let _ = stdout.flush();
}

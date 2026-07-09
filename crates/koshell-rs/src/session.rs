//! Interactive terminal session: spawns the shell (with shell integration) in a PTY and
//! forwards stdin/stdout while recording terminal facts and detecting `#?`.
//!
//! Threads communicate through a channel with a single processor thread owning the mirror
//! and timeline, so the terminal-emulator state is never shared across threads:
//! - reader thread: PTY output -> channel
//! - stdin thread: keystrokes -> PTY and -> channel (for recording); also owns the
//!   Ctrl+C interrupt swallow (while an AI response streams onto an idle prompt),
//!   since it sits on the raw input stream
//! - resize thread: SIGWINCH -> PTY resize and -> channel
//! - processor thread: applies events, writes visible output to stdout, handles `#?`
//!   (pending-question deadlines drive the channel receive timeout)

use std::collections::HashMap;
use std::io::{IsTerminal, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Instant;

use anyhow::{Context, Result};
use koshell_proto::{ClientMessage, ServerMessage};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use crate::daemon_spawn::{CONNECT_RETRY_BUDGET, CONNECT_RETRY_STEP, DaemonSpawner};
use crate::event_log::{self, Event, EventLog};
use crate::ipc::{self, IpcClient};
use crate::presentation::Presentation;
use crate::shell;
use crate::shell_integration::{MarkerKind, MarkerScanner, Segment, create_shell_launch_config};
use crate::trigger::{Action, CompletionKind, SessionState, Trigger};

/// Immutable per-session metadata used for the IPC handshake.
struct SessionMeta {
    cwd: String,
    shell: String,
    cols: u16,
    rows: u16,
}

const DEFAULT_COLUMNS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

/// Messages from the I/O threads to the single processor thread.
enum Msg {
    Pty(Vec<u8>),
    Input(Vec<u8>),
    Resize(u16, u16),
    /// A reply from the AI daemon, forwarded by the IPC reader thread.
    Daemon(ServerMessage),
    /// A Ctrl+C was swallowed while the interrupt window was armed (an AI
    /// response streaming onto an idle prompt).
    Interrupt,
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

/// Reads the terminal's pixel dimensions (`ws_xpixel`/`ws_ypixel`) from stdout so the
/// inner PTY advertises the same geometry that pixel-addressed image protocols (sixel,
/// kitty graphics) size against. Returns `(0, 0)` when the terminal reports no pixel
/// size — the same "unknown" value a bare shell would see, so nothing regresses.
fn terminal_pixel_size() -> (u16, u16) {
    use std::os::unix::io::AsRawFd;
    // Safety: `ws` is a valid, owned `winsize` for the duration of the ioctl, and the
    // stdout fd is valid while this process holds it.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let fd = std::io::stdout().as_raw_fd();
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    if rc == 0 {
        (ws.ws_xpixel, ws.ws_ypixel)
    } else {
        (0, 0)
    }
}

/// Restores the terminal out of raw mode when dropped, including on panic.
struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Cheap, TTY-free readiness probe backing `koshell preflight`. The auto-wrap snippet
/// runs it as `koshell preflight && exec koshell`, on the *outer* shell — the safe side
/// of the `exec`. It confirms the two preconditions a fail-open cannot recover from on
/// its own: that the binary loads and runs at all (implied by reaching this code, so a
/// binary too broken to start makes `koshell preflight` itself fail and the snippet keep
/// the current shell), and that a real shell is resolvable so any later fail-open has
/// somewhere to land. Returns a process exit code: `0` ready, non-zero not.
pub fn preflight() -> i32 {
    let env: HashMap<String, String> = std::env::vars().collect();
    // Already inside a live koshell on this same terminal: the snippet guards this too, but
    // never green-light a nested exec (which `run_interactive_shell` would reject anyway). A
    // marker inherited across a tty boundary (a new tmux pane), or onto a recycled pts whose
    // koshell has died, is not nested, so preflight still passes there and the pane wraps.
    let marker_live = shell::tty_marker_is_live(
        env.get(shell::KOSHELL_TTY_MARKER_ENV_KEY)
            .map(String::as_str),
    );
    if shell::is_nested_koshell(&env, shell::controlling_tty().as_deref(), marker_live) {
        return 1;
    }
    match shell::resolve_shell(&env) {
        Ok(_) => 0,
        Err(error) => {
            log::warn!("koshell preflight: no shell resolvable: {error}");
            1
        }
    }
}

/// Fails open to the user's real shell after a startup failure under the `exec koshell`
/// auto-wrap. Replacing koshell's process image with a bare shell is strictly safer than
/// exiting: an `exec`-ed koshell that dies would otherwise close the terminal, and on a
/// Linux login TTY that can be the user's only way in. `KOSHELL_NO_AUTO=1` stops the
/// fresh shell's rc snippet from re-`exec`-ing koshell, which would loop the crash. This
/// returns only if no shell could be resolved or the `exec` itself failed; the caller
/// then exits non-zero.
pub fn exec_fallback_shell() {
    use std::os::unix::process::CommandExt;
    let env: HashMap<String, String> = std::env::vars().collect();
    let Ok(shell) = shell::resolve_shell(&env) else {
        log::error!("koshell fail-open: no shell resolvable, cannot recover");
        return;
    };
    // `exec` never returns on success; a returned error falls through to the caller.
    let error = std::process::Command::new(&shell)
        .env("KOSHELL_NO_AUTO", "1")
        .exec();
    log::error!("koshell fail-open exec of {shell} failed: {error}");
}

/// Runs an interactive session wrapped in a PTY and returns the child's exit code.
/// With an empty `command`, launches the default shell (with integration for
/// bash/zsh); otherwise launches `command[0]` with the remaining elements as its
/// arguments — explicit bash/zsh still gets integration, anything else runs directly
/// and `#?` uses the non-integrated capture path.
pub fn run_interactive_shell(command: &[String]) -> Result<i32> {
    let env: HashMap<String, String> = std::env::vars().collect();
    shell::assert_not_nested_koshell(&env, shell::controlling_tty().as_deref())?;

    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        anyhow::bail!("koshell must be started from an interactive TTY.");
    }

    let (shell_path, extra_args) = match command.split_first() {
        Some((program, args)) => (shell::resolve_command(program, &env)?, args),
        None => (shell::resolve_shell(&env)?, &[] as &[String]),
    };
    let pty_env = shell::create_pty_env(&env);
    let mut launch = create_shell_launch_config(&shell_path, extra_args, pty_env)?;

    let (cols, rows) =
        sane_size(crossterm::terminal::size().unwrap_or((DEFAULT_COLUMNS, DEFAULT_ROWS)));
    let (pixel_width, pixel_height) = terminal_pixel_size();
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows,
        cols,
        pixel_width,
        pixel_height,
    })?;

    // Brand the child shell with the tty it will run on, plus a PID liveness marker, so a
    // descendant that inherits this env across a tty boundary (a new tmux pane / fresh pts)
    // re-wraps, and a descendant on a *recycled* pts whose original koshell has died is not
    // fooled by the stale brand. Held for the session; dropping it removes the marker file.
    // See `shell::is_nested_koshell`.
    // Export the instance's session id (the same `koshell-<pid>` sent in hello)
    // so child `koshell status`/`reload` can address this instance's daemon
    // connection. Unconditional — unlike the tty brand, it does not depend on
    // the liveness marker, since it is only an address, not a nesting guard.
    launch
        .env
        .insert(ipc::SESSION_ID_ENV.to_string(), ipc::session_id());

    let _tty_marker = pair.master.tty_name().and_then(|tty| {
        let tty = tty.to_string_lossy().into_owned();
        // Brand with KOSHELL_TTY only when the liveness marker is also written: without the
        // marker a descendant cannot tell a live wrap from a stale one, and branding anyway
        // would make even the genuine inner shell re-wrap (and could recurse). Leaving
        // KOSHELL_TTY unset falls back to the flat KOSHELL=1 guard, which is safe.
        let marker = shell::register_tty_marker(&tty)?;
        launch.env.insert(
            shell::KOSHELL_TTY_MARKER_ENV_KEY.to_string(),
            marker.path().to_string_lossy().into_owned(),
        );
        launch
            .env
            .insert(shell::KOSHELL_TTY_ENV_KEY.to_string(), tty);
        Some(marker)
    });

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
    // The inner shell's pid, for forwarding termination signals to it (see the signal
    // thread below). The shell is a session leader on its own controlling PTY.
    let child_pid = child.process_id();

    let mut reader = pair.master.try_clone_reader()?;
    // Shared with the processor thread, which forwards a swallowed Esc when the cancel
    // race is lost (the pending question fired just before the keypress).
    let writer: Arc<Mutex<Box<dyn Write + Send>>> =
        Arc::new(Mutex::new(pair.master.take_writer()?));
    let master = pair.master;

    crossterm::terminal::enable_raw_mode()?;
    let _raw_guard = RawModeGuard;

    let (tx, rx) = mpsc::channel::<Msg>();

    let meta = SessionMeta {
        cwd: std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        shell: shell_path,
        cols,
        rows,
    };
    let socket_path = ipc::default_socket_path();
    // With shell integration active, the marker layer owns `#?` at the shell prompt and
    // submit-time mirror capture is confined to command spans (see `trigger.rs`).
    let shell_integrated = launch.kind.is_some();

    // Dogfooding event log (design 0007): local JSONL, fail-silent, inert when
    // disabled. The writer is joined after the child is reaped so the final
    // `session_end` is drained before the process exits.
    let (event_log, event_log_writer) = event_log::open();
    let session_started = Instant::now();
    event_log.emit(Event::SessionStart {
        shell: meta.shell.clone(),
        integrated: shell_integrated,
        cols,
        rows,
        version: env!("CARGO_PKG_VERSION"),
    });

    // Whether a Ctrl+C currently belongs to the AI response instead of the child: a
    // stream-mode response is in flight (the triggering command already ended, so no
    // foreground program is waiting for the interrupt) and the alternate screen is
    // inactive. While armed, Ctrl+C is swallowed and aborts the response — forwarding
    // it would discard whatever the user typed ahead on the live input line. With a
    // command still running (block mode) the window stays dark and Ctrl+C passes
    // through to the program as always.
    let interrupt_window = Arc::new(AtomicBool::new(false));

    // Processor thread owns the terminal-core state and the (lazy) IPC connection.
    // It keeps a channel sender so daemon replies (read by a dedicated IPC reader
    // thread) flow through the same single-consumer loop as PTY output.
    let interrupt_window_proc = interrupt_window.clone();
    let writer_proc = writer.clone();
    let tx_daemon = tx.clone();
    let event_log_proc = event_log.clone();
    let processor = thread::spawn(move || {
        let mut state = SessionState::new(cols, rows, shell_integrated);
        state.set_event_log(event_log_proc.clone());
        let mut scanner = MarkerScanner::new();
        let mut stdout = std::io::stdout();
        let mut ipc_client: Option<IpcClient> = None;
        let mut spawner = DaemonSpawner::new();
        let mut presentation = Presentation::new();
        presentation.set_event_log(event_log_proc.clone());
        let mut request_seq: u64 = 0;
        loop {
            // Trigger deadlines (receipt notice, stabilization tier, max-wait) and
            // presentation deadlines (waiting notice, buffered-output max hold) bound
            // the channel wait; with nothing pending, block until the next message.
            let wait_from = Instant::now();
            let timeout = [
                state.next_deadline(wait_from),
                presentation.next_deadline(wait_from),
            ]
            .into_iter()
            .flatten()
            .min();
            let msg = match timeout {
                Some(timeout) => match rx.recv_timeout(timeout) {
                    Ok(msg) => Some(msg),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                },
                None => match rx.recv() {
                    Ok(msg) => Some(msg),
                    Err(_) => break,
                },
            };

            let now = Instant::now();
            let mut actions: Vec<Action> = Vec::new();
            match msg {
                Some(Msg::Pty(bytes)) => {
                    for segment in scanner.feed(&bytes) {
                        match segment {
                            Segment::Visible(visible) => {
                                presentation.pty_output(&visible, &mut stdout, &mut state, now);
                            }
                            Segment::Marker(marker) if marker.kind == MarkerKind::Cwd => {
                                // Mirror the inner shell's working directory onto
                                // koshell's own process so external tooling that reads
                                // the pane process (tmux `pane_current_path`, OSC 7
                                // consumers) sees the real directory instead of the
                                // wrapper's startup cwd. koshell's own paths are all
                                // absolute (XDG runtime/cache, temp rc dirs), so moving
                                // the process cwd has no side effects; a stale directory
                                // (since removed) just fails the chdir and is ignored.
                                if let Some(dir) = &marker.cwd {
                                    let _ = std::env::set_current_dir(dir);
                                }
                            }
                            Segment::Marker(marker) => {
                                actions.extend(state.handle_marker(marker, now));
                            }
                        }
                    }
                }
                Some(Msg::Daemon(message)) => {
                    presentation.handle_server_message(&message, &mut stdout, &mut state, now);
                }
                Some(Msg::Input(bytes)) => {
                    // Mid-stream typing metric (design 0007): a chunk with any
                    // content beyond a bare Ctrl+C counts as typing while an
                    // answer streams; the bytes themselves are never logged.
                    if bytes.iter().any(|&byte| byte != 0x03) {
                        presentation.note_mid_stream_input();
                    }
                    actions.extend(state.record_input(&bytes, now));
                    // A forwarded Ctrl+C (command still running, or the swallow
                    // window was stale) also withdraws the in-flight response:
                    // pending and dispatched questions are cancelled alike, so
                    // withdrawal never depends on dispatch timing.
                    if bytes.contains(&0x03)
                        && !state.alt_screen()
                        && let Some(request_id) =
                            presentation.user_interrupt(&mut stdout, &mut state, now)
                    {
                        send_ai_cancel(&mut ipc_client, &request_id);
                    }
                }
                Some(Msg::Resize(columns, lines)) => state.resize(columns, lines),
                Some(Msg::Interrupt) => {
                    match presentation.user_interrupt(&mut stdout, &mut state, now) {
                        Some(request_id) => send_ai_cancel(&mut ipc_client, &request_id),
                        None => {
                            // The interrupt race was lost (the response finished
                            // first); restore transparency by forwarding the
                            // swallowed Ctrl+C and treating it as ordinary input.
                            if let Ok(mut writer) = writer_proc.lock() {
                                let _ = writer.write_all(b"\x03");
                                let _ = writer.flush();
                            }
                            actions.extend(state.record_input(&[0x03], now));
                        }
                    }
                }
                Some(Msg::Exit) => break,
                None => {}
            }
            actions.extend(state.poll(now));
            presentation.poll(now, &mut stdout, &mut state);

            for action in actions {
                match action {
                    Action::Notice(text) => present_notice(&mut stdout, &mut state, &text),
                    Action::Fire(trigger) => {
                        request_seq += 1;
                        dispatch_trigger(
                            &mut stdout,
                            &mut state,
                            &mut ipc_client,
                            &mut spawner,
                            &mut presentation,
                            &tx_daemon,
                            &socket_path,
                            &meta,
                            request_seq,
                            &trigger,
                            &event_log_proc,
                        );
                    }
                }
            }
            interrupt_window_proc.store(
                presentation.owns_interrupt() && !state.alt_screen(),
                Ordering::Relaxed,
            );
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

    // stdin -> PTY (low latency) and -> channel (for recording). While the interrupt
    // window is armed, a Ctrl+C is swallowed as an interrupt keypress and never
    // reaches the child; everything else is forwarded untouched.
    let tx_input = tx.clone();
    let writer_input = writer.clone();
    let interrupt_window_input = interrupt_window.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut stdin = std::io::stdin();
        let forward = |bytes: &[u8]| -> bool {
            if bytes.is_empty() {
                return true;
            }
            let _ = tx_input.send(Msg::Input(bytes.to_vec()));
            let Ok(mut writer) = writer_input.lock() else {
                return false;
            };
            if writer.write_all(bytes).is_err() {
                return false;
            }
            let _ = writer.flush();
            true
        };
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut chunk = &buf[..n];
                    // While the interrupt window is armed, the first Ctrl+C in the
                    // chunk is swallowed: it aborts the streaming AI response and
                    // never reaches the child (whose line editor would discard the
                    // input typed ahead on the live line).
                    if interrupt_window_input.load(Ordering::Relaxed)
                        && let Some(pos) = chunk.iter().position(|&byte| byte == 0x03)
                    {
                        if !forward(&chunk[..pos]) {
                            break;
                        }
                        let _ = tx_input.send(Msg::Interrupt);
                        chunk = &chunk[pos + 1..];
                        if chunk.is_empty() {
                            continue;
                        }
                    }
                    if !forward(chunk) {
                        break;
                    }
                }
            }
        }
    });

    // SIGWINCH -> PTY resize and -> channel; SIGHUP/SIGTERM/SIGINT -> forward to the
    // inner shell. Without forwarding, koshell would die by its own default disposition
    // and the shell would only ever see a hang-up from the master closing; forwarding
    // the original signal lets the shell run its real TERM/INT/HUP traps, as if it owned
    // the TTY directly.
    let tx_resize = tx.clone();
    let mut signals = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGWINCH,
        signal_hook::consts::SIGHUP,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
    ])?;
    thread::spawn(move || {
        for signo in signals.forever() {
            if signo == signal_hook::consts::SIGWINCH {
                if let Ok(size) = crossterm::terminal::size() {
                    let (columns, lines) = sane_size(size);
                    let (pixel_width, pixel_height) = terminal_pixel_size();
                    let _ = master.resize(PtySize {
                        rows: lines,
                        cols: columns,
                        pixel_width,
                        pixel_height,
                    });
                    let _ = tx_resize.send(Msg::Resize(columns, lines));
                }
            } else if let Some(pid) = child_pid {
                // The shell's exit then closes the PTY, the reader hits EOF, and the main
                // thread's `child.wait()` reaps it; a shell that traps and survives keeps
                // koshell alive with it, which is the transparent outcome. A failed kill
                // (already reaped) is ignored.
                unsafe {
                    libc::kill(pid as libc::pid_t, signo);
                }
            }
        }
    });

    drop(tx);

    let exit_code = reap_child(&mut *child, child_pid);
    let _ = reader_handle.join();
    let _ = processor.join();

    event_log.emit(Event::SessionEnd {
        exit_code,
        duration_ms: session_started.elapsed().as_millis() as u64,
    });
    // The processor's clones died with its join; dropping the last handle
    // disconnects the channel so the writer drains and exits.
    drop(event_log);
    if let Some(writer) = event_log_writer {
        writer.join();
    }

    Ok(exit_code)
}

/// Reaps the inner shell and computes its exit code with faithful signal semantics: a
/// process killed by signal N reports `128 + N`, the same value a shell reports in `$?`
/// for its own children and the value a parent shell would see for koshell had koshell
/// been killed directly. `portable_pty`'s `ExitStatus` collapses every signal death to
/// code 1 (it renders the numeric signal to a localized string and drops the number), so
/// we reap the pid ourselves via `waitpid` and read `WIFSIGNALED`/`WTERMSIG` directly.
///
/// This direct `waitpid` is the only reaper and cannot double-reap: the concrete Unix
/// `Child` behind portable-pty is a `std::process::Child` (its `Drop` does not reap) and
/// portable-pty installs no `SIGCHLD` handler in the parent. The wait targets the shell's
/// specific pid, so it never reaps the AI daemon or any other child. When the pid is
/// unknown (the API is fallible, though Unix always yields one for a spawned child) or the
/// `waitpid` fails, we fall back to portable-pty's own wait, which returns immediately if
/// the child was already reaped.
fn reap_child(child: &mut (dyn portable_pty::Child + Send + Sync), child_pid: Option<u32>) -> i32 {
    if let Some(pid) = child_pid {
        let pid = pid as libc::pid_t;
        let mut status: libc::c_int = 0;
        loop {
            // Safety: `status` is a valid, owned out-param for the duration of the call.
            let rc = unsafe { libc::waitpid(pid, &mut status, 0) };
            if rc == -1 {
                // A forwarded signal (e.g. SIGWINCH on resize) can interrupt the wait;
                // retry. Any other error falls through to portable-pty's wait.
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                break;
            }
            if libc::WIFSIGNALED(status) {
                return 128 + libc::WTERMSIG(status);
            }
            if libc::WIFEXITED(status) {
                return libc::WEXITSTATUS(status);
            }
            // Stopped/continued (untraced without WUNTRACED shouldn't occur): keep waiting
            // for the terminal state.
        }
    }
    child
        .wait()
        .map(|status| status.exit_code() as i32)
        .unwrap_or(1)
}

/// Prints a dim one-line presentation notice and feeds it to the mirror, keeping screen
/// snapshots truthful to what the user sees (the mirror-feed invariant). A rendered
/// prompt under the cursor stays the last line (the notice is inserted above it).
fn present_notice(stdout: &mut std::io::Stdout, state: &mut SessionState, text: &str) {
    crate::presentation::notice_before_prompt(text, stdout, state);
}

/// Sends a best-effort `ai_cancel` for a locally aborted request. The local
/// rendering stop is authoritative; this only asks the daemon to stop generating
/// and to unblock its queue, so a send failure just lets the response run out
/// server-side (its output is suppressed either way).
fn send_ai_cancel(ipc_client: &mut Option<IpcClient>, request_id: &str) {
    log::info!("#? [{request_id}] interrupted by the user (^C)");
    if let Some(client) = ipc_client.as_mut() {
        let cancel = ClientMessage::AiCancel {
            request_id: request_id.to_string(),
        };
        if client.send(&cancel).is_err() {
            *ipc_client = None;
        }
    }
}

/// Sends the `hello` handshake and spawns a dedicated reader thread that forwards
/// streamed daemon replies into the processor channel; it exits on EOF or a broken
/// socket. Returns the connected client, or `None` if the read half could not be
/// cloned.
fn attach_daemon(
    mut client: IpcClient,
    meta: &SessionMeta,
    tx: &mpsc::Sender<Msg>,
) -> Option<IpcClient> {
    let _ = client.send(&ipc::hello(
        meta.cwd.clone(),
        meta.shell.clone(),
        meta.rows,
        meta.cols,
    ));
    let Ok(mut reader) = client.reader() else {
        return None;
    };
    let tx_reader = tx.clone();
    thread::spawn(move || {
        while let Ok(Some(message)) = reader.recv() {
            if tx_reader.send(Msg::Daemon(message)).is_err() {
                break;
            }
        }
    });
    Some(client)
}

/// Sends a `#?` request to the AI daemon (connecting lazily), and acknowledges it inline.
/// If no daemon is reachable the terminal auto-spawns one (design 0008) and retries the
/// connect briefly; if it still cannot reach a daemon the terminal degrades gracefully.
#[allow(clippy::too_many_arguments)]
fn dispatch_trigger(
    stdout: &mut std::io::Stdout,
    state: &mut SessionState,
    ipc_client: &mut Option<IpcClient>,
    spawner: &mut DaemonSpawner,
    presentation: &mut Presentation,
    tx: &mpsc::Sender<Msg>,
    socket_path: &std::path::PathBuf,
    meta: &SessionMeta,
    request_seq: u64,
    trigger: &Trigger,
    event_log: &EventLog,
) {
    if ipc_client.is_none() {
        if let Ok(client) = IpcClient::connect(socket_path) {
            *ipc_client = attach_daemon(client, meta, tx);
        } else if spawner.try_spawn(Instant::now()) {
            // The daemon is connectable in ~200ms; retry within a bounded budget.
            // The PTY reader keeps draining into the channel, so this only delays
            // rendering — at the moment the user is already waiting on `#?`.
            let deadline = Instant::now() + CONNECT_RETRY_BUDGET;
            loop {
                thread::sleep(CONNECT_RETRY_STEP);
                if let Ok(client) = IpcClient::connect(socket_path) {
                    *ipc_client = attach_daemon(client, meta, tx);
                    break;
                }
                if Instant::now() >= deadline {
                    break;
                }
            }
        }
    }

    let request_id = format!("koshell-req-{request_seq}");
    let sent = if let Some(client) = ipc_client.as_mut() {
        let request = ClientMessage::AiRequest {
            request_id: request_id.clone(),
            question: trigger.question.clone(),
            trigger: "#?".to_string(),
            context_package: trigger.context_package.clone(),
        };
        match client.send(&request) {
            Ok(()) => true,
            Err(_) => {
                *ipc_client = None;
                false
            }
        }
    } else {
        false
    };
    let now = Instant::now();
    if sent {
        event_log.emit(Event::Dispatched {
            request_id: request_id.clone(),
            question: trigger.question.clone(),
            fire_reason: trigger.completion.as_str(),
            still_running: trigger.still_running,
            submit_to_dispatch_ms: now
                .saturating_duration_since(trigger.submitted_at)
                .as_millis() as u64,
        });
        // The streamed response (or the delayed waiting notice) is the user-facing
        // receipt; the dispatch itself is only worth a log line.
        presentation.note_dispatch(&request_id, trigger.still_running, state, now);
        log::info!(
            "#? [{request_id}] dispatched (completion: {:?}, still running: {}): {}",
            trigger.completion,
            trigger.still_running,
            trigger.question
        );
    } else {
        event_log.emit(Event::DispatchFailed {
            request_id: request_id.clone(),
            question: trigger.question.clone(),
            fire_reason: trigger.completion.as_str(),
            reason: "daemon_unavailable",
        });
        // Explicit degradation stays on the terminal: the question will get no
        // answer, and the user must know why (and in what completion state it fired).
        log::warn!(
            "#? [{request_id}] AI daemon unavailable: {}",
            trigger.question
        );
        let annotation = match (trigger.completion, trigger.still_running) {
            (CompletionKind::Stabilized | CompletionKind::MaxWait, true) => {
                " (command may still be running)"
            }
            (CompletionKind::MaxWait, false) => " (output not settled)",
            _ => "",
        };
        crate::presentation::notice_before_prompt(
            &format!(
                "#? received (AI daemon unavailable){annotation}: {}",
                trigger.question
            ),
            stdout,
            state,
        );
    }
}

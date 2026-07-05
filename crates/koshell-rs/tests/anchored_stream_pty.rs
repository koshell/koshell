//! Real-PTY end-to-end test for anchored streaming (design 0005): a fake AI daemon
//! streams a response to a `#?` asked inside a python REPL. The answer must land in
//! the free zone above the live prompt, and input typed mid-stream must stay on the
//! live line — the terminal remains fully usable while the AI speaks.
//!
//! The raw PTY byte stream contains the redraw choreography (erase, cursor-up,
//! column moves), so assertions replay the whole stream through the same terminal
//! emulator koshell uses for its mirror and check the final rendered screen.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use koshell_rs::mirror::TerminalMirror;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const OVERALL_TIMEOUT: Duration = Duration::from_secs(25);

/// Resolves an executable against the ambient `PATH`, so version-managed interpreters
/// (fnm, pyenv, …) that live outside the standard directories are still found.
fn resolve(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// Binds a fake AI daemon on `socket` that answers every `ai_request` with an ack,
/// two spaced-out deltas ("Hello", " world"), and a response end — enough to drive
/// koshell's anchored streaming across multiple redraws.
fn spawn_fake_daemon(socket: PathBuf) {
    let listener = UnixListener::bind(&socket).expect("bind fake daemon socket");
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            thread::spawn(move || {
                let mut writer = stream.try_clone().expect("clone daemon stream");
                let reader = BufReader::new(stream);
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                        continue;
                    };
                    if value["type"] != "ai_request" {
                        continue;
                    }
                    let id = value["request_id"].as_str().unwrap_or_default().to_string();
                    let mut send = |json: serde_json::Value| {
                        let _ = writer.write_all(json.to_string().as_bytes());
                        let _ = writer.write_all(b"\n");
                        let _ = writer.flush();
                    };
                    send(serde_json::json!({"type": "ack", "request_id": id}));
                    thread::sleep(Duration::from_millis(200));
                    send(serde_json::json!({
                        "type": "ai_delta", "request_id": id, "delta": "Hello"
                    }));
                    thread::sleep(Duration::from_millis(500));
                    send(serde_json::json!({
                        "type": "ai_delta", "request_id": id, "delta": " world"
                    }));
                    thread::sleep(Duration::from_millis(200));
                    send(serde_json::json!({"type": "ai_response_end", "request_id": id}));
                }
            });
        }
    });
}

/// Binds a fake daemon that streams a long answer ("chunk-0 ", "chunk-1 ", …, one
/// every 150 ms, up to 40) and honours `ai_cancel`: it stops streaming, terminates
/// the request with `ai_response_end`, and sets the returned flag — enough to prove
/// the terminal-side interrupt reaches the daemon and the tail never renders.
fn spawn_cancellable_daemon(socket: PathBuf) -> Arc<AtomicBool> {
    let cancel_received = Arc::new(AtomicBool::new(false));
    let flag = cancel_received.clone();
    let listener = UnixListener::bind(&socket).expect("bind fake daemon socket");
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let flag = flag.clone();
            thread::spawn(move || {
                let mut writer = stream.try_clone().expect("clone daemon stream");
                let reader = BufReader::new(stream);
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                        continue;
                    };
                    if value["type"] == "ai_cancel" {
                        flag.store(true, Ordering::SeqCst);
                        continue;
                    }
                    if value["type"] != "ai_request" {
                        continue;
                    }
                    let id = value["request_id"].as_str().unwrap_or_default().to_string();
                    let mut stream_writer = writer.try_clone().expect("clone for streamer");
                    let flag = flag.clone();
                    let mut send = |json: serde_json::Value| {
                        let _ = writer.write_all(json.to_string().as_bytes());
                        let _ = writer.write_all(b"\n");
                        let _ = writer.flush();
                    };
                    send(serde_json::json!({"type": "ack", "request_id": id}));
                    // Stream on a separate thread so the reader loop keeps
                    // consuming (and can see the ai_cancel) mid-response.
                    thread::spawn(move || {
                        let mut send = |json: serde_json::Value| {
                            let _ = stream_writer.write_all(json.to_string().as_bytes());
                            let _ = stream_writer.write_all(b"\n");
                            let _ = stream_writer.flush();
                        };
                        for chunk in 0..40 {
                            if flag.load(Ordering::SeqCst) {
                                break;
                            }
                            send(serde_json::json!({
                                "type": "ai_delta", "request_id": id,
                                "delta": format!("chunk-{chunk} ")
                            }));
                            thread::sleep(Duration::from_millis(150));
                        }
                        send(serde_json::json!({
                            "type": "ai_response_end", "request_id": id
                        }));
                    });
                }
            });
        }
    });
    cancel_received
}

/// Runs koshell (wrapping bash) with a fake daemon reachable, drives `steps` into
/// it, and returns the raw PTY output. `spawn_daemon` binds the fake daemon on the
/// socket path koshell will look up.
fn run_koshell_session(
    bash: &Path,
    extra_path_dir: Option<&Path>,
    steps: &[(Duration, &[u8])],
    spawn_daemon: impl FnOnce(PathBuf),
) -> String {
    let home = tempfile::tempdir().expect("temp HOME");
    std::fs::write(home.path().join(".bashrc"), "HISTCONTROL=\n").expect("write .bashrc");
    let runtime = tempfile::tempdir().expect("temp XDG_RUNTIME_DIR");
    let socket_dir = runtime.path().join("koshell");
    std::fs::create_dir_all(&socket_dir).expect("create socket dir");
    spawn_daemon(socket_dir.join("daemon.sock"));

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
    // Disable daemon auto-spawn so the no-daemon scenarios stay hermetic (an
    // installed koshell-ai-daemon on PATH would otherwise be launched).
    cmd.env("KOSHELL_NO_DAEMON_SPAWN", "1");
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

#[test]
fn python_repl_answer_streams_above_the_live_prompt_while_typing() {
    let Some(bash) = resolve("bash") else {
        eprintln!("skipping: no bash");
        return;
    };
    let Some(python) = resolve("python3") else {
        eprintln!("skipping anchored stream test: no python3");
        return;
    };

    let output = run_koshell_session(
        &bash,
        python.parent(),
        &[
            (Duration::from_millis(900), b"python3 -q\n"),
            (Duration::from_millis(900), b"#? hello"),
            (Duration::from_millis(300), b"\n"),
            // Typed while the response streams (between the two deltas): must echo
            // on the live line, not into the answer.
            (Duration::from_millis(600), b"1+1"),
            // After the response ends, the line is still a working REPL line.
            (Duration::from_millis(1500), b"\n"),
            (Duration::from_millis(700), b"exit()\n"),
            (Duration::from_millis(400), b"exit\n"),
        ],
        spawn_fake_daemon,
    );

    // Replay the raw byte stream through the terminal emulator: assertions are
    // about the final rendered screen, not the redraw choreography.
    let mut mirror = TerminalMirror::new(80, 24);
    mirror.write(output.as_bytes());
    let screen = mirror.snapshot().screen;

    let question = screen
        .find(">>> #? hello")
        .unwrap_or_else(|| panic!("question line missing.\n--- screen ---\n{screen}"));
    let answer = screen
        .find("[koshell ai]\nHello world\n>>> 1+1")
        .unwrap_or_else(|| {
            panic!(
                "the streamed answer must sit directly above the live prompt carrying \
             the mid-stream input.\n--- screen ---\n{screen}"
            )
        });
    assert!(
        answer > question,
        "answer must render below the question line.\n--- screen ---\n{screen}"
    );
    assert!(
        screen.contains(">>> 1+1\n2"),
        "the interleaved input must still execute normally afterwards.\n--- screen ---\n{screen}"
    );
}

#[test]
fn ctrl_c_mid_stream_stops_the_answer_and_cancels_daemon_side() {
    let Some(bash) = resolve("bash") else {
        eprintln!("skipping: no bash");
        return;
    };
    let Some(python) = resolve("python3") else {
        eprintln!("skipping interrupt test: no python3");
        return;
    };

    let mut cancel_received = None;
    let output = run_koshell_session(
        &bash,
        python.parent(),
        &[
            (Duration::from_millis(900), b"python3 -q\n"),
            (Duration::from_millis(900), b"#? hello"),
            (Duration::from_millis(300), b"\n"),
            // Mid-stream Ctrl+C: swallowed by koshell (never reaches python),
            // stops the rendering immediately, and cancels daemon-side.
            (Duration::from_millis(2000), b"\x03"),
            // The prompt must still be a working REPL line afterwards.
            (Duration::from_millis(800), b"1+1\n"),
            (Duration::from_millis(700), b"exit()\n"),
            (Duration::from_millis(400), b"exit\n"),
        ],
        |socket| {
            cancel_received = Some(spawn_cancellable_daemon(socket));
        },
    );
    let cancel_received = cancel_received.expect("daemon spawned");

    let mut mirror = TerminalMirror::new(80, 24);
    mirror.write(output.as_bytes());
    let screen = mirror.snapshot().screen;

    assert!(
        cancel_received.load(Ordering::SeqCst),
        "the daemon must receive ai_cancel.\n--- screen ---\n{screen}"
    );
    assert!(
        screen.contains("chunk-0"),
        "the part streamed before the interrupt stays on screen.\n--- screen ---\n{screen}"
    );
    // How many chunks render before the interrupt depends on how fast the
    // stabilization fired, so the assertion is about order, not count: once the
    // interrupt notice is on screen, no further chunk may render below it.
    let interrupted = screen.find("answer interrupted (^C)").unwrap_or_else(|| {
        panic!("the interrupt must be acknowledged on screen.\n--- screen ---\n{screen}")
    });
    assert!(
        !screen[interrupted..].contains("chunk-"),
        "the answer tail must not keep rendering after Ctrl+C.\n--- screen ---\n{screen}"
    );
    assert!(
        !screen.contains("KeyboardInterrupt"),
        "the swallowed Ctrl+C must never reach python.\n--- screen ---\n{screen}"
    );
    assert!(
        screen.contains(">>> 1+1\n2"),
        "the REPL line must stay usable after the interrupt.\n--- screen ---\n{screen}"
    );
}

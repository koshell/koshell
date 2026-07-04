//! Real-PTY end-to-end test for the dogfooding event log (design 0007): the
//! anchored-streaming REPL flow must leave the expected JSONL event sequence
//! under the XDG data dir — and, the load-bearing assertion, the raw file must
//! contain no screen content: no prompt text, no command output, no AI answer.
//!
//! The harness mirrors `anchored_stream_pty.rs` (same fake daemon, same proven
//! step timings); assertions on latency fields are only ever `>=`, never exact.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

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
/// two spaced-out deltas ("Hello", " world"), and a response end.
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

/// Runs koshell (wrapping bash) with a fake daemon reachable and the event log
/// pointed at `data_home`, drives `steps` into it, and waits for the session to
/// end (so the event-log writer has been joined and the file is complete).
fn run_koshell_session(
    bash: &Path,
    extra_path_dir: Option<&Path>,
    data_home: &Path,
    no_event_log: bool,
    steps: &[(Duration, &[u8])],
    spawn_daemon: impl FnOnce(PathBuf),
) {
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
    cmd.env("SHELL", bash.as_os_str());
    cmd.env("HOME", home.path());
    cmd.env("XDG_RUNTIME_DIR", runtime.path());
    cmd.env("XDG_DATA_HOME", data_home);
    if no_event_log {
        cmd.env("KOSHELL_NO_EVENT_LOG", "1");
    }
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

    let deadline = Instant::now() + OVERALL_TIMEOUT;
    loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(_) => {}
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
}

#[test]
fn anchored_repl_flow_leaves_the_event_sequence_and_no_screen_content() {
    let Some(bash) = resolve("bash") else {
        eprintln!("skipping: no bash");
        return;
    };
    let Some(python) = resolve("python3") else {
        eprintln!("skipping event log test: no python3");
        return;
    };

    let data_home = tempfile::tempdir().expect("temp XDG_DATA_HOME");
    run_koshell_session(
        &bash,
        python.parent(),
        data_home.path(),
        false,
        &[
            (Duration::from_millis(900), b"python3 -q\n"),
            (Duration::from_millis(900), b"#? hello"),
            (Duration::from_millis(300), b"\n"),
            // Typed while the response streams (between the two deltas): the
            // mid-stream typing metric must count it.
            (Duration::from_millis(600), b"1+1"),
            (Duration::from_millis(1500), b"\n"),
            (Duration::from_millis(700), b"exit()\n"),
            (Duration::from_millis(400), b"exit\n"),
        ],
        spawn_fake_daemon,
    );

    let log_path = data_home.path().join("koshell").join("events.jsonl");
    let raw = std::fs::read_to_string(&log_path).expect("event log file written");

    // The privacy invariant (publicly promised): no screen content, ever. The
    // prompt, the launched command, the typed input, and the AI answer were all
    // on screen — none may appear in the log.
    for leaked in [">>>", "python3", "Hello world", "1+1", "exit()"] {
        assert!(
            !raw.contains(leaked),
            "screen content {leaked:?} leaked into the event log:\n{raw}"
        );
    }

    let events: Vec<serde_json::Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).expect("every line is valid JSON"))
        .collect();
    let kinds: Vec<&str> = events
        .iter()
        .map(|event| event["event"].as_str().expect("event tag"))
        .collect();
    assert_eq!(
        kinds,
        [
            "session_start",
            "question_submitted",
            "dispatched",
            "first_delta",
            "response_end",
            "session_end",
        ],
        "unexpected event sequence:\n{raw}"
    );

    let by_kind = |kind: &str| {
        events
            .iter()
            .find(|event| event["event"] == kind)
            .unwrap_or_else(|| panic!("missing {kind}"))
    };

    let start = by_kind("session_start");
    assert_eq!(start["integrated"], true);
    assert_eq!(start["cols"], 80);
    assert_eq!(start["rows"], 24);
    let session = start["session"].as_str().expect("session id");
    assert!(session.starts_with("koshell-"));
    assert!(events.iter().all(|event| event["session"] == session));
    assert!(
        events
            .iter()
            .all(|event| event["ts"].as_u64().is_some_and(|ts| ts > 0))
    );

    let submitted = by_kind("question_submitted");
    assert_eq!(submitted["question"], "hello");
    assert_eq!(submitted["origin"], "in_program");
    assert_eq!(submitted["form"], "standalone");

    let dispatched = by_kind("dispatched");
    assert_eq!(dispatched["question"], "hello");
    assert_eq!(dispatched["fire_reason"], "stabilized");
    assert_eq!(dispatched["still_running"], false);
    let submit_to_dispatch = dispatched["submit_to_dispatch_ms"]
        .as_u64()
        .expect("latency field");
    assert!(
        submit_to_dispatch >= 100,
        "stabilization needs at least the prompt tier, got {submit_to_dispatch}ms"
    );

    let first_delta = by_kind("first_delta");
    assert_eq!(first_delta["anchored"], true);
    assert_eq!(first_delta["mode"], "stream");
    let to_first = first_delta["dispatch_to_first_delta_ms"]
        .as_u64()
        .expect("latency field");
    assert!(
        to_first >= 100,
        "the fake daemon waits 200ms, got {to_first}ms"
    );

    let end = by_kind("response_end");
    assert_eq!(end["status"], "ok");
    assert_eq!(end["began_anchored"], true);
    assert_eq!(end["degraded_to_block"], false);
    assert_eq!(end["delta_count"], 2);
    let total = end["total_ms"].as_u64().expect("latency field");
    assert!(
        total >= to_first,
        "total {total}ms < first delta {to_first}ms"
    );
    assert!(
        end["mid_stream_input_chunks"].as_u64().unwrap_or(0) >= 1,
        "the mid-stream typing must be counted:\n{raw}"
    );

    assert_eq!(by_kind("session_end")["exit_code"], 0);
    assert!(by_kind("session_end")["duration_ms"].as_u64().unwrap_or(0) > 0);
}

#[test]
fn kill_switch_disables_the_event_log_entirely() {
    let Some(bash) = resolve("bash") else {
        eprintln!("skipping: no bash");
        return;
    };

    let data_home = tempfile::tempdir().expect("temp XDG_DATA_HOME");
    run_koshell_session(
        &bash,
        None,
        data_home.path(),
        true,
        &[
            (Duration::from_millis(900), b"echo hi\n"),
            (Duration::from_millis(400), b"exit\n"),
        ],
        spawn_fake_daemon,
    );

    assert!(
        !data_home
            .path()
            .join("koshell")
            .join("events.jsonl")
            .exists(),
        "KOSHELL_NO_EVENT_LOG must prevent the file from being created"
    );
}

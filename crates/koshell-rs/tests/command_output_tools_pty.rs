//! Real-PTY acceptance for the read-only completed-command tools (design 0020).
//!
//! This is the test the whole slice exists for. A command prints far more than one
//! screen, with a sentinel near the *start* of its output so the sentinel is guaranteed
//! to be outside both the pushed tail and the current screen by the time `#?` fires. A
//! stub daemon then plays the agent's part: it lists the recent commands, picks the id,
//! pages the output, and only answers once it has found the sentinel.
//!
//! The stub stands in for a model so the test needs no provider, no network, and no
//! nondeterminism — what is under test is the terminal half: that one stable id spans
//! a real command, that the bytes captured are the command's and not the prompt's, and
//! that a page request comes back with the evidence the screen no longer holds.
//!
//! It also covers the two negotiation cases in the same harness: an old terminal (no
//! advertised capability) must leave a new daemon push-only, and a tool call the
//! terminal cannot serve must come back as a structured failure rather than hanging.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const OVERALL_TIMEOUT: Duration = Duration::from_secs(30);

/// The sentinel the answer must cite. It is printed first, then buried under enough
/// lines that it cannot be on screen or in the pushed tail when the question fires.
const SENTINEL: &str = "KOSHELL_SENTINEL_9be21f";

/// Lines of filler after the sentinel. The pushed primary-text budget is 8,000
/// characters and the screen is 24 rows, so 400 lines of ~40 characters buries it
/// under roughly 16,000 characters either way.
const FILLER_LINES: usize = 400;

/// The stub "agent": on an `ai_request` it exercises the pull path exactly as the
/// prompt instructs — list, then read the newest command, paging until it finds the
/// sentinel — and reports what it found. It writes a transcript file so the test can
/// assert on the tool traffic as well as the answer.
const STUB_DAEMON_PY: &str = r#"
import json, os, socket

runtime = os.environ["XDG_RUNTIME_DIR"]
transcript_path = os.environ["KOSHELL_TEST_TRANSCRIPT"]
sentinel = os.environ["KOSHELL_TEST_SENTINEL"]
sock_dir = os.path.join(runtime, "koshell")
os.makedirs(sock_dir, exist_ok=True)
sock_path = os.path.join(sock_dir, "daemon.sock")
try:
    os.unlink(sock_path)
except FileNotFoundError:
    pass

transcript = []

def record(kind, value):
    transcript.append({"kind": kind, "value": value})
    with open(transcript_path, "w") as handle:
        json.dump(transcript, handle)

srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(sock_path)
srv.listen(1)
conn, _ = srv.accept()
f = conn.makefile("rwb", buffering=0)

def send(obj):
    f.write((json.dumps(obj) + "\n").encode())

def call_tool(rid, call_id, name, args):
    # The daemon announces every call so the user can watch the work and interrupt.
    send({
        "type": "ai_tool_activity",
        "request_id": rid,
        "tool_name": name,
        "phase": "started",
        "message": "TOOLLINE %s" % name,
    })
    send({
        "type": "ai_tool_call",
        "request_id": rid,
        "tool_call_id": call_id,
        "tool_name": name,
        "arguments": args,
    })
    for raw in f:
        try:
            msg = json.loads(raw)
        except Exception:
            continue
        if msg.get("type") == "tool_response" and msg.get("tool_call_id") == call_id:
            record("tool_response", msg)
            return msg
    return None

for raw in f:
    try:
        msg = json.loads(raw)
    except Exception:
        continue
    if msg.get("type") == "hello":
        record("hello", msg)
        continue
    if msg.get("type") != "ai_request":
        continue

    rid = msg["request_id"]
    record("context_package", msg.get("context_package"))
    send({"type": "ack", "request_id": rid})

    # The whole point: the sentinel must NOT be in the pushed evidence.
    pushed = json.dumps(msg.get("context_package"))
    record("sentinel_in_push", sentinel in pushed)

    listed = call_tool(rid, "tool-1", "list_recent_commands", {})
    answer = "NO ANSWER"
    if listed and listed.get("ok"):
        commands = listed["result"]["commands"]
        record("listed_ids", [c["commandId"] for c in commands])
        # Newest first; the sentinel command is the one that just ended.
        target = commands[0]["commandId"] if commands else None
        if target:
            offset = 0
            pages = 0
            found = False
            while pages < 20:
                page = call_tool(
                    rid,
                    "tool-read-%d" % pages,
                    "read_command_output",
                    {"commandId": target, "offset": offset, "limit": 4000},
                )
                pages += 1
                if not page or not page.get("ok"):
                    break
                result = page["result"]
                if sentinel in result["content"]:
                    found = True
                    break
                if not result["hasMore"]:
                    break
                offset = result["nextOffset"]
            record("pages_read", pages)
            answer = "FOUND %s after %d page(s)" % (sentinel, pages) if found else "NOT FOUND"

    send({"type": "ai_delta", "request_id": rid, "delta": answer})
    send({"type": "ai_response_end", "request_id": rid})
    break
"#;

/// A stub that calls a tool this terminal does not serve, to prove an unserviceable
/// call settles as a structured failure instead of hanging the turn.
const STUB_BAD_TOOL_PY: &str = r#"
import json, os, socket

runtime = os.environ["XDG_RUNTIME_DIR"]
transcript_path = os.environ["KOSHELL_TEST_TRANSCRIPT"]
sock_dir = os.path.join(runtime, "koshell")
os.makedirs(sock_dir, exist_ok=True)
sock_path = os.path.join(sock_dir, "daemon.sock")
try:
    os.unlink(sock_path)
except FileNotFoundError:
    pass

srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(sock_path)
srv.listen(1)
conn, _ = srv.accept()
f = conn.makefile("rwb", buffering=0)

def send(obj):
    f.write((json.dumps(obj) + "\n").encode())

for raw in f:
    try:
        msg = json.loads(raw)
    except Exception:
        continue
    if msg.get("type") != "ai_request":
        continue
    rid = msg["request_id"]
    send({"type": "ack", "request_id": rid})
    send({
        "type": "ai_tool_call",
        "request_id": rid,
        "tool_call_id": "tool-1",
        "tool_name": "run_shell_command",
        "arguments": {"command": "rm -rf /"},
    })
    outcome = "NO RESPONSE"
    for line in f:
        try:
            reply = json.loads(line)
        except Exception:
            continue
        if reply.get("type") == "tool_response":
            with open(transcript_path, "w") as handle:
                json.dump(reply, handle)
            outcome = "REFUSED" if not reply.get("ok") else "SERVED"
            break
    send({"type": "ai_delta", "request_id": rid, "delta": outcome})
    send({"type": "ai_response_end", "request_id": rid})
    break
"#;

fn resolve(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

struct Session {
    output: String,
    transcript: Option<serde_json::Value>,
    /// The dogfooding event log this session wrote (design 0007), one value per line.
    events: Vec<serde_json::Value>,
}

/// Runs koshell wrapping `shell`, drives `steps`, and returns the terminal output plus
/// whatever the stub daemon recorded.
fn run_session(
    shell: &Path,
    python_dir: &Path,
    stub_source: &str,
    steps: &[(Duration, &[u8])],
) -> Session {
    let home = tempfile::tempdir().expect("temp HOME");
    // HISTCONTROL= keeps every line in history, which is where the bash DEBUG trap
    // reads the full command line (with its trailing `#?`) from.
    std::fs::write(home.path().join(".bashrc"), "HISTCONTROL=\n").expect("write .bashrc");
    // zsh does not treat `#` as a comment in an interactive shell by default, so a
    // bare `#? question` would be a glob error rather than a trigger line.
    std::fs::write(home.path().join(".zshrc"), "setopt interactive_comments\n")
        .expect("write .zshrc");
    let runtime = tempfile::tempdir().expect("temp XDG_RUNTIME_DIR");
    let state = tempfile::tempdir().expect("temp XDG_STATE_HOME");
    let data = tempfile::tempdir().expect("temp XDG_DATA_HOME");

    let stub_py = python_dir.join("stub_daemon.py");
    std::fs::write(&stub_py, stub_source).expect("write stub daemon");
    let transcript = python_dir.join("transcript.json");
    let _ = std::fs::remove_file(&transcript);

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

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_koshell"));
    cmd.env_clear();
    cmd.env("SHELL", shell.as_os_str());
    cmd.env("HOME", home.path());
    cmd.env("XDG_RUNTIME_DIR", runtime.path());
    cmd.env("XDG_STATE_HOME", state.path());
    cmd.env("XDG_DATA_HOME", data.path());
    cmd.env(
        "KOSHELL_DAEMON_CMD",
        format!("python3 {}", stub_py.display()),
    );
    cmd.env("KOSHELL_TEST_TRANSCRIPT", &transcript);
    cmd.env("KOSHELL_TEST_SENTINEL", SENTINEL);
    cmd.env("PATH", base_path);
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

    let events = std::fs::read_to_string(data.path().join("koshell").join("events.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();

    Session {
        output: String::from_utf8_lossy(&output).into_owned(),
        transcript: std::fs::read_to_string(&transcript)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok()),
        events,
    }
}

/// The command under test: print the sentinel, then bury it under filler.
///
/// The sentinel lives in a file rather than in the command line, because the pushed
/// package also carries `recentInput` — a sentinel typed at the prompt would be in the
/// push no matter how much output buried it, and the test would prove nothing.
fn sentinel_command(dir: &Path) -> String {
    let sentinel_file = dir.join("sentinel.txt");
    std::fs::write(&sentinel_file, format!("{SENTINEL}\n")).expect("write sentinel file");
    format!(
        "cat {path}; for i in $(seq 1 {FILLER_LINES}); do echo \"filler line $i ----------------------------\"; done\r",
        path = sentinel_file.display(),
    )
}

fn transcript_entries(session: &Session) -> Vec<serde_json::Value> {
    session
        .transcript
        .as_ref()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
}

fn entry<'a>(entries: &'a [serde_json::Value], kind: &str) -> Option<&'a serde_json::Value> {
    entries
        .iter()
        .find(|entry| entry["kind"] == kind)
        .map(|entry| &entry["value"])
}

/// The acceptance case: evidence outside the pushed window is recoverable.
fn sentinel_case(shell_name: &str) {
    let Some(shell) = resolve(shell_name) else {
        eprintln!("skipping: no {shell_name}");
        return;
    };
    if resolve("python3").is_none() {
        eprintln!("skipping: no python3");
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");

    let command = sentinel_command(dir.path());
    let session = run_session(
        &shell,
        dir.path(),
        STUB_DAEMON_PY,
        &[
            // Let the shell settle and source its rc.
            (Duration::from_millis(1_200), b""),
            // Run the command that buries the sentinel.
            (Duration::from_millis(300), command.as_bytes()),
            // Then ask about it on a separate line, so the question's own span is
            // not the one carrying the sentinel.
            (
                Duration::from_millis(2_500),
                b"#? where is the sentinel\r".as_slice(),
            ),
            (Duration::from_millis(4_000), b"exit\r".as_slice()),
        ],
    );

    let entries = transcript_entries(&session);
    assert!(
        !entries.is_empty(),
        "the stub daemon recorded nothing; output was:\n{}",
        session.output
    );

    // The terminal must advertise the capability, or the daemon would never call.
    let hello = entry(&entries, "hello").expect("a hello was recorded");
    let capabilities: Vec<String> = hello["capabilities"]
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        capabilities.iter().any(|c| c == "command_output_tools_v1"),
        "the terminal advertises the command-output capability, got {capabilities:?}"
    );

    // The premise of the whole slice: the sentinel is genuinely not in the push.
    assert_eq!(
        entry(&entries, "sentinel_in_push"),
        Some(&serde_json::Value::Bool(false)),
        "the sentinel must be outside the pushed window for this test to mean anything"
    );

    // And the pushed inventory told the agent that more exists.
    let package = entry(&entries, "context_package").expect("a context package was recorded");
    assert_eq!(
        package["contractVersion"], "koshell_ai_context_v2",
        "the pushed contract advertises v2"
    );
    assert_eq!(
        package["pullContext"]["commandOutput"]["available"],
        serde_json::Value::Bool(true),
        "the inventory advertises retrievable command output"
    );

    // The list carried the sentinel command's span with a stable id.
    let listed = entry(&entries, "listed_ids").expect("the agent listed commands");
    assert!(
        listed.as_array().is_some_and(|ids| !ids.is_empty()),
        "list_recent_commands returned at least one command, got {listed}"
    );

    // The payoff: the answer cites evidence the screen no longer holds.
    assert!(
        session.output.contains(&format!("FOUND {SENTINEL}")),
        "the answer recovered the off-screen sentinel; terminal output was:\n{}",
        session.output
    );

    // And the work was visible while it happened, rather than the terminal sitting
    // silent through several round trips with no basis for deciding to interrupt.
    assert!(
        session.output.contains("TOOLLINE list_recent_commands"),
        "the list call was announced on the terminal; output was:\n{}",
        session.output
    );
    assert!(
        session.output.contains("TOOLLINE read_command_output"),
        "the read call was announced on the terminal; output was:\n{}",
        session.output
    );

    // The tool loop is also on the record, or "how often does the agent pull, and
    // does the user interrupt when it does" stays unmeasurable.
    let events = &session.events;
    let of_kind = |kind: &str| -> Vec<&serde_json::Value> {
        events
            .iter()
            .filter(|event| event["event"] == kind)
            .collect()
    };

    let activity = of_kind("tool_activity");
    let announced: Vec<&str> = activity
        .iter()
        .filter_map(|event| event["tool_name"].as_str())
        .collect();
    assert!(
        announced.contains(&"list_recent_commands") && announced.contains(&"read_command_output"),
        "both announcements are logged, got {announced:?}"
    );
    assert!(
        activity.iter().all(|event| event["rendered"] == true),
        "the user was shown every announcement: {activity:?}"
    );

    let calls = of_kind("tool_call");
    assert!(
        calls.len() >= 2,
        "the served calls are logged with their outcome, got {calls:?}"
    );
    assert!(
        calls.iter().all(|event| event["ok"] == true),
        "the acceptance run's calls all succeeded: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .all(|event| event["duration_us"].as_u64().is_some()),
        "each served call records how long the read took: {calls:?}"
    );

    // The response the tools served reports them, so one line answers whether an
    // answer used tools and how it ended.
    let response_end = of_kind("response_end");
    let end_event = response_end.last().expect("a response_end was logged");
    assert_eq!(end_event["status"], "ok");
    assert!(
        end_event["tool_calls"].as_u64().is_some_and(|n| n >= 2),
        "the response counts the tool calls it made: {end_event}"
    );
    assert_eq!(end_event["tool_failures"], 0);

    // The privacy invariant holds for the new fields too: the log gained tool names
    // and codes, not command output.
    let raw = serde_json::to_string(events).expect("events serialize");
    assert!(
        !raw.contains(SENTINEL),
        "no command output may reach the event log: {raw}"
    );
    assert!(
        !raw.contains("filler line"),
        "no command output may reach the event log: {raw}"
    );
}

#[test]
fn bash_recovers_a_sentinel_outside_the_pushed_window() {
    sentinel_case("bash");
}

#[test]
fn zsh_recovers_a_sentinel_outside_the_pushed_window() {
    sentinel_case("zsh");
}

// The observe-only boundary, proved over the wire: the terminal serves exactly two
// readers, and anything else is refused rather than dispatched.
#[test]
fn an_unserviceable_tool_call_is_refused_without_hanging() {
    let Some(shell) = resolve("bash") else {
        eprintln!("skipping: no bash");
        return;
    };
    if resolve("python3").is_none() {
        eprintln!("skipping: no python3");
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");

    let session = run_session(
        &shell,
        dir.path(),
        STUB_BAD_TOOL_PY,
        &[
            (Duration::from_millis(1_200), b""),
            (Duration::from_millis(300), b"#? anything\r".as_slice()),
            (Duration::from_millis(3_000), b"exit\r".as_slice()),
        ],
    );

    let response = session
        .transcript
        .as_ref()
        .expect("the stub recorded the tool response");
    assert_eq!(response["ok"], serde_json::Value::Bool(false));
    assert_eq!(response["error"]["code"], "unsupported_tool");
    // The refusal settles the call, so the turn completes.
    assert!(
        session.output.contains("REFUSED"),
        "the daemon's turn completed after the refusal; output was:\n{}",
        session.output
    );

    // A refused call is logged with its code, so a daemon calling a tool this
    // terminal cannot serve shows up as a version mismatch instead of as silence.
    let refused = session
        .events
        .iter()
        .find(|event| event["event"] == "tool_call")
        .expect("the refused call was logged");
    assert_eq!(refused["ok"], serde_json::Value::Bool(false));
    assert_eq!(refused["code"], "unsupported_tool");
    // The tool name was neither an announcement nor a name this terminal knows;
    // it is still an identifier, so it is recorded as itself.
    assert_eq!(refused["tool_name"], "run_shell_command");
    assert!(
        !session
            .events
            .iter()
            .any(|event| event["event"] == "tool_activity"),
        "the stub announced nothing, so nothing was rendered or logged"
    );
}

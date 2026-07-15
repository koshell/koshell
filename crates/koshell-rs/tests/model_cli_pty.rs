//! Real-PTY acceptance for the searchable model picker (design 0018). A
//! scripted Unix-socket daemon supplies a catalog and accepts the selected model;
//! no provider, network, or long-running daemon is involved.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use koshell_proto::{ClientMessage, ModelCatalogEntry, ServerMessage};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

fn read_message(reader: &mut impl BufRead) -> ClientMessage {
    let mut line = String::new();
    reader.read_line(&mut line).expect("read client message");
    serde_json::from_str(line.trim_end()).expect("parse client message")
}

fn send_message(stream: &mut UnixStream, message: &ServerMessage) {
    serde_json::to_writer(&mut *stream, message).expect("serialize server message");
    stream.write_all(b"\n").expect("write newline");
    stream.flush().expect("flush reply");
}

#[test]
fn picker_selects_a_model_over_the_additive_ipc_flow() {
    let runtime = tempfile::tempdir().expect("runtime dir");
    let socket_dir = runtime.path().join("koshell");
    std::fs::create_dir(&socket_dir).expect("socket dir");
    let listener = UnixListener::bind(socket_dir.join("daemon.sock")).expect("bind daemon");

    let daemon = thread::spawn(move || {
        // model_cli first performs a reachability-only probe.
        let (probe, _) = listener.accept().expect("accept probe");
        drop(probe);

        let (list_connection, _) = listener.accept().expect("accept list");
        let mut list_writer = list_connection.try_clone().expect("clone list");
        let mut list_reader = BufReader::new(list_connection);
        assert!(matches!(
            read_message(&mut list_reader),
            ClientMessage::Hello { .. }
        ));
        let request_id = match read_message(&mut list_reader) {
            ClientMessage::ModelList {
                request_id,
                all,
                query,
            } => {
                assert!(!all);
                assert_eq!(query, None);
                request_id
            }
            other => panic!("unexpected list request: {other:?}"),
        };
        send_message(
            &mut list_writer,
            &ServerMessage::Ack {
                request_id: request_id.clone(),
            },
        );
        send_message(
            &mut list_writer,
            &ServerMessage::ModelCatalog {
                request_id,
                configured_model: Some("test/one".to_string()),
                entries: vec![ModelCatalogEntry {
                    model_ref: "test/two".to_string(),
                    provider: "test".to_string(),
                    id: "two".to_string(),
                    name: "Test Model Two".to_string(),
                    available: true,
                    context_window: 128_000,
                    reasoning: false,
                }],
            },
        );

        let (set_connection, _) = listener.accept().expect("accept set");
        let mut set_writer = set_connection.try_clone().expect("clone set");
        let mut set_reader = BufReader::new(set_connection);
        assert!(matches!(
            read_message(&mut set_reader),
            ClientMessage::Hello { .. }
        ));
        let request_id = match read_message(&mut set_reader) {
            ClientMessage::ModelSet {
                request_id,
                model,
                session_id,
                session_only,
            } => {
                assert_eq!(model, "test/two");
                assert_eq!(session_id, None);
                assert!(!session_only);
                request_id
            }
            other => panic!("unexpected set request: {other:?}"),
        };
        send_message(
            &mut set_writer,
            &ServerMessage::Ack {
                request_id: request_id.clone(),
            },
        );
        send_message(
            &mut set_writer,
            &ServerMessage::ModelResult {
                request_id,
                ok: true,
                message: "default model set to test/two".to_string(),
                configured_model: Some("test/two".to_string()),
                active_model: None,
            },
        );
    });

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_koshell"));
    command.arg("model");
    command.env_clear();
    command.env("HOME", runtime.path());
    command.env("XDG_RUNTIME_DIR", runtime.path());
    command.env("TERM", "xterm-256color");
    command.env("PATH", "/usr/bin:/bin");

    let mut child = pair.slave.spawn_command(command).expect("spawn picker");
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().expect("clone reader");
    let mut writer = pair.master.take_writer().expect("take writer");
    let (tx, rx) = mpsc::channel();
    let reader_thread = thread::spawn(move || {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).ok();
        let _ = tx.send(bytes);
    });

    thread::sleep(Duration::from_millis(500));
    writer.write_all(b"\r").expect("select first model");
    writer.flush().expect("flush input");
    drop(writer);

    let status = child.wait().expect("wait picker");
    daemon.join().expect("join daemon");
    let output = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("picker output");
    reader_thread.join().expect("join reader");

    assert!(status.success(), "picker exited with {status:?}");
    let output = String::from_utf8_lossy(&output);
    assert!(output.contains("Select a Koshell model"));
    assert!(output.contains("default model set to test/two"));
}

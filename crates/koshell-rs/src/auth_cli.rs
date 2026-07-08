//! `koshell auth <login|logout|status> [provider]` — AI provider credential
//! management (design 0014). No PTY and no session: like `daemon_cli`, this is
//! a plain-stdio client on a one-shot daemon connection. The interactive login
//! flow itself runs inside the daemon (pi is TypeScript-side); this end renders
//! its display events and answers its prompts from stdin.
//!
//! Cancellation is the connection: Ctrl-C keeps its default disposition, the
//! process dies, the socket closes, and the daemon aborts the login. One known
//! cosmetic edge of the blocking prompt read: if the daemon aborts the login
//! (its 15-minute cap) while this end sits at a prompt, the failure prints only
//! after the user presses Enter.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use koshell_proto::{AuthStatusEntry, ClientMessage, PROTOCOL_VERSION, ServerMessage};

use crate::cli::AuthAction;
use crate::daemon_cli::{self, Probe};
use crate::daemon_spawn;
use crate::ipc;

/// How long to wait for the `ack` before concluding the daemon predates the
/// auth messages (it ignores unknown types without replying).
const ACK_TIMEOUT: Duration = Duration::from_secs(2);

/// Runs a `koshell auth` action, returning the process exit code.
pub fn run(action: AuthAction) -> i32 {
    let socket_path = ipc::default_socket_path();
    if !ensure_daemon(&socket_path) {
        return 1;
    }
    let stream = match UnixStream::connect(&socket_path) {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("could not connect to the AI daemon: {error}");
            return 1;
        }
    };
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut out = std::io::stdout();
    match drive(stream, &mut input, &mut out, &action, true) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("koshell auth failed: {error}");
            1
        }
    }
}

/// Connects or auto-starts the daemon, mirroring the terminal's on-demand
/// spawn. Returns false (with guidance printed) when no daemon can be reached.
fn ensure_daemon(socket_path: &Path) -> bool {
    if matches!(daemon_cli::probe(socket_path), Probe::Alive) {
        return true;
    }
    let Some(plan) = daemon_spawn::resolve_plan_from_env() else {
        eprintln!("the AI daemon is not running and no launch command resolved.");
        eprintln!(
            "  set KOSHELL_DAEMON_CMD, or install the koshell-ai-daemon binary next to koshell,"
        );
        eprintln!("  then run `koshell daemon start`.");
        return false;
    };
    if let Err(error) = daemon_spawn::spawn(&plan) {
        eprintln!("failed to start the AI daemon: {error}");
        return false;
    }
    if daemon_cli::wait_until_alive(socket_path) {
        println!("AI daemon: started ({})", plan.source);
        true
    } else {
        eprintln!("started the AI daemon, but it did not become reachable in time.");
        eprintln!(
            "  check the log: {}",
            daemon_spawn::daemon_log_path().display()
        );
        false
    }
}

fn request_for(action: &AuthAction, request_id: &str) -> ClientMessage {
    match action {
        AuthAction::Login { provider } => ClientMessage::AuthLogin {
            request_id: request_id.to_string(),
            provider: provider.clone(),
        },
        AuthAction::Logout { provider } => ClientMessage::AuthLogout {
            request_id: request_id.to_string(),
            provider: provider.clone(),
        },
        AuthAction::Status { provider } => ClientMessage::AuthStatusRequest {
            request_id: request_id.to_string(),
            provider: provider.clone(),
        },
    }
}

fn send_line(writer: &mut UnixStream, message: &ClientMessage) -> anyhow::Result<()> {
    let mut line = serde_json::to_string(message)?;
    line.push('\n');
    writer.write_all(line.as_bytes())?;
    writer.flush()?;
    Ok(())
}

/// Drives one auth request over an established connection. IO is injected so
/// tests can script the daemon side and the user's answers. `open_urls`
/// enables the best-effort browser launch on `auth_url` (off in tests).
fn drive(
    stream: UnixStream,
    input: &mut impl BufRead,
    out: &mut impl Write,
    action: &AuthAction,
    open_urls: bool,
) -> anyhow::Result<i32> {
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let request_id = format!("koshell-auth-{}", std::process::id());

    // The hello handshake gates credential changes on a protocol-version match,
    // so a mismatched daemon answers with its readable upgrade message.
    send_line(
        &mut writer,
        &ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            terminal_session_id: format!("koshell-auth-{}", std::process::id()),
            cwd: std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "/".to_string()),
            shell: "koshell-auth".to_string(),
            rows: 0,
            cols: 0,
        },
    )?;
    send_line(&mut writer, &request_for(action, &request_id))?;

    // A daemon that predates the auth messages never acks them.
    // SO_RCVTIMEO lives on the socket, so setting it via the writer clone
    // bounds the reader too. Best-effort: macOS rejects setsockopt once the
    // peer has hung up (EINVAL), and in that state the next read reports the
    // closed connection immediately anyway.
    let _ = writer.set_read_timeout(Some(ACK_TIMEOUT));
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => anyhow::bail!(too_old_hint()),
            Ok(_) => {}
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                anyhow::bail!(too_old_hint());
            }
            Err(error) => return Err(error.into()),
        }
        if let Ok(ServerMessage::Ack { request_id: id }) =
            serde_json::from_str::<ServerMessage>(line.trim_end())
            && id == request_id
        {
            break;
        }
    }
    let _ = writer.set_read_timeout(None);

    // Event loop: a login legitimately takes as long as the user needs.
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            anyhow::bail!("the connection to the AI daemon closed unexpectedly");
        }
        // Unknown-but-valid message types are skipped, per the additive
        // protocol evolution rules.
        let Ok(message) = serde_json::from_str::<ServerMessage>(line.trim_end()) else {
            continue;
        };
        match message {
            ServerMessage::AuthUrl {
                url, instructions, ..
            } => {
                if open_urls {
                    open_browser(&url);
                }
                writeln!(out, "Open this URL to authorize:")?;
                writeln!(out, "  {url}")?;
                if let Some(instructions) = instructions {
                    writeln!(out, "{instructions}")?;
                }
            }
            ServerMessage::AuthDeviceCode {
                user_code,
                verification_uri,
                expires_in_seconds,
                ..
            } => {
                writeln!(out, "Enter the code {user_code} at:")?;
                writeln!(out, "  {verification_uri}")?;
                if let Some(expires) = expires_in_seconds {
                    writeln!(out, "The code expires in {expires}s.")?;
                }
            }
            ServerMessage::AuthProgress { message, .. } => {
                writeln!(out, "{message}")?;
            }
            ServerMessage::AuthPrompt {
                prompt_id,
                message,
                placeholder,
                allow_empty,
                ..
            } => {
                let value = ask_text(input, out, &message, placeholder.as_deref(), allow_empty)?;
                send_line(
                    &mut writer,
                    &ClientMessage::AuthPromptResponse {
                        request_id: request_id.clone(),
                        prompt_id,
                        value,
                    },
                )?;
            }
            ServerMessage::AuthSelect {
                prompt_id,
                message,
                options,
                ..
            } => {
                let value = ask_select(input, out, &message, &options)?;
                send_line(
                    &mut writer,
                    &ClientMessage::AuthPromptResponse {
                        request_id: request_id.clone(),
                        prompt_id,
                        value,
                    },
                )?;
            }
            ServerMessage::AuthResult { ok, message, .. } => {
                if let Some(message) = message {
                    writeln!(out, "{message}")?;
                }
                return Ok(if ok { 0 } else { 1 });
            }
            ServerMessage::AuthStatus { entries, .. } => {
                return render_status(out, action, &entries);
            }
            _ => {}
        }
    }
}

fn too_old_hint() -> String {
    "the AI daemon did not answer the auth request — it likely predates auth support. \
     Restart it with `koshell daemon restart` so the upgraded daemon is picked up."
        .to_string()
}

/// Asks a free-text question. Returns `None` when stdin reaches EOF (the user
/// declined); re-asks locally on an empty answer unless the prompt allows it.
fn ask_text(
    input: &mut impl BufRead,
    out: &mut impl Write,
    message: &str,
    placeholder: Option<&str>,
    allow_empty: bool,
) -> anyhow::Result<Option<String>> {
    loop {
        match placeholder {
            Some(hint) => write!(out, "{message} [{hint}]: ")?,
            None => write!(out, "{message}: ")?,
        }
        out.flush()?;
        let mut answer = String::new();
        if input.read_line(&mut answer)? == 0 {
            writeln!(out)?;
            return Ok(None);
        }
        let answer = answer.trim_end_matches(['\r', '\n']).to_string();
        if answer.is_empty() && !allow_empty {
            writeln!(out, "(an answer is required)")?;
            continue;
        }
        return Ok(Some(answer));
    }
}

/// Asks a numbered selection, answering with the chosen option id. Returns
/// `None` on stdin EOF; re-asks on input that is not a listed number.
fn ask_select(
    input: &mut impl BufRead,
    out: &mut impl Write,
    message: &str,
    options: &[koshell_proto::AuthSelectOption],
) -> anyhow::Result<Option<String>> {
    writeln!(out, "{message}")?;
    for (index, option) in options.iter().enumerate() {
        writeln!(out, "  {}. {}", index + 1, option.label)?;
    }
    loop {
        write!(out, "Choose [1-{}]: ", options.len())?;
        out.flush()?;
        let mut answer = String::new();
        if input.read_line(&mut answer)? == 0 {
            writeln!(out)?;
            return Ok(None);
        }
        if let Ok(choice) = answer.trim().parse::<usize>()
            && let Some(option) = choice.checked_sub(1).and_then(|i| options.get(i))
        {
            return Ok(Some(option.id.clone()));
        }
        writeln!(out, "(enter a number between 1 and {})", options.len())?;
    }
}

/// Prints the status table. With an explicit provider argument the exit code
/// reports whether that provider is configured (0/1); the full table exits 0.
fn render_status(
    out: &mut impl Write,
    action: &AuthAction,
    entries: &[AuthStatusEntry],
) -> anyhow::Result<i32> {
    for entry in entries {
        let status = if entry.configured {
            match (entry.source.as_deref(), entry.label.as_deref()) {
                (Some("stored"), _) => "logged in".to_string(),
                (Some("environment"), Some(label)) => format!("configured via ${label}"),
                (Some("environment"), None) => "configured via the environment".to_string(),
                (Some("config"), _) => "configured via config.toml".to_string(),
                _ => "configured".to_string(),
            }
        } else if entry.oauth {
            format!(
                "not configured (run `koshell auth login {}`)",
                entry.provider
            )
        } else {
            "not configured".to_string()
        };
        writeln!(out, "{:<16} {:<38} {status}", entry.provider, entry.name)?;
    }
    if let AuthAction::Status {
        provider: Some(provider),
    } = action
    {
        let configured = entries
            .iter()
            .any(|entry| &entry.provider == provider && entry.configured);
        return Ok(if configured { 0 } else { 1 });
    }
    Ok(0)
}

/// Best-effort browser launch: detached, never through a shell, silent on
/// failure — the URL is always printed as the fallback.
fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(target_os = "macos"))]
    let program = "xdg-open";
    let _ = std::process::Command::new(program)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::os::unix::net::UnixListener;
    use std::thread;

    use koshell_proto::AuthSelectOption;

    use super::*;

    /// Starts a scripted daemon: reads client lines and runs `script` with the
    /// connection. Returns the connected client stream and the join handle.
    fn scripted_daemon(
        script: impl FnOnce(UnixStream) + Send + 'static,
    ) -> (UnixStream, thread::JoinHandle<()>) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        let handle = thread::spawn(move || {
            let (conn, _) = listener.accept().expect("accept");
            script(conn);
            // Keep the socket dir alive for the whole exchange.
            drop(dir);
        });
        let stream = UnixStream::connect(&path).expect("connect");
        (stream, handle)
    }

    fn read_client_line(reader: &mut impl BufRead) -> ClientMessage {
        let mut line = String::new();
        reader.read_line(&mut line).expect("read client line");
        serde_json::from_str(line.trim_end()).expect("parse client line")
    }

    fn send_server_line(conn: &mut UnixStream, message: &ServerMessage) {
        let mut line = serde_json::to_string(message).expect("serialize");
        line.push('\n');
        conn.write_all(line.as_bytes()).expect("write");
        conn.flush().expect("flush");
    }

    #[test]
    fn login_round_trip_answers_the_prompt_and_exits_zero() {
        let (stream, handle) = scripted_daemon(|conn| {
            let mut writer = conn.try_clone().expect("clone");
            let mut reader = BufReader::new(conn);
            assert!(matches!(
                read_client_line(&mut reader),
                ClientMessage::Hello { .. }
            ));
            let request_id = match read_client_line(&mut reader) {
                ClientMessage::AuthLogin {
                    request_id,
                    provider,
                } => {
                    assert_eq!(provider, "anthropic");
                    request_id
                }
                other => panic!("unexpected request: {other:?}"),
            };
            send_server_line(
                &mut writer,
                &ServerMessage::Ack {
                    request_id: request_id.clone(),
                },
            );
            send_server_line(
                &mut writer,
                &ServerMessage::AuthUrl {
                    request_id: request_id.clone(),
                    url: "https://example.test/authorize".to_string(),
                    instructions: Some("authorize, then paste the code".to_string()),
                },
            );
            send_server_line(
                &mut writer,
                &ServerMessage::AuthPrompt {
                    request_id: request_id.clone(),
                    prompt_id: "prompt-1".to_string(),
                    message: "Code".to_string(),
                    placeholder: None,
                    allow_empty: false,
                },
            );
            match read_client_line(&mut reader) {
                ClientMessage::AuthPromptResponse {
                    prompt_id, value, ..
                } => {
                    assert_eq!(prompt_id, "prompt-1");
                    assert_eq!(value.as_deref(), Some("the-code"));
                }
                other => panic!("unexpected response: {other:?}"),
            }
            send_server_line(
                &mut writer,
                &ServerMessage::AuthResult {
                    request_id,
                    ok: true,
                    message: Some("logged in to Anthropic".to_string()),
                },
            );
        });

        let mut input = Cursor::new(b"the-code\n".to_vec());
        let mut out = Vec::new();
        let action = AuthAction::Login {
            provider: "anthropic".to_string(),
        };
        let code = drive(stream, &mut input, &mut out, &action, false).expect("drive");
        handle.join().expect("join");

        assert_eq!(code, 0);
        let output = String::from_utf8(out).expect("utf8");
        assert!(output.contains("https://example.test/authorize"));
        assert!(output.contains("authorize, then paste the code"));
        assert!(output.contains("logged in to Anthropic"));
    }

    #[test]
    fn select_prompt_maps_the_number_to_the_option_id() {
        let (stream, handle) = scripted_daemon(|conn| {
            let mut writer = conn.try_clone().expect("clone");
            let mut reader = BufReader::new(conn);
            read_client_line(&mut reader); // hello
            let request_id = match read_client_line(&mut reader) {
                ClientMessage::AuthLogin { request_id, .. } => request_id,
                other => panic!("unexpected request: {other:?}"),
            };
            send_server_line(
                &mut writer,
                &ServerMessage::Ack {
                    request_id: request_id.clone(),
                },
            );
            send_server_line(
                &mut writer,
                &ServerMessage::AuthSelect {
                    request_id: request_id.clone(),
                    prompt_id: "prompt-1".to_string(),
                    message: "How do you want to sign in?".to_string(),
                    options: vec![
                        AuthSelectOption {
                            id: "browser".to_string(),
                            label: "Open a browser".to_string(),
                        },
                        AuthSelectOption {
                            id: "device_code".to_string(),
                            label: "Enter a device code".to_string(),
                        },
                    ],
                },
            );
            match read_client_line(&mut reader) {
                ClientMessage::AuthPromptResponse { value, .. } => {
                    assert_eq!(value.as_deref(), Some("device_code"));
                }
                other => panic!("unexpected response: {other:?}"),
            }
            send_server_line(
                &mut writer,
                &ServerMessage::AuthResult {
                    request_id,
                    ok: true,
                    message: None,
                },
            );
        });

        // "9" is out of range and re-asked; "2" picks the device_code option.
        let mut input = Cursor::new(b"9\n2\n".to_vec());
        let mut out = Vec::new();
        let action = AuthAction::Login {
            provider: "openai-codex".to_string(),
        };
        let code = drive(stream, &mut input, &mut out, &action, false).expect("drive");
        handle.join().expect("join");
        assert_eq!(code, 0);
    }

    #[test]
    fn a_daemon_that_never_acks_reads_as_too_old() {
        let (stream, handle) = scripted_daemon(|conn| {
            // Read both client lines, then hang up without acking — the shape
            // of a pre-auth daemon dropping unknown message types.
            let mut reader = BufReader::new(conn);
            read_client_line(&mut reader);
            read_client_line(&mut reader);
        });

        let mut input = Cursor::new(Vec::new());
        let mut out = Vec::new();
        let action = AuthAction::Status { provider: None };
        let error = drive(stream, &mut input, &mut out, &action, false).unwrap_err();
        handle.join().expect("join");
        assert!(error.to_string().contains("koshell daemon restart"));
    }

    #[test]
    fn failed_result_exits_one_with_the_message() {
        let (stream, handle) = scripted_daemon(|conn| {
            let mut writer = conn.try_clone().expect("clone");
            let mut reader = BufReader::new(conn);
            read_client_line(&mut reader); // hello
            let request_id = match read_client_line(&mut reader) {
                ClientMessage::AuthLogout { request_id, .. } => request_id,
                other => panic!("unexpected request: {other:?}"),
            };
            send_server_line(
                &mut writer,
                &ServerMessage::Ack {
                    request_id: request_id.clone(),
                },
            );
            send_server_line(
                &mut writer,
                &ServerMessage::AuthResult {
                    request_id,
                    ok: false,
                    message: Some("saving the credential failed".to_string()),
                },
            );
        });

        let mut input = Cursor::new(Vec::new());
        let mut out = Vec::new();
        let action = AuthAction::Logout {
            provider: "anthropic".to_string(),
        };
        let code = drive(stream, &mut input, &mut out, &action, false).expect("drive");
        handle.join().expect("join");
        assert_eq!(code, 1);
        assert!(
            String::from_utf8(out)
                .expect("utf8")
                .contains("saving the credential failed")
        );
    }

    #[test]
    fn status_renders_the_table_and_reports_a_named_provider_in_the_exit_code() {
        let entries = vec![
            AuthStatusEntry {
                provider: "anthropic".to_string(),
                name: "Anthropic (Claude Pro/Max)".to_string(),
                oauth: true,
                configured: true,
                source: Some("stored".to_string()),
                label: None,
            },
            AuthStatusEntry {
                provider: "github-copilot".to_string(),
                name: "GitHub Copilot".to_string(),
                oauth: true,
                configured: false,
                source: None,
                label: None,
            },
            AuthStatusEntry {
                provider: "mistral".to_string(),
                name: "mistral".to_string(),
                oauth: false,
                configured: true,
                source: Some("environment".to_string()),
                label: Some("MISTRAL_API_KEY".to_string()),
            },
        ];
        let mut out = Vec::new();
        let code = render_status(&mut out, &AuthAction::Status { provider: None }, &entries)
            .expect("render");
        assert_eq!(code, 0);
        let output = String::from_utf8(out).expect("utf8");
        assert!(output.contains("logged in"));
        assert!(output.contains("koshell auth login github-copilot"));
        assert!(output.contains("configured via $MISTRAL_API_KEY"));

        let mut out = Vec::new();
        let code = render_status(
            &mut out,
            &AuthAction::Status {
                provider: Some("github-copilot".to_string()),
            },
            &entries[1..2],
        )
        .expect("render");
        assert_eq!(code, 1);

        let mut out = Vec::new();
        let code = render_status(
            &mut out,
            &AuthAction::Status {
                provider: Some("anthropic".to_string()),
            },
            &entries[..1],
        )
        .expect("render");
        assert_eq!(code, 0);
    }
}

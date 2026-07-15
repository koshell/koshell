//! `koshell model` — live model discovery, scripted selection, and a searchable
//! crossterm picker (design 0018). pi and provider semantics remain daemon-side;
//! this module is a plain-stdio IPC client plus terminal presentation.

use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use koshell_proto::{ClientMessage, ModelCatalogEntry, PROTOCOL_VERSION, ServerMessage};

use crate::cli::ModelAction;
use crate::daemon_cli::{self, Probe};
use crate::{daemon_spawn, ipc};

const ACK_TIMEOUT: Duration = Duration::from_secs(2);

pub fn run(root_session_only: bool, action: Option<ModelAction>) -> i32 {
    if root_session_only && action.is_some() {
        eprintln!("place --session-only after `model set`, or omit the subcommand for the picker");
        return 2;
    }
    let socket_path = ipc::default_socket_path();
    if !ensure_daemon(&socket_path) {
        return 1;
    }

    let result = match action {
        None => run_picker(&socket_path, root_session_only),
        Some(ModelAction::Show) => run_show(&socket_path),
        Some(ModelAction::List { all, query }) => run_list(&socket_path, all, query),
        Some(ModelAction::Set {
            model,
            session_only,
        }) => run_set(&socket_path, model, session_only),
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("koshell model failed: {error}");
            1
        }
    }
}

fn ensure_daemon(socket_path: &Path) -> bool {
    if matches!(daemon_cli::probe(socket_path), Probe::Alive) {
        return true;
    }
    let Some(plan) = daemon_spawn::resolve_plan_from_env() else {
        eprintln!("the AI daemon is not running and no launch command resolved.");
        eprintln!(
            "  set KOSHELL_DAEMON_CMD, or install koshell-ai-daemon next to koshell, then run `koshell daemon start`."
        );
        return false;
    };
    if let Err(error) = daemon_spawn::spawn(&plan) {
        eprintln!("failed to start the AI daemon: {error}");
        return false;
    }
    if daemon_cli::wait_until_alive(socket_path) {
        true
    } else {
        eprintln!("started the AI daemon, but it did not become reachable in time.");
        false
    }
}

fn request(path: &Path, message: ClientMessage, request_id: &str) -> anyhow::Result<ServerMessage> {
    let stream = UnixStream::connect(path)?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let (cols, rows) = crossterm::terminal::size().unwrap_or((0, 0));
    send_line(
        &mut writer,
        &ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            terminal_session_id: format!("koshell-model-{}", std::process::id()),
            cwd: std::env::current_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| "/".to_string()),
            shell: "koshell-model".to_string(),
            rows,
            cols,
        },
    )?;
    send_line(&mut writer, &message)?;
    let _ = writer.set_read_timeout(Some(ACK_TIMEOUT));
    loop {
        let Some(reply) = read_message(&mut reader)? else {
            anyhow::bail!(too_old_hint());
        };
        if let ServerMessage::Ack { request_id: id } = reply
            && id == request_id
        {
            break;
        }
    }
    let _ = writer.set_read_timeout(None);
    loop {
        let Some(reply) = read_message(&mut reader)? else {
            anyhow::bail!("the connection to the AI daemon closed unexpectedly");
        };
        let matches_request = match &reply {
            ServerMessage::ModelCatalog { request_id: id, .. }
            | ServerMessage::ModelState { request_id: id, .. }
            | ServerMessage::ModelResult { request_id: id, .. } => id == request_id,
            _ => false,
        };
        if matches_request {
            return Ok(reply);
        }
    }
}

fn send_line(stream: &mut UnixStream, message: &ClientMessage) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *stream, message)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

fn read_message(reader: &mut BufReader<UnixStream>) -> anyhow::Result<Option<ServerMessage>> {
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return Ok(None),
            Ok(_) => {}
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                anyhow::bail!(too_old_hint());
            }
            Err(error) => return Err(error.into()),
        }
        if let Ok(message) = serde_json::from_str(line.trim_end()) {
            return Ok(Some(message));
        }
    }
}

fn too_old_hint() -> &'static str {
    "the AI daemon did not answer the model request; restart it with `koshell daemon restart` so the upgraded daemon is picked up"
}

fn next_request_id() -> String {
    format!("koshell-model-{}", std::process::id())
}

fn session_id() -> Option<String> {
    ipc::current_session_id()
}

fn fetch_catalog(
    path: &Path,
    all: bool,
    query: Option<String>,
) -> anyhow::Result<(Option<String>, Vec<ModelCatalogEntry>)> {
    let request_id = next_request_id();
    match request(
        path,
        ClientMessage::ModelList {
            request_id: request_id.clone(),
            all,
            query,
        },
        &request_id,
    )? {
        ServerMessage::ModelCatalog {
            configured_model,
            entries,
            ..
        } => Ok((configured_model, entries)),
        ServerMessage::ModelResult { message, .. } => anyhow::bail!(message),
        other => anyhow::bail!("unexpected daemon reply: {other:?}"),
    }
}

fn run_list(path: &Path, all: bool, query: Option<String>) -> anyhow::Result<i32> {
    let (_, entries) = fetch_catalog(path, all, query)?;
    for entry in entries {
        let availability = if entry.available {
            "ready"
        } else {
            "no credentials"
        };
        println!(
            "{:<58} {:<16} {:>7}k  {}",
            entry.model_ref,
            availability,
            entry.context_window / 1_000,
            entry.name
        );
    }
    Ok(0)
}

fn run_show(path: &Path) -> anyhow::Result<i32> {
    let request_id = next_request_id();
    match request(
        path,
        ClientMessage::ModelShow {
            request_id: request_id.clone(),
            session_id: session_id(),
        },
        &request_id,
    )? {
        ServerMessage::ModelState {
            configured_model,
            active_model,
            conversation,
            ..
        } => {
            println!(
                "configured default: {}",
                configured_model.as_deref().unwrap_or("—")
            );
            if session_id().is_some() {
                println!(
                    "active conversation: {}",
                    if conversation {
                        active_model.as_deref().unwrap_or("—")
                    } else {
                        "— (no conversation yet)"
                    }
                );
            }
            Ok(0)
        }
        ServerMessage::ModelResult { message, .. } => anyhow::bail!(message),
        other => anyhow::bail!("unexpected daemon reply: {other:?}"),
    }
}

fn run_set(path: &Path, model: String, session_only: bool) -> anyhow::Result<i32> {
    let current_session = session_id();
    if session_only && current_session.is_none() {
        anyhow::bail!("--session-only must be run inside a koshell session");
    }
    let request_id = next_request_id();
    match request(
        path,
        ClientMessage::ModelSet {
            request_id: request_id.clone(),
            model,
            session_id: current_session,
            session_only,
        },
        &request_id,
    )? {
        ServerMessage::ModelResult { ok, message, .. } => {
            println!("{message}");
            Ok(if ok { 0 } else { 1 })
        }
        other => anyhow::bail!("unexpected daemon reply: {other:?}"),
    }
}

#[derive(Debug)]
struct PickerState {
    query: String,
    selected: usize,
}

impl PickerState {
    fn new() -> Self {
        Self {
            query: String::new(),
            selected: 0,
        }
    }

    fn filtered<'a>(&self, entries: &'a [ModelCatalogEntry]) -> Vec<&'a ModelCatalogEntry> {
        let query = self.query.to_lowercase();
        entries
            .iter()
            .filter(|entry| {
                query.is_empty()
                    || entry.model_ref.to_lowercase().contains(&query)
                    || entry.provider.to_lowercase().contains(&query)
                    || entry.id.to_lowercase().contains(&query)
                    || entry.name.to_lowercase().contains(&query)
            })
            .collect()
    }

    fn clamp(&mut self, len: usize) {
        self.selected = self.selected.min(len.saturating_sub(1));
    }
}

struct PickerGuard;

impl PickerGuard {
    fn enter() -> anyhow::Result<Self> {
        enable_raw_mode()?;
        execute!(std::io::stdout(), EnterAlternateScreen, Hide)?;
        Ok(Self)
    }
}

impl Drop for PickerGuard {
    fn drop(&mut self) {
        let _ = execute!(std::io::stdout(), Show, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

fn run_picker(path: &Path, session_only: bool) -> anyhow::Result<i32> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        anyhow::bail!(
            "the interactive picker requires a terminal; use `koshell model list` and `koshell model set <provider/id>` in scripts"
        );
    }
    let (configured, entries) = fetch_catalog(path, false, None)?;
    if entries.is_empty() {
        anyhow::bail!(
            "no credential-available models found; run `koshell model list --all`, configure credentials, and restart the daemon if you exported a key after it started"
        );
    }

    let selected = pick_model(&entries, configured.as_deref())?;
    match selected {
        Some(model) => run_set(path, model, session_only),
        None => Ok(0),
    }
}

fn pick_model(
    entries: &[ModelCatalogEntry],
    configured: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let _guard = PickerGuard::enter()?;
    let mut state = PickerState::new();
    loop {
        let filtered = state.filtered(entries);
        state.clamp(filtered.len());
        draw_picker(&state, &filtered, configured)?;
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Esc => return Ok(None),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(None);
                }
                KeyCode::Enter => {
                    return Ok(filtered
                        .get(state.selected)
                        .map(|entry| entry.model_ref.clone()));
                }
                KeyCode::Up => state.selected = state.selected.saturating_sub(1),
                KeyCode::Down => {
                    if state.selected + 1 < filtered.len() {
                        state.selected += 1;
                    }
                }
                KeyCode::Backspace => {
                    state.query.pop();
                    state.selected = 0;
                }
                KeyCode::Char(character)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    state.query.push(character);
                    state.selected = 0;
                }
                _ => {}
            }
        }
    }
}

fn draw_picker(
    state: &PickerState,
    entries: &[&ModelCatalogEntry],
    configured: Option<&str>,
) -> anyhow::Result<()> {
    let mut out = std::io::stdout();
    let (width, height) = crossterm::terminal::size().unwrap_or((80, 24));
    let visible = usize::from(height.saturating_sub(6)).max(1);
    let start = state.selected.saturating_sub(visible.saturating_sub(1));
    execute!(
        out,
        MoveTo(0, 0),
        Clear(ClearType::All),
        SetAttribute(Attribute::Bold),
        Print("Select a Koshell model"),
        SetAttribute(Attribute::Reset),
        Print("\r\n"),
        Print(format!("Default: {}\r\n", configured.unwrap_or("—"))),
        Print(format!("Search: {}\r\n\r\n", state.query))
    )?;
    for (index, entry) in entries.iter().enumerate().skip(start).take(visible) {
        let marker = if index == state.selected { "> " } else { "  " };
        let current = if configured == Some(entry.model_ref.as_str()) {
            " *"
        } else {
            ""
        };
        let line = format!("{marker}{} — {}{current}", entry.model_ref, entry.name);
        let truncated: String = line.chars().take(usize::from(width)).collect();
        if index == state.selected {
            execute!(out, SetAttribute(Attribute::Reverse))?;
        }
        execute!(
            out,
            Print(truncated),
            SetAttribute(Attribute::Reset),
            Print("\r\n")
        )?;
    }
    execute!(
        out,
        MoveTo(0, height.saturating_sub(1)),
        Print("Type to filter · ↑/↓ move · Enter select · Esc cancel")
    )?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;
    use std::thread;

    use super::*;

    fn entry(model_ref: &str, name: &str) -> ModelCatalogEntry {
        let (provider, id) = model_ref.split_once('/').expect("provider/id");
        ModelCatalogEntry {
            model_ref: model_ref.to_string(),
            provider: provider.to_string(),
            id: id.to_string(),
            name: name.to_string(),
            available: true,
            context_window: 128_000,
            reasoning: false,
        }
    }

    #[test]
    fn picker_filter_covers_provider_id_and_name() {
        let entries = vec![
            entry("anthropic/claude-sonnet", "Claude Sonnet"),
            entry("openai/gpt-5", "GPT Five"),
        ];
        let mut state = PickerState::new();
        state.query = "ANTHROPIC".to_string();
        assert_eq!(
            state.filtered(&entries)[0].model_ref,
            "anthropic/claude-sonnet"
        );
        state.query = "five".to_string();
        assert_eq!(state.filtered(&entries)[0].model_ref, "openai/gpt-5");
        state.query = "gpt-5".to_string();
        assert_eq!(state.filtered(&entries)[0].model_ref, "openai/gpt-5");
    }

    #[test]
    fn picker_selection_clamps_after_filtering() {
        let mut state = PickerState {
            query: String::new(),
            selected: 8,
        };
        state.clamp(2);
        assert_eq!(state.selected, 1);
        state.clamp(0);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn request_handshakes_and_reads_a_catalog() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut hello = String::new();
            let mut request = String::new();
            reader.read_line(&mut hello).expect("hello");
            reader.read_line(&mut request).expect("request");
            assert!(matches!(
                serde_json::from_str::<ClientMessage>(hello.trim_end()).expect("parse hello"),
                ClientMessage::Hello { .. }
            ));
            assert!(matches!(
                serde_json::from_str::<ClientMessage>(request.trim_end()).expect("parse request"),
                ClientMessage::ModelList { .. }
            ));
            for reply in [
                ServerMessage::Ack {
                    request_id: "model-test".to_string(),
                },
                ServerMessage::ModelCatalog {
                    request_id: "model-test".to_string(),
                    configured_model: Some("test/one".to_string()),
                    entries: vec![entry("test/one", "Test One")],
                },
            ] {
                serde_json::to_writer(&mut stream, &reply).expect("serialize");
                stream.write_all(b"\n").expect("newline");
                stream.flush().expect("flush");
            }
        });

        let reply = request(
            &path,
            ClientMessage::ModelList {
                request_id: "model-test".to_string(),
                all: false,
                query: None,
            },
            "model-test",
        )
        .expect("model reply");
        server.join().expect("join");
        match reply {
            ServerMessage::ModelCatalog {
                configured_model,
                entries,
                ..
            } => {
                assert_eq!(configured_model.as_deref(), Some("test/one"));
                assert_eq!(entries[0].model_ref, "test/one");
            }
            other => panic!("unexpected reply: {other:?}"),
        }
    }

    #[test]
    fn request_reports_upgrade_guidance_when_an_old_daemon_does_not_ack() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).expect("hello");
            line.clear();
            reader.read_line(&mut line).expect("request");
            // A pre-design-0018 daemon ignores the unknown request and closes.
        });

        let error = request(
            &path,
            ClientMessage::ModelShow {
                request_id: "model-test".to_string(),
                session_id: None,
            },
            "model-test",
        )
        .expect_err("old daemon must fail");
        server.join().expect("join");
        assert!(error.to_string().contains("koshell daemon restart"));
    }
}

//! `koshell new` and `koshell clear` — the two conversation/context reset commands
//! (design 0023).
//!
//! Both run as an ordinary child process of the inner shell, so they cannot touch the
//! wrapper's state directly: the mirror, timeline, command index, and the live daemon
//! connection all live in the `koshell` process one level up. They address it in band
//! instead, by writing an OSC 777 control marker to the controlling terminal
//! (`shell_integration::MarkerKind::is_control`). The wrapper's marker scanner strips
//! those bytes from the stream before the terminal or the mirror sees them and applies
//! the request from the one thread that owns the state (`session::apply_control_marker`),
//! which is also where the user-visible notice comes from.
//!
//! Why `/dev/tty` and not stdout: the marker is a request addressed to the terminal, not
//! output. Writing it to stdout would let `koshell clear > log` silently drop the request
//! into a file (and put a stray escape sequence in it), while `/dev/tty` reaches the
//! wrapper under any redirection. Nothing is written to stdout at all, so the commands
//! stay quiet in pipelines.
//!
//! Delivery is one-way, so the exit code reports that the request was *sent*, not what it
//! achieved; the wrapper's notice is the outcome. That is also why the daemon half is
//! fire-and-forget (`ClientMessage::ConversationReset`): the user asked for the reset, so
//! there is no decision left for a reply to carry.

use std::collections::HashMap;
use std::io::Write;

use crate::shell;
use crate::shell_integration::{MarkerKind, ShellIntegrationMarker, format_marker};

/// Which reset the user asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// `koshell new`: discard the AI conversation only.
    NewConversation,
    /// `koshell clear`: discard the screen, the AI-readable terminal evidence, and the
    /// conversation.
    ClearContext,
}

impl Control {
    fn marker_kind(self) -> MarkerKind {
        match self {
            Control::NewConversation => MarkerKind::NewConversation,
            Control::ClearContext => MarkerKind::ClearContext,
        }
    }

    fn command(self) -> &'static str {
        match self {
            Control::NewConversation => "koshell new",
            Control::ClearContext => "koshell clear",
        }
    }
}

/// Runs `koshell new` / `koshell clear`, returning the process exit code.
pub fn run(control: Control) -> i32 {
    let env: HashMap<String, String> = std::env::vars().collect();
    if !inside_live_koshell(&env) {
        eprintln!(
            "{} needs a koshell-wrapped terminal: no live koshell owns this one.",
            control.command()
        );
        eprintln!("  start koshell (or install the auto-wrap: `koshell shell-init zsh`).");
        return 1;
    }
    match emit(control) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{} could not reach koshell: {error}", control.command());
            1
        }
    }
}

/// Whether a live koshell owns this process's terminal — exactly the condition that makes
/// *launching* koshell here a refused nested start, read in the positive direction. Reusing
/// [`shell::is_nested_koshell`] keeps the two answers from ever disagreeing: if koshell
/// refuses to start here, `new`/`clear` work here, and vice versa.
fn inside_live_koshell(env: &HashMap<String, String>) -> bool {
    let tty = shell::controlling_tty();
    let marker_live = env
        .get(shell::KOSHELL_ENV_KEY)
        .and_then(|value| shell::koshell_tty(value))
        .map(shell::tty_is_live)
        .unwrap_or(false);
    shell::is_nested_koshell(env, tty.as_deref(), marker_live)
}

/// Writes the control marker to the controlling terminal.
fn emit(control: Control) -> std::io::Result<()> {
    let marker = format_marker(&ShellIntegrationMarker {
        kind: control.marker_kind(),
        command: None,
        exit_code: None,
        cwd: None,
        executed: true,
    });
    let mut tty = std::fs::OpenOptions::new().write(true).open("/dev/tty")?;
    tty.write_all(&marker)?;
    tty.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell_integration::{MARKER_PREFIX, MARKER_SUFFIX, parse_marker_payload};

    fn env_of(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn outside_koshell_is_refused_without_touching_the_terminal() {
        assert!(!inside_live_koshell(&env_of(&[])));
        // A brand naming another terminal (the tmux-pane shape) is not this terminal's
        // koshell either: that pane's own koshell is the one that would answer.
        assert!(!inside_live_koshell(&env_of(&[(
            "KOSHELL",
            "koshell-1,/dev/pts/999"
        )])));
    }

    #[test]
    fn the_coarse_brand_is_enough_to_address_the_wrapper() {
        // Without a tty field koshell could not brand the child pts, but the marker still
        // reaches whoever owns /dev/tty — the same fallback the nested guard takes.
        assert!(inside_live_koshell(&env_of(&[("KOSHELL", "koshell-1")])));
    }

    #[test]
    fn each_control_emits_its_own_parseable_marker() {
        for (control, kind) in [
            (Control::NewConversation, MarkerKind::NewConversation),
            (Control::ClearContext, MarkerKind::ClearContext),
        ] {
            let bytes = format_marker(&ShellIntegrationMarker {
                kind: control.marker_kind(),
                command: None,
                exit_code: None,
                cwd: None,
                executed: true,
            });
            assert!(bytes.starts_with(MARKER_PREFIX));
            assert_eq!(*bytes.last().unwrap(), MARKER_SUFFIX);
            let payload = &bytes[MARKER_PREFIX.len()..bytes.len() - 1];
            let parsed = parse_marker_payload(payload).expect("the wrapper can parse it");
            assert_eq!(parsed.kind, kind);
            assert!(parsed.kind.is_control());
        }
    }
}

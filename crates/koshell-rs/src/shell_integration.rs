//! Shell integration: generate temporary bash/zsh rc files that emit OSC 777 command
//! boundary markers, and parse those markers back out of the PTY stream.
//!
//! Ported from `coshell/src/shell-integration.ts` (renamed to koshell). The rc hooks
//! source the user's own rc files first, then install `preexec`/`precmd` (zsh) or a
//! `DEBUG` trap plus `PROMPT_COMMAND` (bash). A `command_start` marker carries the
//! command line; `command_end` carries the command line and exit code. The precmd
//! fallback also emits markers for comment-only lines containing `#?`, so a bare
//! `#? question` works even though the shell runs no command.

use std::collections::HashMap;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use tempfile::TempDir;

/// OSC 777 marker prefix identifying koshell command-boundary markers.
pub const MARKER_PREFIX: &[u8] = b"\x1b]777;koshell;";
/// BEL terminator for the marker.
pub const MARKER_SUFFIX: u8 = 0x07;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellIntegrationKind {
    Bash,
    Zsh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerKind {
    CommandStart,
    CommandEnd,
}

/// A parsed command-boundary marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellIntegrationMarker {
    pub kind: MarkerKind,
    pub command: Option<String>,
    pub exit_code: Option<i32>,
}

/// How to launch the child shell, including any generated rc file.
pub struct ShellLaunchConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub kind: Option<ShellIntegrationKind>,
    // Kept alive for the session lifetime; the temp dir is removed on drop.
    _temp_dir: Option<TempDir>,
}

/// Builds a launch config for `shell`, installing shell integration for bash/zsh.
/// Falls back to launching the shell directly for other shells.
pub fn create_shell_launch_config(
    shell: &str,
    env: HashMap<String, String>,
) -> anyhow::Result<ShellLaunchConfig> {
    let shell_name = Path::new(shell)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    match shell_name {
        "bash" => create_bash_launch_config(shell, env),
        "zsh" => create_zsh_launch_config(shell, env),
        _ => Ok(ShellLaunchConfig {
            command: shell.to_string(),
            args: Vec::new(),
            env,
            kind: None,
            _temp_dir: None,
        }),
    }
}

/// Parses a base64 marker payload into a [`ShellIntegrationMarker`].
pub fn parse_marker_payload(payload: &[u8]) -> Option<ShellIntegrationMarker> {
    let decoded = BASE64.decode(payload).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    let kind = match value.get("type")?.as_str()? {
        "command_start" => MarkerKind::CommandStart,
        "command_end" => MarkerKind::CommandEnd,
        _ => return None,
    };
    let command = value
        .get("command")
        .and_then(|c| c.as_str())
        .map(str::to_string);
    let exit_code = value
        .get("exitCode")
        .and_then(serde_json::Value::as_i64)
        .map(|c| c as i32);
    Some(ShellIntegrationMarker {
        kind,
        command,
        exit_code: if kind == MarkerKind::CommandEnd {
            exit_code
        } else {
            None
        },
    })
}

/// Formats a marker as the on-wire OSC sequence (used by tests).
pub fn format_marker(marker: &ShellIntegrationMarker) -> Vec<u8> {
    let type_str = match marker.kind {
        MarkerKind::CommandStart => "command_start",
        MarkerKind::CommandEnd => "command_end",
    };
    let mut json = serde_json::json!({ "type": type_str });
    if let Some(command) = &marker.command {
        json["command"] = serde_json::Value::String(command.clone());
    }
    if let Some(exit_code) = marker.exit_code {
        json["exitCode"] = serde_json::Value::Number(exit_code.into());
    }
    let payload = BASE64.encode(serde_json::to_vec(&json).unwrap_or_default());
    let mut out = Vec::new();
    out.extend_from_slice(MARKER_PREFIX);
    out.extend_from_slice(payload.as_bytes());
    out.push(MARKER_SUFFIX);
    out
}

/// A segment of the PTY stream: either user-visible bytes or a parsed marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Visible(Vec<u8>),
    Marker(ShellIntegrationMarker),
}

/// Splits a PTY byte stream into visible output and koshell markers, buffering across
/// chunk boundaries so a marker split across reads is still detected and never leaks to
/// the user's terminal.
#[derive(Default)]
pub struct MarkerScanner {
    buf: Vec<u8>,
}

impl MarkerScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds a chunk and returns the ordered segments that are now complete.
    pub fn feed(&mut self, data: &[u8]) -> Vec<Segment> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        let mut visible: Vec<u8> = Vec::new();

        loop {
            if let Some(pos) = find_subslice(&self.buf, MARKER_PREFIX) {
                visible.extend_from_slice(&self.buf[..pos]);
                let after = pos + MARKER_PREFIX.len();
                if let Some(rel) = self.buf[after..].iter().position(|&b| b == MARKER_SUFFIX) {
                    let payload_end = after + rel;
                    if !visible.is_empty() {
                        out.push(Segment::Visible(std::mem::take(&mut visible)));
                    }
                    if let Some(marker) = parse_marker_payload(&self.buf[after..payload_end]) {
                        out.push(Segment::Marker(marker));
                    }
                    self.buf.drain(..payload_end + 1);
                    continue;
                }
                // Incomplete marker: keep it buffered until the suffix arrives.
                self.buf.drain(..pos);
                break;
            }

            // No complete prefix; emit everything except a possible partial-prefix tail.
            let keep = partial_prefix_len(&self.buf, MARKER_PREFIX);
            let emit_upto = self.buf.len() - keep;
            visible.extend_from_slice(&self.buf[..emit_upto]);
            self.buf.drain(..emit_upto);
            break;
        }

        if !visible.is_empty() {
            out.push(Segment::Visible(visible));
        }
        out
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Length of the longest suffix of `buf` that is a proper prefix of `prefix`.
fn partial_prefix_len(buf: &[u8], prefix: &[u8]) -> usize {
    let max = prefix.len().saturating_sub(1).min(buf.len());
    (1..=max)
        .rev()
        .find(|&k| buf[buf.len() - k..] == prefix[..k])
        .unwrap_or(0)
}

fn create_bash_launch_config(
    shell: &str,
    env: HashMap<String, String>,
) -> anyhow::Result<ShellLaunchConfig> {
    let temp_dir = tempfile::Builder::new().prefix("koshell-bash-").tempdir()?;
    let rc_file = temp_dir.path().join("bashrc");
    let home = std::env::var("HOME").unwrap_or_default();
    let user_rc = format!("{home}/.bashrc");

    let contents = format!(
        "{source_rc}\n{shared}{bash_hooks}",
        source_rc = source_if_readable(&user_rc),
        shared = shared_shell_functions(),
        bash_hooks = BASH_HOOKS,
    );
    std::fs::write(&rc_file, contents)?;

    Ok(ShellLaunchConfig {
        command: shell.to_string(),
        args: vec![
            "--rcfile".to_string(),
            rc_file.to_string_lossy().to_string(),
            "-i".to_string(),
        ],
        env,
        kind: Some(ShellIntegrationKind::Bash),
        _temp_dir: Some(temp_dir),
    })
}

fn create_zsh_launch_config(
    shell: &str,
    mut env: HashMap<String, String>,
) -> anyhow::Result<ShellLaunchConfig> {
    let temp_dir = tempfile::Builder::new().prefix("koshell-zsh-").tempdir()?;
    let zshrc = temp_dir.path().join(".zshrc");
    let home = std::env::var("HOME").unwrap_or_default();

    let contents = format!(
        "{zshenv}\n{zprofile}\n{zshrc_src}\n{shared}{zsh_hooks}",
        zshenv = source_if_readable(&format!("{home}/.zshenv")),
        zprofile = source_if_readable(&format!("{home}/.zprofile")),
        zshrc_src = source_if_readable(&format!("{home}/.zshrc")),
        shared = shared_shell_functions(),
        zsh_hooks = ZSH_HOOKS,
    );
    std::fs::write(&zshrc, contents)?;

    // zsh sources rc files from ZDOTDIR; point it at our temp dir.
    env.insert(
        "ZDOTDIR".to_string(),
        temp_dir.path().to_string_lossy().to_string(),
    );

    Ok(ShellLaunchConfig {
        command: shell.to_string(),
        args: vec!["-i".to_string()],
        env,
        kind: Some(ShellIntegrationKind::Zsh),
        _temp_dir: Some(temp_dir),
    })
}

fn source_if_readable(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let quoted = shell_quote(path);
    format!("[ -r {quoted} ] && . {quoted}")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn shared_shell_functions() -> &'static str {
    SHARED_SHELL_FUNCTIONS
}

const SHARED_SHELL_FUNCTIONS: &str = r#"__koshell_base64() {
  if command -v base64 >/dev/null 2>&1; then
    printf '%s' "$1" | base64 | tr -d '\n'
  else
    printf '%s' "$1"
  fi
}
__koshell_json_escape() {
  printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'
}
__koshell_emit_marker() {
  local type="$1"
  local command="$2"
  local exit_code="${3:-}"
  local escaped_command
  escaped_command=$(__koshell_json_escape "$command")
  if [ -n "$exit_code" ]; then
    __koshell_payload='{"type":"'"$type"'","command":"'"$escaped_command"'","exitCode":'"$exit_code"'}'
  else
    __koshell_payload='{"type":"'"$type"'","command":"'"$escaped_command"'"}'
  fi
  printf '\033]777;koshell;%s\007' "$(__koshell_base64 "$__koshell_payload")"
}
__koshell_emit_start() {
  __koshell_emit_marker "command_start" "$1"
}
__koshell_emit_end() {
  __koshell_emit_marker "command_end" "$2" "$1"
}
"#;

const BASH_HOOKS: &str = r#"__koshell_last_started_command=""
__koshell_last_history_command=""
__koshell_command_active=""
__koshell_in_prompt_command=""
__koshell_debug_trap() {
  local command="$BASH_COMMAND"
  if [ -n "$__koshell_in_prompt_command" ]; then
    return
  fi
  case "$command" in
    __koshell_*|PROMPT_COMMAND=*|trap\ *) return ;;
  esac
  if [ "$command" != "$__koshell_last_started_command" ]; then
    __koshell_last_started_command="$command"
    __koshell_command_active=1
    __koshell_emit_start "$command"
  fi
}
__koshell_prompt_command() {
  local exit_status=$?
  __koshell_in_prompt_command=1
  local history_command
  history_command=$(HISTTIMEFORMAT= builtin history 1 2>/dev/null | sed 's/^ *[0-9][0-9]* *//')
  if [ -n "$history_command" ] && [ "$history_command" != "$__koshell_last_history_command" ]; then
    __koshell_last_history_command="$history_command"
    if [ -n "$__koshell_command_active" ]; then
      __koshell_emit_end "$exit_status" "$history_command"
    else
      case "$history_command" in
        *'#?'*)
          __koshell_emit_start "$history_command"
          __koshell_emit_end "$exit_status" "$history_command"
          ;;
      esac
    fi
  fi
  __koshell_command_active=""
  __koshell_in_prompt_command=""
}
if [ -n "${PROMPT_COMMAND:-}" ]; then
  PROMPT_COMMAND="__koshell_prompt_command; $PROMPT_COMMAND"
else
  PROMPT_COMMAND="__koshell_prompt_command"
fi
trap '__koshell_debug_trap' DEBUG
"#;

const ZSH_HOOKS: &str = r#"autoload -Uz add-zsh-hook
__koshell_zsh_last_history_command=""
__koshell_zsh_command_active=""
__koshell_zsh_preexec() {
  __koshell_zsh_command_active=1
  __koshell_emit_start "$1"
}
__koshell_zsh_precmd() {
  local exit_status=$?
  local history_command
  history_command=$(fc -ln -1 2>/dev/null | sed 's/^\t//')
  if [ -n "$history_command" ] && [ "$history_command" != "$__koshell_zsh_last_history_command" ]; then
    __koshell_zsh_last_history_command="$history_command"
    if [ -n "$__koshell_zsh_command_active" ]; then
      __koshell_emit_end "$exit_status" "$history_command"
    else
      case "$history_command" in
        *'#?'*)
          __koshell_emit_start "$history_command"
          __koshell_emit_end "$exit_status" "$history_command"
          ;;
      esac
    fi
  fi
  __koshell_zsh_command_active=""
}
add-zsh-hook preexec __koshell_zsh_preexec
add-zsh-hook precmd __koshell_zsh_precmd
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_command_start_marker() {
        let marker = ShellIntegrationMarker {
            kind: MarkerKind::CommandStart,
            command: Some("ls #? explain".to_string()),
            exit_code: None,
        };
        let bytes = format_marker(&marker);
        assert!(bytes.starts_with(MARKER_PREFIX));
        assert_eq!(*bytes.last().unwrap(), MARKER_SUFFIX);
        let payload = &bytes[MARKER_PREFIX.len()..bytes.len() - 1];
        assert_eq!(parse_marker_payload(payload), Some(marker));
    }

    #[test]
    fn round_trips_command_end_marker_with_exit_code() {
        let marker = ShellIntegrationMarker {
            kind: MarkerKind::CommandEnd,
            command: Some("false".to_string()),
            exit_code: Some(1),
        };
        let bytes = format_marker(&marker);
        let payload = &bytes[MARKER_PREFIX.len()..bytes.len() - 1];
        assert_eq!(parse_marker_payload(payload), Some(marker));
    }

    #[test]
    fn rejects_invalid_payloads() {
        assert_eq!(parse_marker_payload(b"not-base64!!"), None);
        let bad = BASE64.encode(b"{\"type\":\"nope\"}");
        assert_eq!(parse_marker_payload(bad.as_bytes()), None);
    }

    #[test]
    fn scanner_separates_visible_output_and_markers() {
        let mut scanner = MarkerScanner::new();
        let marker = format_marker(&ShellIntegrationMarker {
            kind: MarkerKind::CommandStart,
            command: Some("ls".to_string()),
            exit_code: None,
        });
        let mut stream = b"before".to_vec();
        stream.extend_from_slice(&marker);
        stream.extend_from_slice(b"after");

        let segments = scanner.feed(&stream);
        assert_eq!(
            segments,
            vec![
                Segment::Visible(b"before".to_vec()),
                Segment::Marker(ShellIntegrationMarker {
                    kind: MarkerKind::CommandStart,
                    command: Some("ls".to_string()),
                    exit_code: None,
                }),
                Segment::Visible(b"after".to_vec()),
            ]
        );
    }

    #[test]
    fn scanner_handles_marker_split_across_chunks() {
        let mut scanner = MarkerScanner::new();
        let marker = format_marker(&ShellIntegrationMarker {
            kind: MarkerKind::CommandEnd,
            command: Some("ls #? explain".to_string()),
            exit_code: Some(0),
        });
        // Split the marker in the middle of the prefix.
        let split = 5;
        let mut first = b"out".to_vec();
        first.extend_from_slice(&marker[..split]);
        let second = &marker[split..];

        let mut segments = scanner.feed(&first);
        segments.extend(scanner.feed(second));

        let visible: Vec<u8> = segments
            .iter()
            .filter_map(|s| match s {
                Segment::Visible(bytes) => Some(bytes.clone()),
                Segment::Marker(_) => None,
            })
            .flatten()
            .collect();
        assert_eq!(visible, b"out");

        let markers: Vec<&ShellIntegrationMarker> = segments
            .iter()
            .filter_map(|s| match s {
                Segment::Marker(m) => Some(m),
                Segment::Visible(_) => None,
            })
            .collect();
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].command.as_deref(), Some("ls #? explain"));
    }

    #[test]
    fn other_shells_get_no_integration() {
        let config = create_shell_launch_config("/bin/sh", HashMap::new()).unwrap();
        assert_eq!(config.kind, None);
        assert!(config.args.is_empty());
    }
}

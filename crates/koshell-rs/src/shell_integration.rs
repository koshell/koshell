//! Shell integration: generate temporary bash/zsh rc files that emit OSC 777 command
//! boundary markers, and parse those markers back out of the PTY stream.
//!
//! Ported from `coshell/src/shell-integration.ts` (renamed to koshell). The rc hooks
//! source the user's own rc files first, then install `preexec`/`precmd` (zsh) or a
//! `DEBUG` trap plus `PROMPT_COMMAND` (bash). A `command_start` marker carries the full
//! typed command line (zsh: `preexec $1`; bash: read back from history inside the DEBUG
//! trap, since `$BASH_COMMAND` strips a trailing `#?` comment), emitted once per
//! accepted line; `command_end` carries the command line and exit code. The precmd
//! fallback also emits markers for comment-only lines containing `#?`, so a bare
//! `#? question` works even though the shell runs no command.
//!
//! Config fidelity (fix 0002, following VS Code's injection scheme): the zsh temp
//! `ZDOTDIR` holds three stage files (`.zshenv`, `.zprofile`, `.zshrc`) that delegate to
//! the user's real files under `$KOSHELL_USER_ZDOTDIR` — honoring a pre-existing custom
//! `ZDOTDIR`, re-capturing one set by the user's `.zshenv`, sourcing `.zprofile` for
//! login shells only, and restoring `ZDOTDIR` before the user's `.zshrc` runs so
//! everything derived from it (compdump, plugin caches, nested zsh, a login shell's
//! native `.zlogin`) lands in the real location. The bash hooks never clobber a user
//! `DEBUG` trap: with bash-preexec imported they register through its hook arrays, and
//! otherwise they chain any existing trap.

use std::collections::HashMap;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use tempfile::TempDir;

/// OSC 777 marker prefix identifying koshell command-boundary markers.
pub const MARKER_PREFIX: &[u8] = b"\x1b]777;koshell;";
/// BEL terminator for the marker.
pub const MARKER_SUFFIX: u8 = 0x07;

/// Cap on the bytes held while waiting for a marker's terminator. A real marker (even
/// one carrying a long command line) is far under this; past it, a `MARKER_PREFIX` in
/// the byte stream was not ours (program output, binary data, a file containing the
/// prefix) and its terminator may never come, so the buffered bytes are flushed as
/// literal output instead of growing the scanner buffer without bound.
const MAX_PENDING_MARKER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellIntegrationKind {
    Bash,
    Zsh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerKind {
    CommandStart,
    CommandEnd,
    /// The inner shell's working directory, reported from `precmd` on every prompt so the
    /// foreground wrapper can mirror it onto its own process (see the working-directory
    /// mirroring handling in `session.rs`). Carries `cwd`, never `command`/`exit_code`.
    Cwd,
}

/// A parsed command-boundary marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellIntegrationMarker {
    pub kind: MarkerKind,
    pub command: Option<String>,
    pub exit_code: Option<i32>,
    /// The absolute working directory, present only on [`MarkerKind::Cwd`] markers.
    pub cwd: Option<String>,
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

/// Builds a launch config for `command`, installing shell integration when its
/// basename is bash/zsh and launching every other program directly (no integration,
/// so `#?` runs through the non-integrated capture path). `extra_args` are the
/// user-supplied arguments from `koshell <command> [args...]`; for bash/zsh they are
/// appended after the integration arguments. Accepted residuals of that ordering:
/// arguments that fight the rc injection (`bash --norc`) may break marker emission,
/// and a non-interactive `bash -c '...'` never sources the rc file, so `#?` stays
/// disarmed for that session.
pub fn create_shell_launch_config(
    command: &str,
    extra_args: &[String],
    env: HashMap<String, String>,
) -> anyhow::Result<ShellLaunchConfig> {
    let command_name = Path::new(command)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    match command_name {
        "bash" => create_bash_launch_config(command, extra_args, env),
        "zsh" => create_zsh_launch_config(command, extra_args, env),
        _ => Ok(ShellLaunchConfig {
            command: command.to_string(),
            args: extra_args.to_vec(),
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
        "cwd" => MarkerKind::Cwd,
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
    let cwd = value
        .get("cwd")
        .and_then(|c| c.as_str())
        .map(str::to_string);
    Some(ShellIntegrationMarker {
        kind,
        command,
        exit_code: if kind == MarkerKind::CommandEnd {
            exit_code
        } else {
            None
        },
        cwd: if kind == MarkerKind::Cwd { cwd } else { None },
    })
}

/// Formats a marker as the on-wire OSC sequence (used by tests).
pub fn format_marker(marker: &ShellIntegrationMarker) -> Vec<u8> {
    let type_str = match marker.kind {
        MarkerKind::CommandStart => "command_start",
        MarkerKind::CommandEnd => "command_end",
        MarkerKind::Cwd => "cwd",
    };
    let mut json = serde_json::json!({ "type": type_str });
    if let Some(command) = &marker.command {
        json["command"] = serde_json::Value::String(command.clone());
    }
    if let Some(exit_code) = marker.exit_code {
        json["exitCode"] = serde_json::Value::Number(exit_code.into());
    }
    if let Some(cwd) = &marker.cwd {
        json["cwd"] = serde_json::Value::String(cwd.clone());
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
                // Incomplete marker: keep it buffered until the suffix arrives, but
                // cap the wait so a spurious prefix with a missing terminator cannot
                // grow the buffer without bound. Past the cap, flush the buffer as
                // literal output, keeping only a possible partial-prefix tail.
                self.buf.drain(..pos);
                if self.buf.len() > MAX_PENDING_MARKER_BYTES {
                    let keep = partial_prefix_len(&self.buf, MARKER_PREFIX);
                    let emit_upto = self.buf.len() - keep;
                    visible.extend_from_slice(&self.buf[..emit_upto]);
                    self.buf.drain(..emit_upto);
                }
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
    extra_args: &[String],
    env: HashMap<String, String>,
) -> anyhow::Result<ShellLaunchConfig> {
    let temp_dir = tempfile::Builder::new().prefix("koshell-bash-").tempdir()?;
    let rc_file = temp_dir.path().join("bashrc");
    let home = env.get("HOME").cloned().unwrap_or_default();
    let user_rc = format!("{home}/.bashrc");

    let contents = format!(
        "{source_rc}\n{shared}{bash_hooks}",
        source_rc = source_if_readable(&user_rc),
        shared = shared_shell_functions(),
        bash_hooks = BASH_HOOKS,
    );
    std::fs::write(&rc_file, contents)?;

    let mut args = vec![
        "--rcfile".to_string(),
        rc_file.to_string_lossy().to_string(),
        "-i".to_string(),
    ];
    args.extend_from_slice(extra_args);

    Ok(ShellLaunchConfig {
        command: shell.to_string(),
        args,
        env,
        kind: Some(ShellIntegrationKind::Bash),
        _temp_dir: Some(temp_dir),
    })
}

fn create_zsh_launch_config(
    shell: &str,
    extra_args: &[String],
    mut env: HashMap<String, String>,
) -> anyhow::Result<ShellLaunchConfig> {
    let temp_dir = tempfile::Builder::new().prefix("koshell-zsh-").tempdir()?;

    // The user's real rc directory: a pre-existing custom ZDOTDIR wins over HOME. The
    // stage files re-capture the value at runtime in case the user's .zshenv moves it.
    let user_zdotdir = env
        .get("ZDOTDIR")
        .filter(|value| !value.trim().is_empty())
        .or_else(|| env.get("HOME"))
        .cloned()
        .unwrap_or_default();

    std::fs::write(temp_dir.path().join(".zshenv"), ZSH_STAGE_ZSHENV)?;
    std::fs::write(temp_dir.path().join(".zprofile"), ZSH_STAGE_ZPROFILE)?;
    let zshrc_contents = format!(
        "{prelude}{shared}{zsh_hooks}",
        prelude = ZSH_STAGE_ZSHRC_PRELUDE,
        shared = shared_shell_functions(),
        zsh_hooks = ZSH_HOOKS,
    );
    std::fs::write(temp_dir.path().join(".zshrc"), zshrc_contents)?;

    // zsh sources rc files from ZDOTDIR; point it at our temp dir. The stage .zshrc
    // restores the user value before the user's own rc runs.
    env.insert(
        "ZDOTDIR".to_string(),
        temp_dir.path().to_string_lossy().to_string(),
    );
    env.insert("KOSHELL_USER_ZDOTDIR".to_string(), user_zdotdir);

    let mut args = vec!["-i".to_string()];
    args.extend_from_slice(extra_args);

    Ok(ShellLaunchConfig {
        command: shell.to_string(),
        args,
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
__koshell_emit_cwd() {
  local escaped_cwd
  escaped_cwd=$(__koshell_json_escape "$PWD")
  __koshell_payload='{"type":"cwd","cwd":"'"$escaped_cwd"'"}'
  printf '\033]777;koshell;%s\007' "$(__koshell_base64 "$__koshell_payload")"
}
"#;

const BASH_HOOKS: &str = r#"__koshell_last_history_number=""
__koshell_command_active=""
__koshell_in_prompt_command=""
__koshell_debug_trap() {
  if [ -n "$__koshell_in_prompt_command" ]; then
    return
  fi
  case "$BASH_COMMAND" in
    __koshell_*|PROMPT_COMMAND=*|trap\ *) return ;;
  esac
  # One start marker per accepted line, not per simple command of a compound line.
  if [ -n "$__koshell_command_active" ]; then
    return
  fi
  __koshell_command_active=1
  # Emit the full typed line: bash has already pushed it to history at this point, and
  # unlike $BASH_COMMAND the history text keeps a trailing `#?` comment, which koshell
  # needs at command START to fire on non-terminating commands. When the line added no
  # history entry (e.g. HISTCONTROL=ignorespace), fall back to $BASH_COMMAND.
  local history_line history_number history_command
  history_line=$(HISTTIMEFORMAT= builtin history 1 2>/dev/null)
  history_number=$(printf '%s' "$history_line" | sed 's/^ *\([0-9][0-9]*\).*/\1/')
  history_command=$(printf '%s' "$history_line" | sed 's/^ *[0-9][0-9]* *//')
  if [ -z "$history_number" ] || [ "$history_number" = "$__koshell_last_history_number" ]; then
    history_command="$BASH_COMMAND"
  fi
  __koshell_emit_start "$history_command"
}
__koshell_prompt_command() {
  local exit_status=$?
  __koshell_in_prompt_command=1
  # Report the working directory on every prompt so koshell mirrors the inner shell's
  # cwd onto its own process (tmux pane_current_path, OSC 7 consumers).
  __koshell_emit_cwd
  # Dedup by the history entry NUMBER, not its text: re-running an identical
  # command (e.g. asking the same `#?` question twice) advances the number and
  # is detected each time, while an empty Enter or prompt redraw adds no history
  # entry and is correctly suppressed.
  local history_line history_number history_command
  history_line=$(HISTTIMEFORMAT= builtin history 1 2>/dev/null)
  history_number=$(printf '%s' "$history_line" | sed 's/^ *\([0-9][0-9]*\).*/\1/')
  history_command=$(printf '%s' "$history_line" | sed 's/^ *[0-9][0-9]* *//')
  if [ -n "$history_number" ] && [ "$history_number" != "$__koshell_last_history_number" ]; then
    __koshell_last_history_number="$history_number"
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
if [ -n "${bash_preexec_imported:-}${__bp_imported:-}" ]; then
  # The user rc imported bash-preexec, which owns the DEBUG trap (iTerm2 integration,
  # atuin, ble.sh, ...). Register through its hook arrays instead of competing for the
  # trap: it passes the history line (trailing #? comment preserved) to preexec
  # functions and restores $? for precmd functions.
  __koshell_bp_preexec() {
    if [ -z "$__koshell_command_active" ]; then
      __koshell_command_active=1
      __koshell_emit_start "$1"
    fi
  }
  preexec_functions+=(__koshell_bp_preexec)
  precmd_functions+=(__koshell_prompt_command)
else
  if [ -n "${PROMPT_COMMAND:-}" ]; then
    PROMPT_COMMAND="__koshell_prompt_command; $PROMPT_COMMAND"
  else
    PROMPT_COMMAND="__koshell_prompt_command"
  fi
  # Chain any DEBUG trap the user rc installed instead of clobbering it (bash keeps a
  # single DEBUG trap). The eval-into-array trick preserves the trap body's quoting
  # even across newlines; term 2 of `trap -- '...' DEBUG` is the body.
  __koshell_get_debug_trap() {
    builtin local -a terms
    builtin eval "terms=( $(trap -p DEBUG) )"
    builtin printf '%s' "${terms[2]:-}"
  }
  __koshell_orig_debug_trap="$(__koshell_get_debug_trap)"
  if [ -n "$__koshell_orig_debug_trap" ]; then
    __koshell_chained_debug_trap() {
      __koshell_debug_trap
      builtin eval "$__koshell_orig_debug_trap"
    }
    trap '__koshell_chained_debug_trap' DEBUG
  else
    trap '__koshell_debug_trap' DEBUG
  fi
fi
"#;

/// Stage `.zshenv`: delegate to the user's `.zshenv`, honoring a `ZDOTDIR` the user's
/// file may itself set (re-captured into `KOSHELL_USER_ZDOTDIR` for the later stages).
const ZSH_STAGE_ZSHENV: &str = r#"# koshell zsh integration stage file (generated).
if [ -f "$KOSHELL_USER_ZDOTDIR/.zshenv" ]; then
  __koshell_stage_zdotdir="$ZDOTDIR"
  ZDOTDIR="$KOSHELL_USER_ZDOTDIR"
  if [ "$KOSHELL_USER_ZDOTDIR" != "$__koshell_stage_zdotdir" ]; then
    . "$KOSHELL_USER_ZDOTDIR/.zshenv"
  fi
  KOSHELL_USER_ZDOTDIR="$ZDOTDIR"
  ZDOTDIR="$__koshell_stage_zdotdir"
  unset __koshell_stage_zdotdir
fi
"#;

/// Stage `.zprofile`: delegate to the user's `.zprofile` for login shells only,
/// matching zsh's own sourcing rule instead of forcing login config on every session.
const ZSH_STAGE_ZPROFILE: &str = r#"# koshell zsh integration stage file (generated).
if [[ -o login ]] && [ -f "$KOSHELL_USER_ZDOTDIR/.zprofile" ]; then
  __koshell_stage_zdotdir="$ZDOTDIR"
  ZDOTDIR="$KOSHELL_USER_ZDOTDIR"
  . "$KOSHELL_USER_ZDOTDIR/.zprofile"
  ZDOTDIR="$__koshell_stage_zdotdir"
  unset __koshell_stage_zdotdir
fi
"#;

/// Stage `.zshrc` prelude: restore the user's `ZDOTDIR` before their rc runs, so
/// everything derived from it (compdump, plugin caches, nested zsh, a login shell's
/// native `.zlogin`) lands in the real location, then source the user's `.zshrc`.
/// macOS `/etc/zshrc` derives `HISTFILE` from `ZDOTDIR`, which still pointed at the
/// temp dir when it ran; point it back before the user rc (which may override it).
const ZSH_STAGE_ZSHRC_PRELUDE: &str = r#"# koshell zsh integration stage file (generated).
__koshell_stage_zdotdir="$ZDOTDIR"
ZDOTDIR="$KOSHELL_USER_ZDOTDIR"
if [ "$HISTFILE" = "$__koshell_stage_zdotdir/.zsh_history" ]; then
  HISTFILE="$ZDOTDIR/.zsh_history"
fi
unset __koshell_stage_zdotdir
if [ -f "$ZDOTDIR/.zshrc" ]; then
  . "$ZDOTDIR/.zshrc"
fi
unset KOSHELL_USER_ZDOTDIR
"#;

const ZSH_HOOKS: &str = r#"autoload -Uz add-zsh-hook
__koshell_zsh_command_active=""
__koshell_zsh_current_command=""
__koshell_zsh_pending_line=""
__koshell_zsh_has_orig_accept_line=""
# Capture the exact submitted line at Enter time via an accept-line wrapper. This is the
# only signal that fires for a comment-only `#?` line, and it never consults history, so it
# is immune to hist_ignore_dups / hist_ignore_space / hist_save_no_dups collapsing repeats.
# Delegate to any pre-existing accept-line widget so plugin wrappers still run.
case "${widgets[accept-line]}" in
  user:*)
    zle -A accept-line __koshell_zsh_orig_accept_line
    __koshell_zsh_has_orig_accept_line=1
    ;;
esac
__koshell_zsh_accept_line() {
  __koshell_zsh_pending_line="$BUFFER"
  if [ -n "$__koshell_zsh_has_orig_accept_line" ]; then
    zle __koshell_zsh_orig_accept_line
  else
    zle .accept-line
  fi
}
zle -N accept-line __koshell_zsh_accept_line
__koshell_zsh_preexec() {
  __koshell_zsh_command_active=1
  __koshell_zsh_current_command="$1"
  # A real command ran for this line; the comment fallback must not fire for it too.
  __koshell_zsh_pending_line=""
  __koshell_emit_start "$1"
}
__koshell_zsh_precmd() {
  local exit_status=$?
  # Report the working directory on every prompt so koshell mirrors the inner shell's
  # cwd onto its own process (tmux pane_current_path, OSC 7 consumers).
  __koshell_emit_cwd
  if [ -n "$__koshell_zsh_command_active" ]; then
    __koshell_emit_end "$exit_status" "$__koshell_zsh_current_command"
  else
    case "$__koshell_zsh_pending_line" in
      *'#?'*)
        __koshell_emit_start "$__koshell_zsh_pending_line"
        __koshell_emit_end "$exit_status" "$__koshell_zsh_pending_line"
        ;;
    esac
  fi
  # Clear per-line state so a prompt redraw (e.g. resize) without a new Enter never re-fires.
  __koshell_zsh_command_active=""
  __koshell_zsh_current_command=""
  __koshell_zsh_pending_line=""
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
            cwd: None,
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
            cwd: None,
        };
        let bytes = format_marker(&marker);
        let payload = &bytes[MARKER_PREFIX.len()..bytes.len() - 1];
        assert_eq!(parse_marker_payload(payload), Some(marker));
    }

    #[test]
    fn round_trips_cwd_marker() {
        let marker = ShellIntegrationMarker {
            kind: MarkerKind::Cwd,
            command: None,
            exit_code: None,
            cwd: Some("/home/user/project".to_string()),
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
            cwd: None,
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
                    cwd: None,
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
            cwd: None,
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
    fn incomplete_marker_buffer_is_capped_against_a_spurious_prefix() {
        let mut scanner = MarkerScanner::new();
        // A marker prefix that program output happened to emit, followed by a large
        // blob with no BEL terminator — the terminator may never come.
        let mut stream = MARKER_PREFIX.to_vec();
        stream.extend(std::iter::repeat_n(b'x', MAX_PENDING_MARKER_BYTES + 1_024));
        let segments = scanner.feed(&stream);

        // The bytes are flushed as literal output rather than held forever, and no
        // marker is fabricated from the unterminated prefix.
        let visible_len: usize = segments
            .iter()
            .map(|s| match s {
                Segment::Visible(bytes) => bytes.len(),
                Segment::Marker(_) => 0,
            })
            .sum();
        assert!(
            segments.iter().all(|s| matches!(s, Segment::Visible(_))),
            "no marker fabricated from an unterminated prefix"
        );
        assert!(
            visible_len >= MAX_PENDING_MARKER_BYTES,
            "buffered bytes flushed as visible output, got {visible_len}"
        );
        // The pending buffer did not grow without bound.
        assert!(
            scanner.buf.len() <= MARKER_PREFIX.len(),
            "scanner buffer stayed bounded, got {}",
            scanner.buf.len()
        );
    }

    #[test]
    fn other_shells_get_no_integration() {
        let config = create_shell_launch_config("/bin/sh", &[], HashMap::new()).unwrap();
        assert_eq!(config.kind, None);
        assert!(config.args.is_empty());
    }

    #[test]
    fn direct_command_keeps_user_arguments_verbatim() {
        let args = vec!["-i".to_string(), "--flag".to_string()];
        let config = create_shell_launch_config("/usr/bin/python3", &args, HashMap::new()).unwrap();
        assert_eq!(config.kind, None);
        assert_eq!(config.command, "/usr/bin/python3");
        assert_eq!(config.args, args);
    }

    #[test]
    fn explicit_shell_appends_user_arguments_after_integration() {
        let args = vec!["-l".to_string()];
        let config = create_shell_launch_config("/bin/zsh", &args, HashMap::new()).unwrap();
        assert_eq!(config.kind, Some(ShellIntegrationKind::Zsh));
        assert_eq!(config.args, ["-i", "-l"]);
    }

    #[test]
    fn zsh_integration_preserves_custom_zdotdir_and_writes_stage_files() {
        let mut env = HashMap::new();
        env.insert("HOME".to_string(), "/home/user".to_string());
        env.insert("ZDOTDIR".to_string(), "/home/user/cfg/zsh".to_string());
        let config = create_shell_launch_config("/bin/zsh", &[], env).unwrap();

        assert_eq!(
            config.env.get("KOSHELL_USER_ZDOTDIR").map(String::as_str),
            Some("/home/user/cfg/zsh")
        );
        let zdotdir = config.env.get("ZDOTDIR").expect("ZDOTDIR set");
        assert_ne!(zdotdir, "/home/user/cfg/zsh");
        for stage in [".zshenv", ".zprofile", ".zshrc"] {
            assert!(
                Path::new(zdotdir).join(stage).is_file(),
                "missing stage file {stage}"
            );
        }
    }

    #[test]
    fn zsh_integration_falls_back_to_home_for_user_zdotdir() {
        let mut env = HashMap::new();
        env.insert("HOME".to_string(), "/home/user".to_string());
        let config = create_shell_launch_config("/bin/zsh", &[], env).unwrap();
        assert_eq!(
            config.env.get("KOSHELL_USER_ZDOTDIR").map(String::as_str),
            Some("/home/user")
        );
    }

    /// Syntax-checks a generated rc file with `interpreter -n`; skips when the
    /// interpreter is not installed.
    fn assert_rc_parses(candidates: &[&str], rc_path: &Path) {
        let Some(interpreter) = candidates.iter().find(|c| Path::new(c).exists()) else {
            eprintln!("skipping syntax check: none of {candidates:?} found");
            return;
        };
        let status = std::process::Command::new(interpreter)
            .arg("-n")
            .arg(rc_path)
            .status()
            .expect("run syntax check");
        assert!(status.success(), "{interpreter} -n rejected {rc_path:?}");
    }

    #[test]
    fn generated_zsh_stage_files_are_valid_zsh() {
        let mut env = HashMap::new();
        env.insert("HOME".to_string(), "/home/user".to_string());
        let config = create_shell_launch_config("/bin/zsh", &[], env).unwrap();
        let zdotdir = config.env.get("ZDOTDIR").expect("ZDOTDIR set");
        for stage in [".zshenv", ".zprofile", ".zshrc"] {
            assert_rc_parses(
                &["/bin/zsh", "/usr/bin/zsh", "/opt/homebrew/bin/zsh"],
                &Path::new(zdotdir).join(stage),
            );
        }
    }

    #[test]
    fn generated_bash_rc_is_valid_bash() {
        let mut env = HashMap::new();
        env.insert("HOME".to_string(), "/home/user".to_string());
        let config = create_shell_launch_config("/bin/bash", &[], env).unwrap();
        let rc_path = &config.args[1];
        assert_rc_parses(
            &["/bin/bash", "/usr/bin/bash", "/opt/homebrew/bin/bash"],
            Path::new(rc_path),
        );
    }
}

//! Shell resolution, PTY environment construction, and nested-start prevention.
//!
//! Ports the algorithms from the frozen `reference/src/shell.ts` so behavior stays
//! identical: `SHELL`-first resolution with a fixed fallback list, a system-only
//! fallback `PATH`, and a tty-scoped `KOSHELL_TTY` marker (with a flat `KOSHELL`
//! fallback) that blocks running koshell inside koshell on the same terminal while
//! letting a shell on a fresh tty — a new tmux pane — wrap itself.

use std::collections::HashMap;
use std::ffi::CString;
use std::path::{Path, PathBuf};

/// Ordered fallback shells, tried when `SHELL` is unset or unresolvable.
const FALLBACK_SHELLS: [&str; 6] = [
    "/bin/zsh",
    "/bin/bash",
    "/bin/sh",
    "/usr/bin/zsh",
    "/usr/bin/bash",
    "/usr/bin/sh",
];

/// System-only fallback `PATH`, used when the source `PATH` is empty or whitespace.
const FALLBACK_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// Public "am I inside koshell" marker, kept for user scripts and as the recursion
/// fallback when the tty-scoped marker below is unavailable.
const KOSHELL_ENV_KEY: &str = "KOSHELL";
const KOSHELL_ENV_VALUE: &str = "1";

/// Primary, tty-scoped recursion marker: the controlling-tty path of the shell koshell
/// wrapped. A descendant recognizes "already wrapped" only when its own controlling tty
/// equals this value, so a shell that inherits the marker across a tty boundary (a new
/// tmux pane, a fresh pts) re-wraps instead of staying inert. Set in `session.rs` once
/// the child pts is known; see [`is_nested_koshell`].
pub const KOSHELL_TTY_ENV_KEY: &str = "KOSHELL_TTY";

/// Path of the liveness marker file for [`KOSHELL_TTY_ENV_KEY`]. koshell writes its own
/// PID there and removes it on exit; a shell (or koshell) that inherits a `KOSHELL_TTY`
/// matching its own tty consults this to tell a *live* wrapping koshell from a *stale*
/// brand — a tmux pane on a recycled pts whose original koshell has died. See
/// [`tty_marker_is_live`] and [`register_tty_marker`].
pub const KOSHELL_TTY_MARKER_ENV_KEY: &str = "KOSHELL_TTY_MARKER";

/// The `SHELL` environment variable key.
const SHELL_ENV_KEY: &str = "SHELL";

/// Returns true when `path` is executable for the current process (`access(X_OK)`).
fn is_executable(path: &str) -> bool {
    let Ok(c_path) = CString::new(path) else {
        return false;
    };
    // Safety: `c_path` is a valid NUL-terminated C string for the duration of the call.
    unsafe { libc::access(c_path.as_ptr(), libc::X_OK) == 0 }
}

/// Resolves `command` to an executable path, mirroring `reference` semantics:
/// absolute paths are checked directly; bare names are looked up against `path_value`
/// (falling back to [`FALLBACK_PATH`] when empty/whitespace).
fn resolve_executable(command: &str, path_value: Option<&str>) -> Option<String> {
    if Path::new(command).is_absolute() {
        return is_executable(command).then(|| command.to_string());
    }

    let paths = match path_value {
        Some(value) if !value.trim().is_empty() => value,
        _ => FALLBACK_PATH,
    };

    for dir in paths.split(':') {
        let candidate = Path::new(dir).join(command);
        let candidate = candidate.to_string_lossy().to_string();
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }

    None
}

/// Resolves which shell binary to launch from the given environment.
pub fn resolve_shell(env: &HashMap<String, String>) -> anyhow::Result<String> {
    if let Some(configured) = env.get(SHELL_ENV_KEY) {
        let trimmed = configured.trim();
        if !trimmed.is_empty()
            && let Some(resolved) = resolve_executable(trimmed, env.get("PATH").map(String::as_str))
        {
            return Ok(resolved);
        }
    }

    for candidate in FALLBACK_SHELLS {
        if is_executable(candidate) {
            return Ok(candidate.to_string());
        }
    }

    let described = match env.get(SHELL_ENV_KEY) {
        Some(value) => format!("{value:?}"),
        None => "unset or empty".to_string(),
    };
    anyhow::bail!("No executable shell found. SHELL was {described}.");
}

/// Resolves an explicitly requested command (`koshell <command> ...`) to an executable
/// path using the same lookup rules as shell resolution.
pub fn resolve_command(command: &str, env: &HashMap<String, String>) -> anyhow::Result<String> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        anyhow::bail!("Empty command.");
    }
    resolve_executable(trimmed, env.get("PATH").map(String::as_str))
        .ok_or_else(|| anyhow::anyhow!("Command not found: {trimmed}"))
}

/// Builds the child PTY environment: copies the source env, forces the `KOSHELL`
/// marker, and normalizes an empty/whitespace `PATH` to [`FALLBACK_PATH`].
pub fn create_pty_env(source: &HashMap<String, String>) -> HashMap<String, String> {
    let mut env = source.clone();
    env.insert(KOSHELL_ENV_KEY.to_string(), KOSHELL_ENV_VALUE.to_string());

    let path = match source.get("PATH") {
        Some(value) if !value.trim().is_empty() => value.clone(),
        _ => FALLBACK_PATH.to_string(),
    };
    env.insert("PATH".to_string(), path);

    env
}

/// The path of this process's controlling terminal (`ttyname(0)`), matching what a
/// shell reports via `$(tty)` and the pts a child is branded with. `None` when stdin is
/// not a terminal (in which case there is no tty to collide with anyway).
pub fn controlling_tty() -> Option<String> {
    use std::os::unix::io::AsRawFd;
    let fd = std::io::stdin().as_raw_fd();
    let mut buf = [0u8; 256];
    // Safety: `buf` is a valid, owned byte buffer for the duration of the call, and
    // `ttyname_r` writes at most `buf.len()` bytes including the NUL terminator.
    let rc = unsafe { libc::ttyname_r(fd, buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return None;
    }
    // Safety: on success `ttyname_r` left a NUL-terminated C string in `buf`.
    let cstr = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr() as *const libc::c_char) };
    cstr.to_str().ok().map(str::to_string)
}

/// Returns true when the liveness marker at `marker_path` names a process that is still
/// alive (`kill(pid, 0)`). Missing path, unreadable/garbage contents, or a dead/foreign
/// pid all read as not-live, which is the safe default (treat the brand as stale → wrap).
pub fn tty_marker_is_live(marker_path: Option<&str>) -> bool {
    let Some(path) = marker_path else {
        return false;
    };
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(pid) = contents.trim().parse::<libc::pid_t>() else {
        return false;
    };
    // Safety: `kill` with signal 0 performs an existence/permission check and delivers no
    // signal. A reused pid owned by another user returns EPERM (non-zero) and reads as
    // not-live, the safe direction.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Returns true when this process is a genuine same-terminal nested koshell launch.
///
/// Primary rule (tty-scoped, liveness-gated): when `KOSHELL_TTY` is set, nested iff it
/// equals this process's controlling tty **and** a live koshell still owns that tty
/// (`marker_live`). A marker inherited across a tty boundary (a new tmux pane on a fresh
/// pts) names a *different* tty; a marker inherited onto a *recycled* pts whose original
/// koshell has died fails the liveness gate. Either way the shell is not nested and wraps.
///
/// Fallback (`KOSHELL_TTY` unset, e.g. the child pts could not be resolved): fall back to
/// the coarse flat `KOSHELL=1` marker so recursion is still broken.
///
/// `marker_live` is the result of [`tty_marker_is_live`] for the inherited
/// [`KOSHELL_TTY_MARKER_ENV_KEY`]; it is only consulted on the tty-match branch.
pub fn is_nested_koshell(
    env: &HashMap<String, String>,
    current_tty: Option<&str>,
    marker_live: bool,
) -> bool {
    match env.get(KOSHELL_TTY_ENV_KEY).map(String::as_str) {
        Some(marked_tty) if !marked_tty.is_empty() => {
            current_tty == Some(marked_tty) && marker_live
        }
        _ => env.get(KOSHELL_ENV_KEY).map(String::as_str) == Some(KOSHELL_ENV_VALUE),
    }
}

/// Fails when koshell is being launched from inside a *live* koshell on the same terminal.
/// `current_tty` is this process's controlling tty (see [`controlling_tty`]); the liveness
/// gate is read from the inherited [`KOSHELL_TTY_MARKER_ENV_KEY`].
pub fn assert_not_nested_koshell(
    env: &HashMap<String, String>,
    current_tty: Option<&str>,
) -> anyhow::Result<()> {
    let marker_live = tty_marker_is_live(env.get(KOSHELL_TTY_MARKER_ENV_KEY).map(String::as_str));
    if is_nested_koshell(env, current_tty, marker_live) {
        anyhow::bail!(
            "koshell is already running in this shell. Start a new regular terminal session before launching koshell again."
        );
    }
    Ok(())
}

/// A live-koshell liveness marker: a file named by the wrapped tty holding this koshell's
/// PID. Removed on drop (normal exit and unwind); a hard crash (`SIGKILL`) leaks the file,
/// but the pid it names is then dead, so [`tty_marker_is_live`] still reports not-live.
pub struct TtyMarker {
    path: PathBuf,
}

impl TtyMarker {
    /// The marker file path, to export as [`KOSHELL_TTY_MARKER_ENV_KEY`].
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TtyMarker {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Writes the liveness marker for the wrapped `tty` (this koshell's PID) and returns a
/// guard that removes it on drop. Best-effort: returns `None` when no runtime directory is
/// usable or the write fails — its absence only reopens the rare recycled-pts residual, it
/// never breaks correctness.
pub fn register_tty_marker(tty: &str) -> Option<TtyMarker> {
    let dir = crate::ipc::runtime_dir().join("tty");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(tty.replace('/', "_"));
    std::fs::write(&path, std::process::id().to_string()).ok()?;
    Some(TtyMarker { path })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn create_pty_env_sets_marker_and_keeps_empty_values() {
        let source = env_of(&[("PATH", "/custom/bin"), ("EMPTY", ""), ("FOO", "bar")]);
        let env = create_pty_env(&source);
        assert_eq!(env.get("KOSHELL").map(String::as_str), Some("1"));
        assert_eq!(env.get("PATH").map(String::as_str), Some("/custom/bin"));
        assert_eq!(env.get("EMPTY").map(String::as_str), Some(""));
        assert_eq!(env.get("FOO").map(String::as_str), Some("bar"));
    }

    #[test]
    fn create_pty_env_replaces_empty_path_with_fallback() {
        let env = create_pty_env(&env_of(&[("PATH", "   ")]));
        assert_eq!(env.get("PATH").map(String::as_str), Some(FALLBACK_PATH));

        let env_missing = create_pty_env(&env_of(&[]));
        assert_eq!(
            env_missing.get("PATH").map(String::as_str),
            Some(FALLBACK_PATH)
        );
    }

    #[test]
    fn nested_detection() {
        let tty = Some("/dev/pts/3");
        let live = true;
        let dead = false;

        // Fallback (KOSHELL_TTY absent): coarse flat KOSHELL marker; liveness ignored.
        assert!(!is_nested_koshell(&env_of(&[]), tty, dead));
        assert!(!is_nested_koshell(&env_of(&[("KOSHELL", "")]), tty, dead));
        assert!(is_nested_koshell(&env_of(&[("KOSHELL", "1")]), tty, dead));
        assert!(is_nested_koshell(&env_of(&[("KOSHELL", "1")]), None, dead));

        // Primary (KOSHELL_TTY present): tty-scoped AND liveness-gated, ignores KOSHELL.
        // Same tty + live koshell → nested (the inner shell / a subshell).
        assert!(is_nested_koshell(
            &env_of(&[("KOSHELL_TTY", "/dev/pts/3")]),
            tty,
            live
        ));
        // Same tty but a *dead* marker (recycled pts) → not nested → wraps.
        assert!(!is_nested_koshell(
            &env_of(&[("KOSHELL_TTY", "/dev/pts/3")]),
            tty,
            dead
        ));
        // A tmux pane inherits both markers but runs on a different tty → not nested,
        // regardless of liveness.
        assert!(!is_nested_koshell(
            &env_of(&[("KOSHELL_TTY", "/dev/pts/3"), ("KOSHELL", "1")]),
            Some("/dev/pts/8"),
            live
        ));
        // No current tty can never match a branded tty.
        assert!(!is_nested_koshell(
            &env_of(&[("KOSHELL_TTY", "/dev/pts/3")]),
            None,
            live
        ));
        // An empty KOSHELL_TTY is treated as absent (falls back to KOSHELL).
        assert!(!is_nested_koshell(
            &env_of(&[("KOSHELL_TTY", "")]),
            tty,
            live
        ));
    }

    #[test]
    fn tty_marker_liveness() {
        let dir = tempfile::tempdir().expect("temp dir");

        // A marker naming our own (live) pid reads as live.
        let live = dir.path().join("live");
        std::fs::write(&live, std::process::id().to_string()).expect("write live marker");
        assert!(tty_marker_is_live(Some(live.to_str().unwrap())));

        // A pid past any pid_max, garbage, empty, and missing all read as not-live.
        let dead = dir.path().join("dead");
        std::fs::write(&dead, "2147483646").expect("write dead marker");
        assert!(!tty_marker_is_live(Some(dead.to_str().unwrap())));
        let garbage = dir.path().join("garbage");
        std::fs::write(&garbage, "not-a-pid").expect("write garbage marker");
        assert!(!tty_marker_is_live(Some(garbage.to_str().unwrap())));
        assert!(!tty_marker_is_live(Some(
            dir.path().join("missing").to_str().unwrap()
        )));
        assert!(!tty_marker_is_live(None));
    }

    #[test]
    fn assert_not_nested_uses_liveness() {
        // Flat fallback still trips regardless of tty.
        assert!(
            assert_not_nested_koshell(&env_of(&[("KOSHELL", "1")]), Some("/dev/pts/3")).is_err()
        );
        // Different tty is never nested.
        assert!(
            assert_not_nested_koshell(
                &env_of(&[("KOSHELL_TTY", "/dev/pts/3")]),
                Some("/dev/pts/8")
            )
            .is_ok()
        );

        // Same-tty brand with a live marker trips; with a dead/missing marker it does not.
        let dir = tempfile::tempdir().expect("temp dir");
        let live = dir.path().join("live");
        std::fs::write(&live, std::process::id().to_string()).expect("write live marker");
        let live = live.to_str().unwrap().to_string();
        assert!(
            assert_not_nested_koshell(
                &env_of(&[("KOSHELL_TTY", "/dev/pts/3"), ("KOSHELL_TTY_MARKER", &live)]),
                Some("/dev/pts/3")
            )
            .is_err()
        );
        assert!(
            assert_not_nested_koshell(
                &env_of(&[
                    ("KOSHELL_TTY", "/dev/pts/3"),
                    ("KOSHELL_TTY_MARKER", "/nonexistent")
                ]),
                Some("/dev/pts/3")
            )
            .is_ok()
        );
        assert!(assert_not_nested_koshell(&env_of(&[]), Some("/dev/pts/3")).is_ok());
    }

    #[test]
    fn resolve_executable_absolute_and_relative() {
        // /bin/sh exists and is executable on macOS and Linux.
        assert_eq!(
            resolve_executable("/bin/sh", None).as_deref(),
            Some("/bin/sh")
        );
        assert_eq!(resolve_executable("/bin/definitely-not-here", None), None);
        assert_eq!(
            resolve_executable("sh", Some("/bin")).as_deref(),
            Some("/bin/sh")
        );
    }

    #[test]
    fn resolve_command_resolves_and_reports_missing() {
        assert_eq!(resolve_command("/bin/sh", &env_of(&[])).unwrap(), "/bin/sh");
        assert_eq!(
            resolve_command("sh", &env_of(&[("PATH", "/bin")])).unwrap(),
            "/bin/sh"
        );
        let error = resolve_command("definitely-not-here", &env_of(&[])).unwrap_err();
        assert!(error.to_string().contains("definitely-not-here"));
        assert!(resolve_command("   ", &env_of(&[])).is_err());
    }

    #[test]
    fn resolve_shell_prefers_configured_then_fallback() {
        assert_eq!(
            resolve_shell(&env_of(&[("SHELL", "/bin/sh")])).unwrap(),
            "/bin/sh"
        );
        // Empty SHELL falls back to the first available fallback shell.
        let resolved = resolve_shell(&env_of(&[("SHELL", "")])).unwrap();
        assert!(FALLBACK_SHELLS.contains(&resolved.as_str()));
    }
}

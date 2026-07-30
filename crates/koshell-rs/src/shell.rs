//! Shell resolution, PTY environment construction, and nested-start prevention.
//!
//! Ports the algorithms from the frozen `reference/src/shell.ts` so behavior stays
//! identical: `SHELL`-first resolution with a fixed fallback list, a system-only
//! fallback `PATH`, and a single tmux-style `KOSHELL` identity marker that blocks
//! running koshell inside koshell on the same terminal while letting a shell on a
//! fresh tty — a new tmux pane — wrap itself.
//!
//! `KOSHELL` follows tmux's `TMUX=<socket>,<pid>,<session>` convention: it carries the
//! information needed to identify the current koshell environment as comma-separated
//! fields, `<session-id>,<tty>`. Field 0 (`koshell-<pid>`) is always present — it is
//! the routing address child `koshell status`/`reload` use, and its mere presence is
//! the public "am I inside koshell" signal and the coarse recursion fallback. Field 1
//! (the wrapped controlling tty) is present only when the child pts was resolved and
//! its liveness marker written; its absence falls back to the coarse guard. The
//! liveness marker's path is not carried in the variable — both this crate and the
//! shell snippet derive it from the tty by the same convention (see
//! [`tty_marker_path`]).

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

/// The single koshell identity variable (tmux-style `<session-id>,<tty>`). Always set
/// inside a koshell (at least field 0), so its presence is the public "am I inside
/// koshell" signal and the coarse recursion fallback; see the module docs and
/// [`is_nested_koshell`].
pub const KOSHELL_ENV_KEY: &str = "KOSHELL";

/// The `SHELL` environment variable key.
const SHELL_ENV_KEY: &str = "SHELL";

/// Field 0 of a `KOSHELL` value: the session id (`koshell-<pid>`), or `None` when the
/// value is empty. Never empty inside a real koshell — the field is always branded.
pub fn koshell_session_id(value: &str) -> Option<&str> {
    let id = value.split(',').next().unwrap_or("");
    (!id.is_empty()).then_some(id)
}

/// Field 1 of a `KOSHELL` value: the wrapped controlling tty, or `None` when absent or
/// empty (the coarse-fallback form, where koshell could not brand the child pts). tty
/// paths never contain a comma, so a plain [`str::split_once`] recovers the field.
pub fn koshell_tty(value: &str) -> Option<&str> {
    value
        .split_once(',')
        .map(|(_, tty)| tty)
        .filter(|tty| !tty.is_empty())
}

/// Builds a `KOSHELL` value branding `session_id` with the wrapped `tty`
/// (`<session-id>,<tty>`).
pub fn koshell_env_value(session_id: &str, tty: &str) -> String {
    format!("{session_id},{tty}")
}

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

/// Builds the child PTY environment: copies the source env, sets the always-present
/// base `KOSHELL` marker (the session id — field 0), and normalizes an empty/whitespace
/// `PATH` to [`FALLBACK_PATH`]. `session.rs` upgrades `KOSHELL` to `<session-id>,<tty>`
/// once the child pts is known and its liveness marker written; if that branding is
/// skipped, this base value keeps the coarse recursion guard working.
pub fn create_pty_env(source: &HashMap<String, String>) -> HashMap<String, String> {
    let mut env = source.clone();
    env.insert(KOSHELL_ENV_KEY.to_string(), crate::ipc::session_id());

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

/// The liveness marker file path for a wrapped `tty`, by convention rather than carried
/// in the environment: `<runtime_dir>/tty/<tty-with-slashes-escaped>`. Both this crate
/// and the shell auto-wrap snippet derive it identically (the snippet reconstructs
/// [`crate::ipc::runtime_dir`]'s XDG precedence and the `/`→`_` escape inline), so the
/// path never needs a second `KOSHELL` field. See [`register_tty_marker`].
pub fn tty_marker_path(tty: &str) -> PathBuf {
    crate::ipc::runtime_dir()
        .join("tty")
        .join(tty.replace('/', "_"))
}

/// Returns true when the liveness marker for `tty` names a process that is still alive.
/// Convenience over [`tty_marker_is_live`] that resolves the conventional path first.
pub fn tty_is_live(tty: &str) -> bool {
    tty_owner_pid(tty).is_some()
}

/// The pid of the *live* koshell that wrapped `tty`, from its conventional marker file.
/// `None` when no koshell owns the tty any more, by any of the routes
/// [`tty_marker_is_live`] treats as not-live.
pub fn tty_owner_pid(tty: &str) -> Option<libc::pid_t> {
    tty_marker_owner_pid(&tty_marker_path(tty).to_string_lossy())
}

/// Returns true when the liveness marker at `marker_path` names a process that is still
/// alive (`kill(pid, 0)`). Missing path, unreadable/garbage contents, or a dead/foreign
/// pid all read as not-live, which is the safe default (treat the brand as stale → wrap).
pub fn tty_marker_is_live(marker_path: Option<&str>) -> bool {
    marker_path.and_then(tty_marker_owner_pid).is_some()
}

/// The live pid recorded in the marker file at `marker_path`, or `None` when the file is
/// missing, unreadable, not a pid, or names a process that is gone.
pub fn tty_marker_owner_pid(marker_path: &str) -> Option<libc::pid_t> {
    let pid = std::fs::read_to_string(marker_path)
        .ok()?
        .trim()
        .parse::<libc::pid_t>()
        .ok()?;
    // Safety: `kill` with signal 0 performs an existence/permission check and delivers no
    // signal. A reused pid owned by another user returns EPERM (non-zero) and reads as
    // not-live, the safe direction.
    (unsafe { libc::kill(pid, 0) == 0 }).then_some(pid)
}

/// Returns true when this process is a genuine same-terminal nested koshell launch.
///
/// Primary rule (tty-scoped, liveness-gated): when `KOSHELL`'s tty field is set, nested
/// iff it equals this process's controlling tty **and** a live koshell still owns that
/// tty (`marker_live`). A brand inherited across a tty boundary (a new tmux pane on a
/// fresh pts) names a *different* tty; a brand inherited onto a *recycled* pts whose
/// original koshell has died fails the liveness gate. Either way the shell is not nested
/// and wraps.
///
/// Fallback (`KOSHELL`'s tty field absent, e.g. the child pts could not be resolved):
/// fall back to the mere presence of `KOSHELL` (field 0) so recursion is still broken.
///
/// `marker_live` is the result of [`tty_is_live`] for the tty in `KOSHELL`; it is only
/// consulted on the tty-match branch.
pub fn is_nested_koshell(
    env: &HashMap<String, String>,
    current_tty: Option<&str>,
    marker_live: bool,
) -> bool {
    match env.get(KOSHELL_ENV_KEY).map(String::as_str) {
        Some(value) if !value.is_empty() => match koshell_tty(value) {
            Some(marked_tty) => current_tty == Some(marked_tty) && marker_live,
            None => true,
        },
        _ => false,
    }
}

/// A refused nested launch: koshell was started from inside a live koshell on the same
/// terminal (fix 0011).
///
/// A typed error rather than a string because the recovery differs from every other
/// startup failure. The generic fail-open (`exec` a bare shell so an `exec koshell` that
/// died cannot close the terminal) is *wrong* here whenever the shell that ran us is still
/// waiting for us to exit: it would replace this process with an extra nested shell the
/// user never asked for — no shell integration, one more `exit` to get out, and the
/// wrapping koshell left holding a command span that never ends. Refusing and exiting
/// leaves the caller's shell exactly where it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NestedLaunch {
    /// The live koshell that owns this terminal, when the brand named a tty whose marker
    /// could be read. `None` on the coarse-fallback path, where nothing can be proven
    /// about who is above us.
    pub owner_pid: Option<libc::pid_t>,
    /// Whether this process took the place of the shell that would otherwise be running
    /// here — an `exec koshell` (the auto-wrap snippet, or a hand-typed one) rather than a
    /// plain `koshell`. Then there is no shell left to return to and the fail-open is the
    /// only thing keeping the terminal alive.
    pub replaced_the_shell: bool,
}

impl NestedLaunch {
    /// Decides `replaced_the_shell` from the owning koshell's pid and this process's
    /// parent pid.
    ///
    /// The discriminator is exact rather than heuristic: the wrapping koshell spawns the
    /// inner shell as its direct child, so a plain `koshell` typed at that shell's prompt
    /// has the *shell* as its parent, while an `exec koshell` replaced the shell's process
    /// image and therefore inherits its parent — the wrapping koshell named by the tty
    /// marker. Without an owner pid (the coarse fallback, where koshell could not brand
    /// the child pts) nothing is provable, so this reports `true` and keeps the
    /// terminal-preserving behavior.
    pub fn detect(owner_pid: Option<libc::pid_t>, parent_pid: libc::pid_t) -> Self {
        Self {
            owner_pid,
            replaced_the_shell: owner_pid.is_none_or(|owner| owner == parent_pid),
        }
    }
}

impl std::fmt::Display for NestedLaunch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.owner_pid {
            Some(pid) => write!(
                f,
                "koshell already wraps this terminal (pid {pid}), so this shell is \
                 already inside it — `#?` works here, and `koshell status` reports the \
                 session. Open a new terminal for a separate koshell."
            ),
            None => write!(
                f,
                "koshell is already running in this shell. Start a new regular terminal \
                 session before launching koshell again."
            ),
        }
    }
}

impl std::error::Error for NestedLaunch {}

/// Fails when koshell is being launched from inside a *live* koshell on the same terminal.
/// `current_tty` is this process's controlling tty (see [`controlling_tty`]); the liveness
/// gate is derived from the tty branded into the inherited `KOSHELL`. The error is a typed
/// [`NestedLaunch`] so the caller can tell this refusal apart from a real startup failure —
/// see its docs for why the fail-open must not fire here.
pub fn assert_not_nested_koshell(
    env: &HashMap<String, String>,
    current_tty: Option<&str>,
) -> anyhow::Result<()> {
    let owner_pid = env
        .get(KOSHELL_ENV_KEY)
        .and_then(|value| koshell_tty(value))
        .and_then(tty_owner_pid);
    if is_nested_koshell(env, current_tty, owner_pid.is_some()) {
        // Safety: `getppid` reads this process's parent pid and cannot fail.
        let parent_pid = unsafe { libc::getppid() };
        return Err(NestedLaunch::detect(owner_pid, parent_pid).into());
    }
    Ok(())
}

/// A live-koshell liveness marker: a file named by the wrapped tty holding this koshell's
/// PID. Removed on drop (normal exit and unwind); a hard crash (`SIGKILL`) leaks the file,
/// but the pid it names is then dead, so [`tty_marker_is_live`] still reports not-live.
pub struct TtyMarker {
    path: PathBuf,
}

impl Drop for TtyMarker {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Writes the liveness marker for the wrapped `tty` (this koshell's PID) at its
/// conventional [`tty_marker_path`] and returns a guard that removes it on drop.
/// Best-effort: returns `None` when no runtime directory is usable or the write fails —
/// its absence only reopens the rare recycled-pts residual, it never breaks correctness.
pub fn register_tty_marker(tty: &str) -> Option<TtyMarker> {
    let path = tty_marker_path(tty);
    std::fs::create_dir_all(path.parent()?).ok()?;
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
        // The base marker is the session id (field 0); no tty field yet — session.rs
        // adds it once the child pts is known.
        assert_eq!(
            env.get("KOSHELL").map(String::as_str),
            Some(crate::ipc::session_id().as_str())
        );
        assert!(
            !env["KOSHELL"].contains(','),
            "no tty field in the base value"
        );
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

        // Fallback (KOSHELL has no tty field): mere presence is nested; liveness ignored.
        assert!(!is_nested_koshell(&env_of(&[]), tty, dead));
        assert!(!is_nested_koshell(&env_of(&[("KOSHELL", "")]), tty, dead));
        assert!(is_nested_koshell(
            &env_of(&[("KOSHELL", "koshell-1")]),
            tty,
            dead
        ));
        assert!(is_nested_koshell(
            &env_of(&[("KOSHELL", "koshell-1")]),
            None,
            dead
        ));
        // A comma-less legacy value is still a valid presence marker.
        assert!(is_nested_koshell(&env_of(&[("KOSHELL", "1")]), tty, dead));

        // Primary (KOSHELL carries a tty field): tty-scoped AND liveness-gated.
        // Same tty + live koshell → nested (the inner shell / a subshell).
        assert!(is_nested_koshell(
            &env_of(&[("KOSHELL", "koshell-1,/dev/pts/3")]),
            tty,
            live
        ));
        // Same tty but a *dead* marker (recycled pts) → not nested → wraps.
        assert!(!is_nested_koshell(
            &env_of(&[("KOSHELL", "koshell-1,/dev/pts/3")]),
            tty,
            dead
        ));
        // A tmux pane inherits the brand but runs on a different tty → not nested,
        // regardless of liveness.
        assert!(!is_nested_koshell(
            &env_of(&[("KOSHELL", "koshell-1,/dev/pts/3")]),
            Some("/dev/pts/8"),
            live
        ));
        // No current tty can never match a branded tty.
        assert!(!is_nested_koshell(
            &env_of(&[("KOSHELL", "koshell-1,/dev/pts/3")]),
            None,
            live
        ));
        // An empty tty field is treated as absent (falls back to presence → nested).
        assert!(is_nested_koshell(
            &env_of(&[("KOSHELL", "koshell-1,")]),
            tty,
            dead
        ));
    }

    #[test]
    fn koshell_env_value_round_trips() {
        let value = koshell_env_value("koshell-4821", "/dev/pts/3");
        assert_eq!(value, "koshell-4821,/dev/pts/3");
        assert_eq!(koshell_session_id(&value), Some("koshell-4821"));
        assert_eq!(koshell_tty(&value), Some("/dev/pts/3"));
        // Base (no tty) form.
        assert_eq!(koshell_session_id("koshell-4821"), Some("koshell-4821"));
        assert_eq!(koshell_tty("koshell-4821"), None);
        assert_eq!(koshell_tty("koshell-4821,"), None);
        assert_eq!(koshell_session_id(""), None);
    }

    #[test]
    fn tty_marker_path_escapes_the_tty() {
        assert!(
            tty_marker_path("/dev/pts/3").ends_with("tty/_dev_pts_3"),
            "the marker path escapes slashes so it is a single path component"
        );
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
        // Coarse fallback (no tty field) still trips regardless of tty.
        assert!(
            assert_not_nested_koshell(&env_of(&[("KOSHELL", "koshell-1")]), Some("/dev/pts/3"))
                .is_err()
        );
        // A branded tty different from ours is never nested (short-circuits before the
        // liveness derivation, so no marker file is needed).
        assert!(
            assert_not_nested_koshell(
                &env_of(&[("KOSHELL", "koshell-1,/dev/pts/3")]),
                Some("/dev/pts/8")
            )
            .is_ok()
        );
        // Not inside koshell at all.
        assert!(assert_not_nested_koshell(&env_of(&[]), Some("/dev/pts/3")).is_ok());
        // The same-tty live/dead liveness branch is covered by `nested_detection` (which
        // takes the liveness bool directly) and by the end-to-end `shell_init_pty` tests
        // that write a real marker at the conventional path.
    }

    // fix 0011: the refusal is a typed error carrying the fail-open decision, so a nested
    // launch is never mistaken for the startup failure the fail-open exists for.
    #[test]
    fn nested_refusal_is_typed_and_carries_the_fail_open_decision() {
        let error = assert_not_nested_koshell(&env_of(&[("KOSHELL", "koshell-1")]), None)
            .expect_err("the coarse fallback refuses");
        let nested = error
            .downcast_ref::<NestedLaunch>()
            .expect("a typed NestedLaunch, not an opaque message");
        // Nothing provable without an owner pid: keep the terminal-preserving fail-open.
        assert_eq!(nested.owner_pid, None);
        assert!(nested.replaced_the_shell);
    }

    #[test]
    fn detect_reads_exec_from_the_parent_being_the_owning_koshell() {
        // Plain `koshell` at the inner shell's prompt: the shell is our parent, so it is
        // still there to return to — refuse and exit, never fail open into a nested shell.
        let typed = NestedLaunch::detect(Some(4100), 4242);
        assert!(!typed.replaced_the_shell);
        assert!(typed.to_string().contains("4100"), "names the owner");

        // `exec koshell` (the auto-wrap snippet, or hand-typed): the shell's process image
        // is gone and we inherited its parent, the wrapping koshell. Exiting would close
        // the terminal, so the fail-open must fire.
        assert!(NestedLaunch::detect(Some(4100), 4100).replaced_the_shell);

        // No owner pid: unprovable, so keep the safe direction.
        assert!(NestedLaunch::detect(None, 4242).replaced_the_shell);
    }

    // The owner pid is what makes the exec discrimination above possible, so the marker
    // read has to yield the pid itself and not only the liveness bool it used to.
    #[test]
    fn tty_marker_owner_pid_reads_the_pid_not_just_liveness() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("marker");
        let path = path.to_str().unwrap();
        assert_eq!(tty_marker_owner_pid(path), None, "no marker, no owner");

        std::fs::write(path, format!("{}\n", std::process::id())).expect("write live marker");
        assert_eq!(
            tty_marker_owner_pid(path),
            Some(std::process::id() as libc::pid_t)
        );
        assert!(tty_marker_is_live(Some(path)));

        std::fs::write(path, "2147483646").expect("write dead marker");
        assert_eq!(tty_marker_owner_pid(path), None, "a dead pid owns nothing");
        assert!(!tty_marker_is_live(Some(path)));
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

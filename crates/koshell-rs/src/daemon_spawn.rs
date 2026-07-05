//! Auto-spawning the AI daemon (design 0008).
//!
//! When a `#?` dispatch cannot connect to the daemon socket, the terminal starts
//! the daemon itself so the user never runs it by hand. This module owns the
//! command resolution chain, the zombie-free detached spawn, and the per-session
//! spawn cooldown. It stays separate from `ipc.rs` (a pure transport client):
//! spawning is a lifecycle concern, like `event_log.rs`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::logging;

/// Escape hatch, following the `KOSHELL_NO_EVENT_LOG` convention: any non-empty
/// value disables auto-spawn. Gates the interactive path only; the explicit
/// `koshell daemon start` command ignores it.
const DISABLE_ENV_KEY: &str = "KOSHELL_NO_DAEMON_SPAWN";
/// Verbatim daemon command line, run through `sh -c`.
const DAEMON_CMD_ENV: &str = "KOSHELL_DAEMON_CMD";
/// Installed daemon binary name, looked for next to the `koshell` executable and
/// then on `PATH`.
const DAEMON_BIN_NAME: &str = "koshell-ai-daemon";

/// A spawn is attempted at most once per this interval per session: a daemon that
/// dies is respawned on a later `#?`, while a broken command costs at most one
/// cheap `sh` fork per window.
const SPAWN_COOLDOWN: Duration = Duration::from_secs(30);
/// After a spawn, the dispatch path retries the connect for up to this long.
pub const CONNECT_RETRY_BUDGET: Duration = Duration::from_secs(1);
/// Poll interval while retrying the connect (the daemon is connectable in ~200ms).
pub const CONNECT_RETRY_STEP: Duration = Duration::from_millis(50);

/// A resolved way to launch the daemon. `source` records which chain rung matched,
/// for logging.
#[derive(Debug, Clone)]
pub struct SpawnPlan {
    pub command_line: String,
    pub source: &'static str,
}

/// Single-quotes a string for safe inclusion in an `sh -c` command line.
pub(crate) fn sh_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Finds an executable by name on the given `PATH`.
fn find_on_path(env: &impl Fn(&str) -> Option<String>, name: &str) -> Option<PathBuf> {
    let path = env("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Resolves the daemon launch command, independent of the kill switch (callers
/// gate that). The env lookup and executable directory are injected so the chain
/// is testable without touching process-global state:
///
/// 1. `KOSHELL_DAEMON_CMD` — verbatim.
/// 2. A `koshell-ai-daemon` binary next to the koshell executable.
/// 3. A `koshell-ai-daemon` binary on `PATH`.
///
/// There is deliberately no source-tree fallback: baking the build-time repo path
/// into the binary would leak it and break once the binary moves. In development,
/// point `KOSHELL_DAEMON_CMD` at the source (e.g. `bun packages/ai-daemon/src/index.ts`).
pub fn resolve_plan(
    env: impl Fn(&str) -> Option<String>,
    exe_dir: Option<&Path>,
) -> Option<SpawnPlan> {
    if let Some(command_line) = env(DAEMON_CMD_ENV).filter(|value| !value.is_empty()) {
        return Some(SpawnPlan {
            command_line,
            source: "env",
        });
    }

    if let Some(dir) = exe_dir {
        let candidate = dir.join(DAEMON_BIN_NAME);
        if candidate.is_file() {
            return Some(SpawnPlan {
                command_line: sh_quote(&candidate.to_string_lossy()),
                source: "adjacent",
            });
        }
    }

    if let Some(found) = find_on_path(&env, DAEMON_BIN_NAME) {
        return Some(SpawnPlan {
            command_line: sh_quote(&found.to_string_lossy()),
            source: "path",
        });
    }

    None
}

/// Resolves the launch command from the real environment. Does not consult the
/// kill switch — the interactive path gates that in [`DaemonSpawner::new`], while
/// `koshell daemon start` deliberately ignores it (an explicit command is intent).
pub fn resolve_plan_from_env() -> Option<SpawnPlan> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf));
    resolve_plan(|key| std::env::var(key).ok(), exe_dir.as_deref())
}

/// Whether the auto-spawn kill switch is set.
fn kill_switch_set() -> bool {
    std::env::var(DISABLE_ENV_KEY)
        .map(|value| !value.is_empty())
        .unwrap_or(false)
}

/// The auto-spawned daemon's log file: `$XDG_STATE_HOME/koshell/daemon.log`.
pub fn daemon_log_path() -> PathBuf {
    logging::state_dir().join("daemon.log")
}

/// Spawns the daemon fully detached, so it outlives this terminal and never
/// becomes a zombie: `sh -c 'exec <cmd> </dev/null >>daemon.log 2>&1 &'`
/// backgrounds the daemon and exits immediately; waiting on the short-lived `sh`
/// reaps the only direct child, and the daemon reparents to init/launchd.
pub fn spawn(plan: &SpawnPlan) -> anyhow::Result<()> {
    let log_path = daemon_log_path();
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let script = format!(
        "exec {} </dev/null >>{} 2>&1 &",
        plan.command_line,
        sh_quote(&log_path.to_string_lossy()),
    );
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        anyhow::bail!("daemon spawn shell exited with status {status}");
    }
    Ok(())
}

/// Whether a spawn attempted at `last` is still within the cooldown at `now`.
fn cooldown_active(last: Option<Instant>, now: Instant) -> bool {
    matches!(last, Some(last) if now.saturating_duration_since(last) < SPAWN_COOLDOWN)
}

/// Per-session daemon spawner: resolves the launch command once (honoring the
/// kill switch) and enforces the spawn cooldown.
pub struct DaemonSpawner {
    plan: Option<SpawnPlan>,
    last_attempt: Option<Instant>,
}

impl DaemonSpawner {
    /// For the interactive auto-spawn path: honors `KOSHELL_NO_DAEMON_SPAWN` and
    /// resolves the launch command from the environment.
    pub fn new() -> Self {
        let plan = if kill_switch_set() {
            None
        } else {
            resolve_plan_from_env()
        };
        if plan.is_none() {
            log::debug!("AI daemon auto-spawn disabled or no launch command resolved");
        }
        Self {
            plan,
            last_attempt: None,
        }
    }

    /// Attempts a spawn, subject to the cooldown. Returns `true` when a spawn was
    /// actually launched (so the caller should retry the connect), `false` when
    /// there is no plan, the cooldown is active, or the spawn failed.
    pub fn try_spawn(&mut self, now: Instant) -> bool {
        let Some(plan) = self.plan.as_ref() else {
            return false;
        };
        if cooldown_active(self.last_attempt, now) {
            return false;
        }
        self.last_attempt = Some(now);
        match spawn(plan) {
            Ok(()) => {
                log::info!(
                    "spawned the AI daemon ({}): {}",
                    plan.source,
                    plan.command_line
                );
                true
            }
            Err(error) => {
                log::warn!("failed to spawn the AI daemon: {error}");
                false
            }
        }
    }
}

impl Default for DaemonSpawner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sh_quote_wraps_and_escapes() {
        assert_eq!(sh_quote("plain"), "'plain'");
        assert_eq!(sh_quote("with space"), "'with space'");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn env_command_beats_everything() {
        let env = |key: &str| match key {
            "KOSHELL_DAEMON_CMD" => Some("my-daemon --flag".to_string()),
            _ => None,
        };
        let plan = resolve_plan(env, None).expect("a plan");
        assert_eq!(plan.source, "env");
        assert_eq!(plan.command_line, "my-daemon --flag");
    }

    #[test]
    fn empty_env_command_is_ignored() {
        let env = |key: &str| match key {
            "KOSHELL_DAEMON_CMD" => Some(String::new()),
            _ => None,
        };
        // No adjacent binary and no PATH for bun, so nothing resolves.
        assert!(resolve_plan(env, None).is_none());
    }

    #[test]
    fn adjacent_binary_beats_path() {
        let dir = tempfile::tempdir().expect("temp dir");
        let binary = dir.path().join(DAEMON_BIN_NAME);
        std::fs::write(&binary, "#!/bin/sh\n").expect("write binary");
        // PATH also has a daemon binary, but the adjacent one wins.
        let path_dir = tempfile::tempdir().expect("path dir");
        std::fs::write(path_dir.path().join(DAEMON_BIN_NAME), "").expect("write path binary");
        let path = path_dir.path().to_string_lossy().into_owned();
        let env = move |key: &str| match key {
            "PATH" => Some(path.clone()),
            _ => None,
        };
        let plan = resolve_plan(env, Some(dir.path())).expect("a plan");
        assert_eq!(plan.source, "adjacent");
        assert_eq!(plan.command_line, sh_quote(&binary.to_string_lossy()));
    }

    #[test]
    fn path_binary_used_when_no_adjacent() {
        let path_dir = tempfile::tempdir().expect("path dir");
        let binary = path_dir.path().join(DAEMON_BIN_NAME);
        std::fs::write(&binary, "").expect("write path binary");
        let path = path_dir.path().to_string_lossy().into_owned();
        let env = move |key: &str| match key {
            "PATH" => Some(path.clone()),
            _ => None,
        };
        let plan = resolve_plan(env, None).expect("a plan");
        assert_eq!(plan.source, "path");
        assert!(plan.command_line.contains(DAEMON_BIN_NAME));
    }

    #[test]
    fn nothing_resolves_without_env_adjacent_or_path() {
        let empty_dir = tempfile::tempdir().expect("empty dir");
        let path = empty_dir.path().to_string_lossy().into_owned();
        let env = move |key: &str| match key {
            "PATH" => Some(path.clone()),
            _ => None,
        };
        // No override, no adjacent binary, no binary on PATH.
        assert!(resolve_plan(env, None).is_none());
    }

    #[test]
    fn cooldown_blocks_a_second_attempt_within_the_window() {
        let start = Instant::now();
        assert!(!cooldown_active(None, start));
        assert!(cooldown_active(
            Some(start),
            start + Duration::from_secs(10)
        ));
        assert!(!cooldown_active(
            Some(start),
            start + SPAWN_COOLDOWN + Duration::from_secs(1)
        ));
    }
}

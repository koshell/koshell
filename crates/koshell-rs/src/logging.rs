//! Leveled logging for the foreground terminal process.
//!
//! koshell owns the terminal in raw mode, so logs must never reach the screen: they
//! are written to `$XDG_STATE_HOME/koshell/koshell.log` (falling back to
//! `~/.local/state/koshell/koshell.log`). The filter resolves in priority order:
//! `--log-level` argument, then the `KOSHELL_LOG` environment variable, then `warn`.
//! Filter syntax is `env_logger`'s (a level name, or module=level pairs).

use std::fs::OpenOptions;
use std::path::PathBuf;

const LOG_ENV_KEY: &str = "KOSHELL_LOG";
const DEFAULT_FILTER: &str = "warn";

/// Resolves the effective log filter from the CLI argument and the environment.
pub fn resolve_filter(cli_level: Option<&str>) -> String {
    if let Some(level) = cli_level {
        return level.to_string();
    }
    match std::env::var(LOG_ENV_KEY) {
        Ok(value) if !value.trim().is_empty() => value,
        _ => DEFAULT_FILTER.to_string(),
    }
}

/// The koshell state directory: `$XDG_STATE_HOME/koshell`, falling back to
/// `~/.local/state/koshell`. Shared by the terminal log and the auto-spawned
/// daemon log.
///
/// `None` when neither variable yields an **absolute** base — no `HOME`, or either one set
/// to a relative path. A relative state directory would resolve against the current
/// working directory, and koshell's cwd is not its own: it follows the inner shell's `cd`
/// (design 0005 working-directory mirroring), so the log would be created in whatever
/// project directory the user happened to be in when it was opened. Losing the log is the
/// better failure, and every caller already treats it as best-effort.
pub fn state_dir() -> Option<PathBuf> {
    resolve_state_dir(
        std::env::var("XDG_STATE_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// The pure resolution behind [`state_dir`], taking the two variables so the absent and
/// relative cases are testable without mutating process-global environment.
fn resolve_state_dir(xdg_state_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let base = match xdg_state_home {
        Some(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(home.filter(|home| !home.trim().is_empty())?)
            .join(".local")
            .join("state"),
    };
    base.is_absolute().then(|| base.join("koshell"))
}

/// The log file path under the XDG state directory, or `None` when there is no usable
/// state directory (see [`state_dir`]).
pub fn log_file_path() -> Option<PathBuf> {
    Some(state_dir()?.join("koshell.log"))
}

/// Initializes the global logger. Failing to open the log file — or having nowhere to put
/// one — disables logging rather than failing startup or writing into the terminal.
pub fn init(cli_level: Option<&str>) {
    let filter = resolve_filter(cli_level);
    let Some(path) = log_file_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    env_logger::Builder::new()
        .parse_filters(&filter)
        .target(env_logger::Target::Pipe(Box::new(file)))
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_level_wins_over_default() {
        assert_eq!(resolve_filter(Some("debug")), "debug");
        // With no CLI level the filter is the env value or the default; both are
        // non-empty either way (the env var may be set in the test environment).
        assert!(!resolve_filter(None).is_empty());
    }

    #[test]
    fn log_path_is_under_a_koshell_state_directory() {
        // Tests always run with a HOME, so a path is resolvable here.
        let path = log_file_path().expect("a state directory under the test HOME");
        assert!(path.ends_with("koshell/koshell.log"));
    }

    #[test]
    fn xdg_state_home_wins_and_home_is_the_fallback() {
        assert_eq!(
            resolve_state_dir(Some("/xdg/state"), Some("/home/user")),
            Some(PathBuf::from("/xdg/state/koshell"))
        );
        // Unset and blank both fall through to HOME, matching the other XDG readers.
        for blank in [None, Some(""), Some("   ")] {
            assert_eq!(
                resolve_state_dir(blank, Some("/home/user")),
                Some(PathBuf::from("/home/user/.local/state/koshell")),
                "XDG_STATE_HOME={blank:?} should fall back to HOME"
            );
        }
    }

    // A relative base would resolve against the current working directory, and koshell's
    // cwd follows the inner shell's `cd` — so the log would land in whatever project
    // directory the user was in. No path is the correct answer, not a relative one.
    #[test]
    fn a_relative_or_absent_base_yields_no_state_directory() {
        assert_eq!(resolve_state_dir(None, None), None);
        for blank in [Some(""), Some("   ")] {
            assert_eq!(resolve_state_dir(None, blank), None, "HOME={blank:?}");
        }
        assert_eq!(resolve_state_dir(Some("relative/state"), None), None);
        assert_eq!(
            resolve_state_dir(Some(".local/state"), Some("/home/user")),
            None,
            "a relative XDG_STATE_HOME is not rescued by an absolute HOME: it was chosen"
        );
        assert_eq!(resolve_state_dir(None, Some("relative/home")), None);
    }
}

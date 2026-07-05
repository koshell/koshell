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
pub fn state_dir() -> PathBuf {
    let base = match std::env::var("XDG_STATE_HOME") {
        Ok(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
        _ => {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home).join(".local").join("state")
        }
    };
    base.join("koshell")
}

/// The log file path under the XDG state directory.
pub fn log_file_path() -> PathBuf {
    state_dir().join("koshell.log")
}

/// Initializes the global logger. Failing to open the log file disables logging
/// rather than failing startup or writing into the terminal.
pub fn init(cli_level: Option<&str>) {
    let filter = resolve_filter(cli_level);
    let path = log_file_path();
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
        let path = log_file_path();
        assert!(path.ends_with("koshell/koshell.log"));
    }
}

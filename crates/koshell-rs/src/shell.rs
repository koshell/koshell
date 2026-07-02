//! Shell resolution, PTY environment construction, and nested-start prevention.
//!
//! Ports the algorithms from the frozen `reference/src/shell.ts` so behavior stays
//! identical: `SHELL`-first resolution with a fixed fallback list, a system-only
//! fallback `PATH`, and a `KOSHELL` marker that blocks running koshell inside koshell.

use std::collections::HashMap;
use std::ffi::CString;
use std::path::Path;

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

/// Environment marker set on the child shell to detect nested koshell launches.
const KOSHELL_ENV_KEY: &str = "KOSHELL";
const KOSHELL_ENV_VALUE: &str = "1";

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

/// Returns true when the environment already carries the koshell marker.
pub fn is_nested_koshell(env: &HashMap<String, String>) -> bool {
    env.get(KOSHELL_ENV_KEY).map(String::as_str) == Some(KOSHELL_ENV_VALUE)
}

/// Fails when koshell is being launched from inside an existing koshell shell.
pub fn assert_not_nested_koshell(env: &HashMap<String, String>) -> anyhow::Result<()> {
    if is_nested_koshell(env) {
        anyhow::bail!(
            "koshell is already running in this shell. Start a new regular terminal session before launching koshell again."
        );
    }
    Ok(())
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
        assert!(!is_nested_koshell(&env_of(&[])));
        assert!(!is_nested_koshell(&env_of(&[("KOSHELL", "")])));
        assert!(is_nested_koshell(&env_of(&[("KOSHELL", "1")])));
        assert!(assert_not_nested_koshell(&env_of(&[("KOSHELL", "1")])).is_err());
        assert!(assert_not_nested_koshell(&env_of(&[])).is_ok());
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

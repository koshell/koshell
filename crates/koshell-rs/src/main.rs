//! `koshell` — the foreground terminal process of the hybrid architecture.
//!
//! This binary is a thin wrapper over the `koshell_rs` library, which owns the PTY,
//! terminal mirror, snapshots, timeline, and `#?` detection. It stays
//! usable as a transparent shell wrapper even when the AI daemon is unavailable.

use clap::Parser;

use koshell_rs::cli::Command;

fn main() {
    let cli = koshell_rs::cli::Cli::parse();
    koshell_rs::logging::init(cli.log_level.as_deref());
    let command = match cli.command {
        Some(Command::ShellInit { shell }) => {
            print!("{}", koshell_rs::shell_init::snippet(shell));
            return;
        }
        Some(Command::Preflight) => {
            std::process::exit(koshell_rs::session::preflight());
        }
        Some(Command::Daemon { action }) => {
            std::process::exit(koshell_rs::daemon_cli::run(action));
        }
        Some(Command::Auth { action }) => {
            std::process::exit(koshell_rs::auth_cli::run(action));
        }
        Some(Command::Model {
            session_only,
            action,
        }) => {
            std::process::exit(koshell_rs::model_cli::run(session_only, action));
        }
        Some(Command::Status) => {
            std::process::exit(koshell_rs::status_cli::run());
        }
        Some(Command::Reload { all }) => {
            std::process::exit(koshell_rs::reload_cli::run(all));
        }
        Some(Command::Launch(command)) => command,
        None => Vec::new(),
    };
    // Fail open (design 0003 / audit obligation 16): a startup error or a panic before
    // koshell takes over the terminal must not leave a dead terminal under the
    // `exec koshell` auto-wrap. `RawModeGuard` restores cooked mode while unwinding, so by
    // the time control returns here the terminal is usable again.
    let launching_shell = command.is_empty();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        koshell_rs::session::run_interactive_shell(&command)
    }));
    match result {
        Ok(Ok(code)) => std::process::exit(code),
        Ok(Err(error)) => eprintln!("koshell failed: {error}"),
        // The default panic hook has already printed the panic location and message.
        Err(_) => eprintln!("koshell panicked during startup"),
    }
    // Only the auto-wrap (bare `exec koshell`, no explicit program) risks locking the user
    // out; an explicit `koshell <command>` still has its parent shell to fall back to, so
    // exiting non-zero is the faithful outcome there.
    if launching_shell {
        koshell_rs::session::exec_fallback_shell();
    }
    std::process::exit(1);
}

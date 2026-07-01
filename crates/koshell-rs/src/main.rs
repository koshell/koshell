//! `koshell` — the foreground terminal process of the hybrid architecture.
//!
//! This process owns the PTY and (in later phases) the terminal mirror, snapshots,
//! timeline, and `#?` detection. It stays usable as a transparent shell wrapper even
//! when the AI daemon is unavailable.

mod session;
mod shell;

fn main() {
    match session::run_interactive_shell() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("koshell failed: {error}");
            std::process::exit(1);
        }
    }
}

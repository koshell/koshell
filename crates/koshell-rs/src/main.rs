//! `koshell` — the foreground terminal process of the hybrid architecture.
//!
//! This binary is a thin wrapper over the `koshell_rs` library, which owns the PTY,
//! terminal mirror, snapshots, timeline, and (in later phases) `#?` detection. It stays
//! usable as a transparent shell wrapper even when the AI daemon is unavailable.

fn main() {
    match koshell_rs::session::run_interactive_shell() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("koshell failed: {error}");
            std::process::exit(1);
        }
    }
}

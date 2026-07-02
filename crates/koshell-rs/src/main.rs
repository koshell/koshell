//! `koshell` — the foreground terminal process of the hybrid architecture.
//!
//! This binary is a thin wrapper over the `koshell_rs` library, which owns the PTY,
//! terminal mirror, snapshots, timeline, and (in later phases) `#?` detection. It stays
//! usable as a transparent shell wrapper even when the AI daemon is unavailable.

use clap::Parser;

fn main() {
    let cli = koshell_rs::cli::Cli::parse();
    koshell_rs::logging::init(cli.log_level.as_deref());
    match koshell_rs::session::run_interactive_shell(&cli.command) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("koshell failed: {error}");
            std::process::exit(1);
        }
    }
}

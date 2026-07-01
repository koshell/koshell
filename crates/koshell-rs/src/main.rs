//! `koshell` — the foreground terminal process of the hybrid architecture.
//!
//! This process owns the PTY, the terminal mirror, snapshots, the timeline, and
//! `#?` detection. It stays usable as a transparent shell wrapper even when the
//! AI daemon is unavailable.
//!
//! Scaffolding stage: the interactive terminal core lands in Phase 1.

fn main() {
    eprintln!("koshell: terminal-core scaffolding. Interactive shell wrapper lands in Phase 1.");
}

//! koshell terminal-core library: PTY session, terminal mirror, timeline, screen
//! diffing, and terminal context. The `koshell` binary is a thin wrapper over this.

/// This build's version (design 0024), stamped by `build.rs`: an explicit
/// `KOSHELL_VERSION`, else the tag on `HEAD`, else the build's UTC timestamp
/// (`YYYYMMDD.HHMMSS`). It is what `koshell --version` prints, what a wrapped terminal
/// records beside its liveness marker, and therefore what `koshell version` compares.
pub const VERSION: &str = env!("KOSHELL_BUILD_VERSION");

pub mod auth_cli;
pub mod cli;
pub mod command_history;
pub mod command_tools;
pub mod context;
pub mod control_cli;
pub mod daemon_cli;
pub mod daemon_spawn;
pub mod event_log;
pub mod ipc;
pub mod logging;
pub mod mirror;
pub mod model_cli;
pub mod presentation;
pub mod reload_cli;
pub mod screen_diff;
pub mod session;
pub mod shell;
pub mod shell_init;
pub mod shell_integration;
pub mod status_cli;
pub mod timeline;
pub mod trigger;
pub mod version_cli;

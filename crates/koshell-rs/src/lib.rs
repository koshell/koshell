//! koshell terminal-core library: PTY session, terminal mirror, timeline, screen
//! diffing, and terminal context. The `koshell` binary is a thin wrapper over this.

pub mod auth_cli;
pub mod cli;
pub mod context;
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

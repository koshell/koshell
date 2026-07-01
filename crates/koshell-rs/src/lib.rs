//! koshell terminal-core library: PTY session, terminal mirror, timeline, screen
//! diffing, and terminal context. The `koshell` binary is a thin wrapper over this.

pub mod context;
pub mod mirror;
pub mod screen_diff;
pub mod session;
pub mod shell;
pub mod shell_integration;
pub mod timeline;
pub mod trigger;

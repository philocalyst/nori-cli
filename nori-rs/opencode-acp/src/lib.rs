//! OpenCode v2 API support for Nori's in-process ACP adapter.
//!
//! OpenCode's execution engine remains an external process. This crate owns its HTTP
//! transport, event stream, and ACP translation. See the turn-lifecycle documentation for
//! remaining differences from OpenCode's upstream ACP adapter.

mod agent;
mod children;
mod config;
mod directories;
mod event;
mod lifecycle;
mod mcp;
mod permission;
mod prompt;
mod provider_error;
mod question;
mod replay;
mod server;
mod service_discovery;
mod session_mcp;
mod turn;

pub use agent::registration;
pub use server::OpenCodeServer;

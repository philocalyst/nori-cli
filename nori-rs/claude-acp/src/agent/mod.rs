//! The Claude Code agent: its [`NativeAgent`](nori_acp_kit::NativeAgent) implementation and
//! the work behind each ACP request.

mod catalog;
mod core;
mod identity;
mod lifecycle;
mod listing;
mod native;
mod prompt;
mod slash_commands;

pub use core::ClaudeAcpAgent;

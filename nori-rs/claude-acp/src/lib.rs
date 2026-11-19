//! Claude Code as an agent inside Nori's process.
//!
//! The driver starts Claude Code's own CLI for each session and translates its stream into ACP;
//! there is no adapter process. [`registration`] is the only way in: Nori's ACP host serves the
//! agent over an in-memory pipe (see `nori_acp_kit`).

mod agent;
mod command_safety;
mod converter;
mod hooks;
mod mcp;
mod permissions;
mod registration;
mod session;
mod settings;
mod terminal;
mod types;
mod utils;

pub use registration::registration;

//! Public types for Claude Code ACP Agent
//!
//! This module contains all the shared types used across the crate.

mod config;
mod error;
mod meta;
mod tool;

pub use config::AgentConfig;
pub use error::AgentError;
pub use error::Result;
pub use error::from_sdk;
pub use meta::NewSessionMeta;
pub use tool::ToolInfo;
pub use tool::ToolKind;
pub use tool::ToolUseEntry;

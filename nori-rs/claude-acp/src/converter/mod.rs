//! Message conversion utilities for ACP ↔ Claude SDK
//!
//! This module handles conversion between:
//! - ACP `PromptRequest` → Claude SDK `UserContentBlock`
//! - Claude SDK `Message` → ACP `SessionNotification`

mod edit_snapshots;
mod notification;
mod tool;

pub use edit_snapshots::EditSnapshots;
pub use notification::NotificationConverter;
pub use tool::extract_tool_info;

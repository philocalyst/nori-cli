//! Session management for ACP Agent
//!
//! This module handles:
//! - Session lifecycle (create, get, remove)
//! - Token usage tracking
//! - Permission handling
//! - Session state management
//! - Interactive permission requests
//! - Background process management

mod background_processes;
mod manager;
mod permission;
mod permission_request;
#[allow(clippy::module_inception)]
mod session;
mod usage;
pub mod warm;
mod wrapped_child;

pub use background_processes::BackgroundProcessManager;
pub use background_processes::BackgroundTerminal;
pub use background_processes::ChildHandle;
pub use background_processes::TerminalExitStatus;
pub use manager::SessionManager;
pub use permission::PermissionHandler;
pub use permission::PermissionMode;
pub use permission::ToolPermissionResult;
pub use permission_request::PermissionOutcome;
pub use permission_request::PermissionRequestBuilder;
pub use session::Session;
pub(crate) use session::get_acp_replacement_tools as acp_replacement_tools;
pub use session::stable_cache_key;
pub use wrapped_child::WrappedChild;

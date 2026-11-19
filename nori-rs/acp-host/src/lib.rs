//! Host side of the Agent Client Protocol: agent registry, subprocess
//! connection management (JSON-RPC over stdio) and permission plumbing.
//!
//! This crate must stay independent of the session harness and any terminal UI.

pub mod accounts;
pub mod antigravity_cli;
pub mod connection;
mod error_category;
pub mod in_process;
pub mod patch;
pub mod registry;
pub mod remote;
pub mod sign_in;

pub use error_category::FailureClass;
pub use error_category::FailureDetails;
pub use error_category::classify_failure;
pub use error_category::classify_text;
pub use error_category::is_subscription_exhausted;

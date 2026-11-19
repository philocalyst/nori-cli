//! MCP (Model Context Protocol) Server integration
//!
//! This module provides built-in tools that can be used by Claude Code.
//! The tools follow the MCP pattern but are implemented natively for efficiency.
//!
//! ## Tool Categories
//!
//! - **File Tools**: Read, Write, Edit - File system operations
//! - **Terminal Tools**: Bash, KillShell - Command execution
//! - **Search Tools**: Grep, Glob - Code search
//!
//! ## External MCP Servers
//!
//! The `external` module provides support for connecting to external MCP servers
//! to extend tool capabilities.
//!
//! ## Session tools
//!
//! The `session_tools` module hands the client's MCP servers to the CLI and, when Nori runs this
//! driver in its own process, Nori's host tools as an in-process server.
//!
//! ## ACP Integration
//!
//! The `acp_server` module provides an MCP server that integrates with the ACP
//! protocol, allowing tools to send notifications during execution.

mod acp_server;
mod registry;
mod session_tools;
pub mod tools;

pub use acp_server::AcpMcpServer;
pub use registry::ACP_TOOL_PREFIX;
pub use registry::MAX_JOURNALED_BYTES;
pub use session_tools::HostToolsBinding;
pub use session_tools::SessionTools;
pub use session_tools::host_tool_name;

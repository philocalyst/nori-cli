//! Base tool trait definition

use async_trait::async_trait;

use crate::mcp::registry::ToolContext;
use crate::mcp::registry::ToolResult;

/// Tool trait for MCP-compatible tools
///
/// Tools implement this trait to provide functionality that can be
/// invoked by Claude or other agents.
#[async_trait]
pub trait Tool: Send + Sync + std::fmt::Debug {
    /// Get the tool name
    fn name(&self) -> &str;

    /// Get the tool description
    fn description(&self) -> &str;

    /// Get the JSON Schema for the tool's input parameters
    fn input_schema(&self) -> serde_json::Value;

    /// Execute the tool with the given input
    async fn execute(&self, input: serde_json::Value, context: &ToolContext) -> ToolResult;
}

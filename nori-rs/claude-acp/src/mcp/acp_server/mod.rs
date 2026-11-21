//! ACP MCP Server implementation
//!
//! This module provides an MCP server that integrates with the ACP protocol,
//! allowing tools to send notifications during execution (e.g., terminal output).

mod bash;
mod notify;
mod rpc;

use rpc::PlaceholderHandler;

use nori_acp_kit::Cx;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Instant;

use async_trait::async_trait;
use claude_code_agent_sdk::SdkMcpServer;
use claude_code_agent_sdk::SdkMcpTool;
use claude_code_agent_sdk::ToolDefinition;
use claude_code_agent_sdk::ToolHandler;
use claude_code_agent_sdk::ToolResult as SdkToolResult;
use futures::FutureExt;
use futures::future::BoxFuture;
use nori_acp_kit::schema::Diff;
use nori_acp_kit::schema::Meta;
use nori_acp_kit::schema::SessionId;
use nori_acp_kit::schema::SessionNotification;
use nori_acp_kit::schema::SessionUpdate;
use nori_acp_kit::schema::Terminal;
use nori_acp_kit::schema::ToolCall;
use nori_acp_kit::schema::ToolCallContent;
use nori_acp_kit::schema::ToolCallId;
use nori_acp_kit::schema::ToolCallStatus;
use nori_acp_kit::schema::ToolCallUpdate;
use nori_acp_kit::schema::ToolCallUpdateFields;
use nori_acp_kit::schema::ToolKind;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tracing::instrument;

use crate::mcp::registry::ToolContext;
use crate::mcp::registry::ToolRegistry;
use crate::mcp::registry::ToolResult;
use crate::mcp::tools::BashOutputTool;
use crate::mcp::tools::BashTool;
use crate::mcp::tools::EditTool;
use crate::mcp::tools::KillShellTool;
use crate::mcp::tools::ReadTool;
use crate::mcp::tools::WriteTool;
use crate::session::BackgroundProcessManager;
use crate::settings::PermissionChecker;
use crate::terminal::TerminalClient;

/// Type alias for the cancel callback to reduce type complexity
type CancelCallback = Arc<Mutex<Option<Box<dyn Fn() + Send + Sync>>>>;

/// ACP-integrated MCP server
///
/// This server implements the SDK's `SdkMcpServer` trait, allowing it to be
/// injected into the Claude Agent SDK. It provides access to:
/// - ACP connection for sending notifications
/// - Terminal API for command execution
/// - Tool registry for built-in tools
///
/// # Lock Ordering (Critical for Deadlock Prevention)
///
/// To prevent deadlocks, follow this lock acquisition order:
/// 1. OnceLock values (cwd) - lock-free after initialization
/// 2. cancel_callback (Mutex) - use try_lock for non-critical access
/// 3. OnceLock values (session_id, connection_cx, etc.) - lock-free
///
/// NEVER hold locks while calling tool.execute() as tools may need
/// to send notifications that could deadlock.
pub struct AcpMcpServer {
    /// Server name
    name: String,
    /// Server version
    version: String,
    /// The tools that replace the CLI's own Bash, Read, Write and Edit
    registry: ToolRegistry,
    /// Tool definitions for SDK
    tools: HashMap<String, SdkMcpTool>,
    /// Session ID (set once at initialization)
    session_id: OnceLock<String>,
    /// ACP connection for sending notifications (set once at initialization)
    connection_cx: OnceLock<Cx>,
    /// Terminal client (set once at initialization)
    ///
    /// Note: While configure_acp_server is called on every prompt and
    /// creates a new TerminalClient each time, all instances are functionally
    /// equivalent (same connection_cx and session_id). OnceLock ensures we
    /// use only the first instance.
    terminal_client: OnceLock<Arc<TerminalClient>>,
    /// Background process manager (set once at initialization)
    background_processes: OnceLock<Arc<BackgroundProcessManager>>,
    /// Working directory (set once at initialization)
    ///
    /// Uses OnceLock instead of RwLock because:
    /// - cwd is set exactly once during session initialization
    /// - All tools only read cwd (no tool ever modifies it)
    /// - OnceLock provides lock-free reads after initialization
    /// - No deadlock risk
    cwd: OnceLock<std::path::PathBuf>,
    /// The session's file tools, with the read ledger their edits are checked against
    workspace: OnceLock<nori_tools::Workspace>,
    /// Permission checker for tool-level permission checks
    permission_checker: OnceLock<Arc<RwLock<PermissionChecker>>>,
    /// Cancel callback - called when MCP cancellation notification is received
    /// Uses Mutex (not RwLock) because writes are rare and we need try_lock for deadlock safety
    cancel_callback: CancelCallback,
}

impl std::fmt::Debug for AcpMcpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpMcpServer")
            .field("name", &self.name)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

impl AcpMcpServer {
    /// Create a new ACP MCP server
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        let mut registry = ToolRegistry::new();
        registry.register(ReadTool::new());
        registry.register(WriteTool::new());
        registry.register(EditTool::new());
        registry.register(BashTool::new());
        registry.register(BashOutputTool);
        registry.register(KillShellTool);
        let tools = Self::build_tool_definitions(&registry);

        Self {
            name: name.into(),
            version: version.into(),
            registry,
            tools,
            session_id: OnceLock::new(),
            connection_cx: OnceLock::new(),
            terminal_client: OnceLock::new(),
            background_processes: OnceLock::new(),
            cwd: OnceLock::new(),
            workspace: OnceLock::new(),
            permission_checker: OnceLock::new(),
            cancel_callback: Arc::new(Mutex::new(None)),
        }
    }

    /// Set the session ID (only sets if not already set)
    pub fn set_session_id(&self, session_id: impl Into<String>) {
        let new_id = session_id.into();
        if let Some(existing) = self.session_id.get() {
            if existing != &new_id {
                tracing::warn!(
                    existing = %existing,
                    new = %new_id,
                    "session_id already set with different value, ignoring new value"
                );
            }
        } else {
            drop(self.session_id.set(new_id));
        }
    }

    /// Set the ACP connection (only sets if not already set)
    pub fn set_connection(&self, cx: Cx) {
        if self.connection_cx.get().is_some() {
            tracing::warn!("connection_cx already set, ignoring new value");
        } else {
            drop(self.connection_cx.set(cx));
        }
    }

    /// Set the terminal client (only sets if not already set)
    pub fn set_terminal_client(&self, client: Arc<TerminalClient>) {
        // Only set if not already set - configure_acp_server may be called multiple times
        if self.terminal_client.get().is_none() {
            drop(self.terminal_client.set(client));
        }
    }

    /// Set the background process manager (only sets if not already set)
    pub fn set_background_processes(&self, manager: Arc<BackgroundProcessManager>) {
        // Only set if not already set - configure_acp_server may be called multiple times
        if self.background_processes.get().is_none() {
            drop(self.background_processes.set(manager));
        }
    }

    /// Set the permission checker (only sets if not already set)
    pub fn set_permission_checker(&self, checker: Arc<RwLock<PermissionChecker>>) {
        // Only set if not already set - configure_acp_server may be called multiple times
        if self.permission_checker.get().is_none() {
            drop(self.permission_checker.set(checker));
        }
    }

    /// Set the working directory (synchronous, lock-free)
    ///
    /// Uses OnceLock to set the value on first call.
    /// - First call: Sets the cwd
    /// - Subsequent calls with same value: Silently ignored
    /// - Subsequent calls with different value: Logs warning, ignored
    ///
    /// This is synchronous (not async) because OnceLock operations are
    /// non-blocking and lock-free after initialization.
    ///
    /// # Deadlock Prevention
    ///
    /// OnceLock provides lock-free reads after initialization, eliminating
    /// the deadlock risk that existed with RwLock.
    pub fn set_cwd(&self, cwd: impl Into<std::path::PathBuf>) {
        let new_cwd = cwd.into();
        if let Some(existing) = self.cwd.get() {
            if existing != &new_cwd {
                tracing::warn!(
                    existing = %existing.display(),
                    new = %new_cwd.display(),
                    "cwd already set with different value, ignoring new value"
                );
            }
            // If already set, silently ignore (first value wins)
            // This matches the pattern of other OnceLock setters in this struct
        } else {
            drop(self.cwd.set(new_cwd));
        }
    }

    /// Set the cancel callback
    ///
    /// This callback is invoked when a MCP cancellation notification is received.
    /// Use this to interrupt the Claude CLI when the user cancels an operation.
    pub async fn set_cancel_callback<F>(&self, callback: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        let mut guard = self.cancel_callback.lock().await;
        *guard = Some(Box::new(callback));
    }

    /// Build tool definitions from the MCP server
    ///
    /// Tools are registered with the `mcp__acp__` prefix so that CLI routes
    /// tool calls to our SDK MCP server via `mcp_message` requests.
    fn build_tool_definitions(registry: &ToolRegistry) -> HashMap<String, SdkMcpTool> {
        let mut tools = HashMap::new();

        for schema in registry.schemas() {
            // Use original tool name without prefix
            // The MCP protocol will automatically add mcp__acp__ prefix
            let tool_name = schema.name.clone();
            let description = schema.description.clone();
            let input_schema = schema.input_schema.clone();

            // Create a placeholder handler - actual execution goes through handle_message
            let tool = SdkMcpTool {
                name: tool_name.clone(),
                description,
                input_schema,
                handler: Arc::new(PlaceholderHandler),
                annotations: None,
            };

            tools.insert(tool_name, tool);
        }

        tools
    }

    /// Create a tool context for tool execution
    ///
    /// # Deadlock Prevention
    ///
    /// OnceLock provides lock-free reads after initialization, eliminating
    /// the deadlock risk that existed with RwLock.
    fn create_tool_context(&self, tool_use_id: Option<&str>) -> ToolContext {
        // OnceLock provides lock-free read after initialization
        let cwd = self.cwd.get().cloned().unwrap_or_default();

        // OnceLock provides lock-free access after initialization
        let session_id = self
            .session_id
            .get()
            .map(std::string::String::as_str)
            .unwrap_or("unknown");

        let terminal_client = self.terminal_client.get();
        let background_processes = self.background_processes.get();
        let connection_cx = self.connection_cx.get();
        let permission_checker = self.permission_checker.get();

        let workspace = self
            .workspace
            .get_or_init(|| nori_tools::Workspace::local(&cwd))
            .clone();
        let mut context = ToolContext::new(session_id.to_string(), cwd).with_workspace(workspace);

        if let Some(client) = terminal_client {
            context = context.with_terminal_client(client.clone());
        }

        if let Some(manager) = background_processes {
            context = context.with_background_processes(manager.clone());
        }

        if let Some(id) = tool_use_id {
            context = context.with_tool_use_id(id);
        }

        if let Some(cx) = connection_cx {
            context = context.with_connection_cx(cx.clone());
        }

        if let Some(checker) = permission_checker {
            context = context.with_permission_checker(checker.clone());
        }

        context
    }

    /// Execute a tool with ACP integration
    ///
    /// # Deadlock Prevention
    ///
    /// OnceLock provides lock-free access to cwd after initialization,
    /// eliminating the deadlock risk that existed with RwLock.
    #[instrument(
        name = "acp_execute_tool",
        skip(self, arguments),
        fields(
            tool_name = %tool_name,
            tool_use_id = ?tool_use_id,
            args_size = arguments.to_string().len(),
        )
    )]
    async fn execute_tool(
        &self,
        tool_name: &str,
        arguments: Value,
        tool_use_id: Option<&str>,
    ) -> Result<ToolResult, String> {
        let start_time = Instant::now();

        // Log arguments preview (truncated for large inputs)
        let args_str = arguments.to_string();

        // Truncate at character boundary to avoid panic on multi-byte UTF-8
        let args_preview = if args_str.len() > 500 {
            // Find a safe character boundary near byte 500
            let safe_boundary = args_str
                .char_indices()
                .map(|(i, _)| i)
                .find(|&i| i > 500)
                .unwrap_or(args_str.len());
            format!("{}...(truncated)", &args_str[..safe_boundary])
        } else {
            args_str.clone()
        };

        tracing::info!(
            tool_name = %tool_name,
            tool_use_id = ?tool_use_id,
            args_preview = %args_preview,
            "Executing ACP tool"
        );

        // CRITICAL: Create context with lock-free OnceLock access
        let context = self.create_tool_context(tool_use_id);

        tracing::debug!("Tool context created, calling tool execution");

        // Special handling for Bash tool - use early return to match original behavior
        if tool_name == "Bash" {
            let result = self
                .execute_bash_tool(arguments, tool_use_id, &context)
                .await;
            let elapsed = start_time.elapsed();
            match &result {
                Ok(r) => {
                    tracing::info!(
                        tool_name = %tool_name,
                        elapsed_ms = elapsed.as_millis(),
                        is_error = r.is_error,
                        content_len = r.content.len(),
                        "ACP Bash tool completed"
                    );
                }
                Err(e) => {
                    tracing::error!(
                        tool_name = %tool_name,
                        elapsed_ms = elapsed.as_millis(),
                        error = %e,
                        "ACP Bash tool failed"
                    );
                }
            }
            return result;
        }

        // Execute other tools normally
        // Note: OnceLock provides lock-free access, no locks to release
        let result = self.registry.execute(tool_name, arguments, &context).await;

        // Send completion notification to Zed (required for non-Bash tools)
        // Use RAII scoping to ensure locks are released promptly
        {
            // OnceLock provides lock-free access after initialization
            let session_id = self.session_id.get();
            let connection_cx = self.connection_cx.get();

            if let (Some(cx), Some(session_id), Some(tool_use_id)) =
                (connection_cx, session_id, tool_use_id)
            {
                let status = if result.is_error {
                    ToolCallStatus::Failed
                } else {
                    ToolCallStatus::Completed
                };

                // Errors carry their message. A file edit carries its diff (the whole file before
                // and after), which is what the client's edit journal records. Anything else
                // sends no content: the result message has its text.
                let content: Option<Vec<ToolCallContent>> = if result.is_error {
                    Some(vec![result.content.clone().into()])
                } else {
                    result.diff.as_ref().map(|diff| {
                        vec![ToolCallContent::Diff(
                            Diff::new(&diff.path, diff.new_text.clone())
                                .old_text(diff.old_text.clone()),
                        )]
                    })
                };

                // Send completion notification with content for errors
                if let Err(e) = Self::send_tool_call_update_with_meta(
                    cx,
                    session_id,
                    tool_use_id,
                    Some(status),
                    None,
                    content,
                    None,
                ) {
                    tracing::debug!("Failed to send tool completion notification: {}", e);
                }
            }
            // Locks are automatically released when this block ends
        }

        let elapsed = start_time.elapsed();

        tracing::info!(
            tool_name = %tool_name,
            elapsed_ms = elapsed.as_millis(),
            is_error = result.is_error,
            content_len = result.content.len(),
            "ACP tool completed"
        );

        Ok(result)
    }
}

#[cfg(test)]
mod tests;

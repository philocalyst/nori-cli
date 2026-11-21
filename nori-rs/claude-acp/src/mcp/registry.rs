//! Tool registry for managing MCP tools

use nori_acp_kit::Cx;
use std::collections::HashMap;
use std::sync::Arc;

use nori_acp_kit::schema::SessionId;
use nori_acp_kit::schema::SessionNotification;
use nori_acp_kit::schema::SessionUpdate;
use nori_acp_kit::schema::Terminal;
use nori_acp_kit::schema::ToolCallContent;
use nori_acp_kit::schema::ToolCallId;
use nori_acp_kit::schema::ToolCallStatus;
use nori_acp_kit::schema::ToolCallUpdate;
use nori_acp_kit::schema::ToolCallUpdateFields;
use serde::Deserialize;
use serde::Serialize;

use super::tools::Tool;
use crate::session::BackgroundProcessManager;
use crate::settings::PermissionChecker;
use crate::terminal::TerminalClient;

/// Files larger than this are not read to report what an overwrite replaced.
pub const MAX_JOURNALED_BYTES: u64 = 4 * 1024 * 1024;

/// A file change a tool made, as the client's edit journal needs it: the file's whole content
/// before and after. Not part of what the CLI receives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub path: std::path::PathBuf,
    /// What the file held before (`None`: it did not exist).
    pub old_text: Option<String>,
    pub new_text: String,
}

/// Tool execution result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// Result status
    pub status: ToolStatus,
    /// Output content
    pub content: String,
    /// Whether this is an error
    pub is_error: bool,
    /// Additional metadata
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// The file change this call made, reported to the client as a diff.
    #[serde(skip)]
    pub diff: Option<FileDiff>,
    /// Images returned to the model as MCP image content.
    #[serde(skip)]
    pub images: Vec<nori_tools::ImageAttachment>,
}

impl ToolResult {
    /// Create a successful result
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            status: ToolStatus::Success,
            content: content.into(),
            is_error: false,
            metadata: None,
            diff: None,
            images: Vec::new(),
        }
    }

    /// Create an error result
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            status: ToolStatus::Error,
            content: message.into(),
            is_error: true,
            metadata: None,
            diff: None,
            images: Vec::new(),
        }
    }

    /// The result of a `nori_tools` file tool: its text for the model, and the first change as
    /// the diff the client's edit journal records. Files too large to journal are changed
    /// without a reported diff, as the journal could not restore them either.
    pub fn from_file_tool(result: nori_tools::ToolResult) -> Self {
        let diff = result.changes.iter().find_map(|change| {
            let new_text = change.new_text.clone()?;
            let journalable = |text: &String| text.len() as u64 <= MAX_JOURNALED_BYTES;
            (journalable(&new_text) && change.old_text.as_ref().is_none_or(journalable)).then(
                || FileDiff {
                    path: change.path.clone(),
                    old_text: change.old_text.clone(),
                    new_text,
                },
            )
        });
        let metadata = (!result.changes.is_empty()).then(|| result.structured());
        let mut out = if result.is_error {
            Self::error(result.text)
        } else {
            Self::success(result.text)
        };
        out.diff = diff;
        out.metadata = metadata;
        out.images = result.images;
        out
    }

    /// Create a result with metadata
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = Some(metadata);
        self
    }
}

/// Tool execution status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolStatus {
    /// Tool executed successfully
    Success,
    /// Tool execution failed
    Error,
    /// Tool execution was cancelled
    Cancelled,
    /// Tool is still running (for async operations)
    Running,
}

/// Tool execution context
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// Session ID
    pub session_id: String,
    /// Working directory
    pub cwd: std::path::PathBuf,
    /// Background process manager
    background_processes: Option<Arc<BackgroundProcessManager>>,
    /// Terminal client for executing commands via Client PTY
    terminal_client: Option<Arc<TerminalClient>>,
    /// Current tool use ID (for sending mid-execution updates)
    tool_use_id: Option<String>,
    /// Connection context for sending notifications
    connection_cx: Option<Cx>,
    /// Permission checker for tool-level permission checks
    pub permission_checker: Option<Arc<tokio::sync::RwLock<PermissionChecker>>>,
    /// The session's file tools: one workspace, so the read ledger spans the session's calls
    workspace: nori_tools::Workspace,
}

impl ToolContext {
    /// Create a new tool context
    pub fn new(session_id: impl Into<String>, cwd: impl Into<std::path::PathBuf>) -> Self {
        let cwd = cwd.into();
        Self {
            session_id: session_id.into(),
            workspace: nori_tools::Workspace::local(&cwd),
            cwd,
            background_processes: None,
            terminal_client: None,
            tool_use_id: None,
            connection_cx: None,
            permission_checker: None,
        }
    }

    /// Use the session's file workspace instead of a fresh one.
    pub fn with_workspace(mut self, workspace: nori_tools::Workspace) -> Self {
        self.workspace = workspace;
        self
    }

    /// The file tools' workspace.
    pub fn workspace(&self) -> &nori_tools::Workspace {
        &self.workspace
    }

    /// Set the background process manager
    pub fn with_background_processes(mut self, manager: Arc<BackgroundProcessManager>) -> Self {
        self.background_processes = Some(manager);
        self
    }

    /// Set the terminal client
    pub fn with_terminal_client(mut self, client: Arc<TerminalClient>) -> Self {
        self.terminal_client = Some(client);
        self
    }

    /// Set the current tool use ID
    pub fn with_tool_use_id(mut self, id: impl Into<String>) -> Self {
        self.tool_use_id = Some(id.into());
        self
    }

    /// Set the connection context for sending notifications
    pub fn with_connection_cx(mut self, cx: Cx) -> Self {
        self.connection_cx = Some(cx);
        self
    }

    /// Set the permission checker for tool-level permission checks
    pub fn with_permission_checker(
        mut self,
        checker: Arc<tokio::sync::RwLock<PermissionChecker>>,
    ) -> Self {
        self.permission_checker = Some(checker);
        self
    }

    /// Get the background process manager
    pub fn background_processes(&self) -> Option<&Arc<BackgroundProcessManager>> {
        self.background_processes.as_ref()
    }

    /// Get the terminal client
    ///
    /// When available, tools can use this to execute commands via the Client's PTY
    /// instead of directly spawning processes.
    pub fn terminal_client(&self) -> Option<&Arc<TerminalClient>> {
        self.terminal_client.as_ref()
    }

    /// Send a ToolCallUpdate notification with Terminal content
    ///
    /// This is used by tools like Bash to send terminal ID immediately after
    /// creating a terminal, so the client can start showing terminal output.
    ///
    /// # Arguments
    ///
    /// * `terminal_id` - The terminal ID from CreateTerminalResponse
    /// * `status` - The tool call status (usually InProgress)
    /// * `title` - Optional title/description for the tool call
    ///
    /// # Returns
    ///
    /// `Ok(())` if notification was sent, `Err` if context doesn't have connection
    pub fn send_terminal_update(
        &self,
        terminal_id: impl Into<String>,
        status: ToolCallStatus,
        title: Option<&str>,
    ) -> Result<(), String> {
        let Some(connection_cx) = &self.connection_cx else {
            return Err("No connection context available".to_string());
        };

        let Some(tool_use_id) = &self.tool_use_id else {
            return Err("No tool use ID available".to_string());
        };

        // Build terminal content
        let terminal = Terminal::new(terminal_id.into());
        let content = vec![ToolCallContent::Terminal(terminal)];

        // Build update fields
        let mut update_fields = ToolCallUpdateFields::new().status(status).content(content);

        if let Some(title) = title {
            update_fields = update_fields.title(title);
        }

        // Build and send notification
        let tool_call_id = ToolCallId::new(tool_use_id.clone());
        let update = ToolCallUpdate::new(tool_call_id, update_fields);
        let notification = SessionNotification::new(
            SessionId::new(self.session_id.as_str()),
            SessionUpdate::ToolCallUpdate(update),
        );

        connection_cx
            .send_notification(notification)
            .map_err(|e| format!("Failed to send notification: {e}"))
    }
}

/// ACP tool prefix for compatibility with TypeScript implementation
pub const ACP_TOOL_PREFIX: &str = "mcp__acp__";

/// Tool registry for managing available tools
#[derive(Debug, Default)]
pub struct ToolRegistry {
    /// Registered tools by name
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Register a tool
    pub fn register<T: Tool + 'static>(&mut self, tool: T) {
        let name = tool.name().to_string();
        self.tools.insert(name, Arc::new(tool));
    }

    /// Get a tool by name, supporting ACP prefix
    ///
    /// If the tool name starts with `mcp__acp__`, it will try to find
    /// the tool with the prefix stripped.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        // Try direct lookup first
        if let Some(tool) = self.tools.get(name) {
            return Some(tool.clone());
        }

        // Try stripping ACP prefix
        if let Some(stripped) = name.strip_prefix(ACP_TOOL_PREFIX)
            && let Some(tool) = self.tools.get(stripped)
        {
            return Some(tool.clone());
        }

        None
    }

    /// Execute a tool by name
    pub async fn execute(
        &self,
        name: &str,
        input: serde_json::Value,
        context: &ToolContext,
    ) -> ToolResult {
        match self.get(name) {
            Some(tool) => tool.execute(input, context).await,
            None => ToolResult::error(format!("Tool not found: {name}")),
        }
    }

    /// Get tool schemas for all registered tools
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools
            .values()
            .map(|tool| ToolSchema {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                input_schema: tool.input_schema(),
            })
            .collect()
    }
}

/// Tool schema for registration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    /// Tool name
    pub name: String,
    /// Tool description
    pub description: String,
    /// JSON Schema for input
    pub input_schema: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_tool_result_success() {
        let result = ToolResult::success("Hello, World!");
        assert_eq!(result.status, ToolStatus::Success);
        assert_eq!(result.content, "Hello, World!");
        assert!(!result.is_error);
    }

    #[test]
    fn test_tool_result_error() {
        let result = ToolResult::error("Something went wrong");
        assert_eq!(result.status, ToolStatus::Error);
        assert_eq!(result.content, "Something went wrong");
        assert!(result.is_error);
    }

    #[test]
    fn test_tool_result_with_metadata() {
        let result = ToolResult::success("data").with_metadata(json!({"lines": 10}));
        assert!(result.metadata.is_some());
    }

    #[test]
    fn test_acp_prefix_constant() {
        assert_eq!(ACP_TOOL_PREFIX, "mcp__acp__");
    }
}

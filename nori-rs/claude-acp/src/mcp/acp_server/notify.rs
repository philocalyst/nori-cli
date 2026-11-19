//! Tool-call updates the server sends to the client while a tool runs.

use super::*;

impl AcpMcpServer {
    /// Send a ToolCallUpdate notification with optional fields
    ///
    /// This is used to send tool call updates (including errors) to Zed.
    ///
    /// # Parameters
    ///
    /// - `cx`: ACP connection context for sending notifications
    /// - `session_id`: Current session identifier
    /// - `tool_use_id`: Tool call identifier from Claude
    /// - `status`: Optional status change (Completed, Failed, InProgress, Pending)
    /// - `title`: Optional display title for the tool call
    /// - `content`: Optional content to display (typically error messages for Failed status)
    /// - `meta`: Optional metadata (terminal_info, terminal_output, terminal_exit)
    ///
    /// # Error Content Behavior
    ///
    /// When a tool fails, include `content` with the error message so Zed can display it:
    /// ```ignore
    /// let content: Option<Vec<ToolCallContent>> = if result.is_error {
    ///     Some(vec![result.content.clone().into()])
    /// } else {
    ///     None
    /// };
    /// ```
    ///
    /// For successful completion, `content` should be `None` to avoid duplicate output
    /// (the tool result is sent separately via the result message).
    pub(super) fn send_tool_call_update_with_meta(
        cx: &Cx,
        session_id: &str,
        tool_use_id: &str,
        status: Option<ToolCallStatus>,
        title: Option<&str>,
        content: Option<Vec<ToolCallContent>>,
        meta: Option<Meta>,
    ) -> Result<(), String> {
        // Build update fields
        let mut update_fields = ToolCallUpdateFields::new();

        if let Some(status) = status {
            update_fields = update_fields.status(status);
        }

        if let Some(title) = title {
            update_fields = update_fields.title(title);
        }

        if let Some(content) = content {
            update_fields = update_fields.content(content);
        }

        // Build and send notification
        let tool_call_id = ToolCallId::new(tool_use_id.to_string());
        let mut update = ToolCallUpdate::new(tool_call_id, update_fields);

        // Add meta if provided
        if let Some(meta) = meta {
            update = update.meta(meta);
        }

        let notification = SessionNotification::new(
            SessionId::new(session_id.to_string()),
            SessionUpdate::ToolCallUpdate(update),
        );

        cx.send_notification(notification)
            .map_err(|e| format!("Failed to send notification: {e}"))
    }

    /// Convert serde_json::Value to Meta (Map<String, Value>)
    pub(super) fn value_to_meta(value: serde_json::Value) -> Option<Meta> {
        match value {
            serde_json::Value::Object(map) => Some(map),
            _ => None,
        }
    }

    /// Send a ToolCall notification with meta field and terminal content
    ///
    /// IMPORTANT: This sends a ToolCall (not ToolCallUpdate) notification.
    /// Zed handles terminal in two ways:
    /// - meta.terminal_info - creates and registers the terminal
    /// - content[].Terminal - associates the terminal with the tool call UI
    ///
    /// Both are needed for terminal output to be displayed correctly.
    pub(super) fn send_tool_call_with_meta(
        cx: &Cx,
        session_id: &str,
        tool_use_id: &str,
        title: Option<&str>,
        status: ToolCallStatus,
        terminal_id: Option<&str>,
        meta: Option<Meta>,
    ) -> Result<(), String> {
        let tool_call_id = ToolCallId::new(tool_use_id.to_string());

        let mut tool_call = ToolCall::new(tool_call_id, title.unwrap_or("Running command"))
            .kind(ToolKind::Execute)
            .status(status);

        // Add terminal content to associate terminal with tool call UI
        if let Some(tid) = terminal_id {
            let terminal = Terminal::new(tid.to_string());
            tool_call = tool_call.content(vec![ToolCallContent::Terminal(terminal)]);
        }

        // Add meta if provided (contains terminal_info for creating the terminal)
        if let Some(meta) = meta {
            tool_call = tool_call.meta(meta);
        }

        let notification = SessionNotification::new(
            SessionId::new(session_id),
            SessionUpdate::ToolCall(tool_call),
        );

        cx.send_notification(notification)
            .map_err(|e| format!("Failed to send notification: {e}"))
    }
}

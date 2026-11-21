//! The ACP messages and tool-call cards built from the CLI's content blocks.

use super::*;

impl NotificationConverter {
    /// Make an agent message notification (full text as chunk)
    ///
    /// Currently unused because Text blocks are skipped in convert_assistant_message
    /// to avoid duplication with stream events.
    #[allow(dead_code, clippy::unused_self)]
    pub(super) fn make_agent_message(
        &self,
        session_id: &SessionId,
        text: &str,
    ) -> SessionNotification {
        // Use AgentMessageChunk since there's no AgentMessage variant
        let notification = SessionNotification::new(
            session_id.clone(),
            SessionUpdate::AgentMessageChunk(ContentChunk::new(AcpContentBlock::Text(
                TextContent::new(text),
            ))),
        );
        self.attach_request_id(notification)
    }

    /// Make an agent message chunk notification (incremental)
    #[allow(clippy::unused_self)]
    pub(super) fn make_agent_message_chunk(
        &self,
        session_id: &SessionId,
        chunk: &str,
    ) -> SessionNotification {
        let notification = SessionNotification::new(
            session_id.clone(),
            SessionUpdate::AgentMessageChunk(ContentChunk::new(AcpContentBlock::Text(
                TextContent::new(chunk),
            ))),
        );
        self.attach_request_id(notification)
    }

    /// Make an agent thought notification (full thought as chunk)
    ///
    /// Currently unused because Thinking blocks are skipped in convert_assistant_message
    /// to avoid duplication with stream events.
    #[allow(dead_code, clippy::unused_self)]
    pub(super) fn make_agent_thought(
        &self,
        session_id: &SessionId,
        thought: &str,
    ) -> SessionNotification {
        // Use AgentThoughtChunk since there's no separate thought variant
        let notification = SessionNotification::new(
            session_id.clone(),
            SessionUpdate::AgentThoughtChunk(ContentChunk::new(AcpContentBlock::Text(
                TextContent::new(thought),
            ))),
        );
        self.attach_request_id(notification)
    }

    /// Make an agent thought chunk notification (incremental)
    #[allow(clippy::unused_self)]
    pub(super) fn make_agent_thought_chunk(
        &self,
        session_id: &SessionId,
        chunk: &str,
    ) -> SessionNotification {
        let notification = SessionNotification::new(
            session_id.clone(),
            SessionUpdate::AgentThoughtChunk(ContentChunk::new(AcpContentBlock::Text(
                TextContent::new(chunk),
            ))),
        );
        self.attach_request_id(notification)
    }

    /// Make an image message notification
    ///
    /// Converts SDK ImageBlock to ACP ImageContent and wraps in AgentMessageChunk.
    /// Reference: vendors/claude-code-acp/src/acp-agent.ts lines 1027-1035
    #[allow(clippy::unused_self)]
    pub(super) fn make_image_message(
        &self,
        session_id: &SessionId,
        image: &ImageBlock,
    ) -> SessionNotification {
        let (data, mime_type, uri) = match &image.source {
            ImageSource::Base64 { media_type, data } => (data.clone(), media_type.clone(), None),
            ImageSource::Url { url } => {
                // For URL-based images, data is empty and uri is set
                (String::new(), String::new(), Some(url.clone()))
            }
        };

        let image_content = ImageContent::new(data, mime_type).uri(uri);

        let notification = SessionNotification::new(
            session_id.clone(),
            SessionUpdate::AgentMessageChunk(ContentChunk::new(AcpContentBlock::Image(
                image_content,
            ))),
        );
        self.attach_request_id(notification)
    }

    /// Map local ToolKind to ACP ToolKind
    pub(super) fn map_tool_kind(kind: ToolKind) -> AcpToolKind {
        match kind {
            ToolKind::Read => AcpToolKind::Read,
            ToolKind::Edit => AcpToolKind::Edit,
            ToolKind::Execute => AcpToolKind::Execute,
            ToolKind::Search => AcpToolKind::Search,
            ToolKind::Think => AcpToolKind::Think,
            ToolKind::Fetch => AcpToolKind::Fetch,
            ToolKind::SwitchMode | ToolKind::Other => AcpToolKind::default(),
        }
    }

    /// Make a tool call notification
    #[allow(clippy::unused_self)]
    pub(super) fn make_tool_call(
        &self,
        session_id: &SessionId,
        tool_use: &ToolUseBlock,
    ) -> SessionNotification {
        let tool_info = extract_tool_info(&tool_use.name, &tool_use.input, self.cwd.as_ref());

        let tool_call_id = ToolCallId::new(tool_use.id.clone());
        let tool_kind = Self::map_tool_kind(tool_info.kind);

        // For Bash tool, include command in title if description is not available
        let title = if tool_use.name == "Bash" {
            // Get description or command
            let description = tool_use.input.get("description").and_then(|v| v.as_str());
            let command = tool_use.input.get("command").and_then(|v| v.as_str());

            match (description, command) {
                (Some(desc), _) => desc.to_string(),
                (None, Some(cmd)) => {
                    // Truncate long commands for display (UTF-8 safe)
                    if cmd.len() > 80 {
                        let boundary = cmd
                            .char_indices()
                            .map(|(i, _)| i)
                            .take_while(|&i| i <= 77)
                            .last()
                            .unwrap_or(0);
                        format!("{}...", &cmd[..boundary])
                    } else {
                        cmd.to_string()
                    }
                }
                _ => tool_info.title.clone(),
            }
        } else {
            tool_info.title.clone()
        };

        // Debug: Log tool call creation
        tracing::debug!(
            tool_call_id = %tool_use.id,
            tool_name = %tool_use.name,
            title = %title,
            session_id = %session_id.0,
            "Creating ToolCall notification for session/update"
        );

        let mut tool_call = ToolCall::new(tool_call_id, &title)
            .kind(tool_kind)
            .status(ToolCallStatus::InProgress) // Tool is being executed
            .raw_input(tool_use.input.clone());

        // Nori's own tools say which they are, so the client need not parse the title; Claude's
        // subagent tool says a subagent runs in the turn.
        if let Some(host_tool) = crate::mcp::host_tool_name(&tool_use.name) {
            tool_call = tool_call.meta(nori_host_tools::tool_call_meta(host_tool));
        } else if matches!(tool_use.name.as_str(), "Agent" | "Task") {
            tool_call = tool_call.meta(nori_host_tools::subagent_meta());
        }

        // Add locations if present
        if let Some(ref locations) = tool_info.locations
            && !locations.is_empty()
        {
            let acp_locations: Vec<ToolCallLocation> = locations
                .iter()
                .map(|loc| {
                    let mut location = ToolCallLocation::new(&loc.path);
                    if let Some(line) = loc.line {
                        location = location.line(line);
                    }
                    location
                })
                .collect();
            tool_call = tool_call.locations(acp_locations);
        }

        // A call already announced (the CLI first shows it with empty input) gets its complete
        // input as an update: one `tool_call` per id.
        let update = if self.announced_tool_uses.insert(tool_use.id.clone()) {
            SessionUpdate::ToolCall(tool_call)
        } else {
            let mut fields = ToolCallUpdateFields::new()
                .title(tool_call.title)
                .kind(tool_call.kind)
                .raw_input(tool_call.raw_input);
            if !tool_call.locations.is_empty() {
                fields = fields.locations(tool_call.locations);
            }
            let mut update = ToolCallUpdate::new(tool_call.tool_call_id, fields);
            if let Some(meta) = tool_call.meta {
                update = update.meta(meta);
            }
            SessionUpdate::ToolCallUpdate(update)
        };
        let notification = SessionNotification::new(session_id.clone(), update);
        self.attach_request_id(notification)
    }

    /// Make tool result notifications
    ///
    /// Returns a vector of notifications:
    /// - ToolCallUpdate for all tools
    /// - Plan notification for TodoWrite tool (when successful)
    pub(super) fn make_tool_result(
        &self,
        session_id: &SessionId,
        tool_result: &ToolResultBlock,
    ) -> Vec<SessionNotification> {
        let Some(entry) = self.get_tool_use(&tool_result.tool_use_id) else {
            // Tool call not found in cache - this can happen if:
            // 1. Tool was never cached (bug in streaming handling)
            // 2. Tool was already processed and removed
            // 3. Duplicate tool result received
            tracing::warn!(
                session_id = %session_id.0,
                tool_use_id = %tool_result.tool_use_id,
                "Tool call not found in cache, skipping tool result notification"
            );
            return vec![];
        };
        self.finish_tool_use(&tool_result.tool_use_id);

        tracing::debug!(
            session_id = %session_id.0,
            tool_use_id = %tool_result.tool_use_id,
            tool_name = %entry.name,
            "Processing tool result notification"
        );

        let output = match &tool_result.content {
            Some(ToolResultContent::Text(text)) => text.clone(),
            // MCP results come as content blocks: their text is the output; anything else
            // (an image, say) is kept as JSON.
            Some(ToolResultContent::Blocks(blocks)) => blocks
                .iter()
                .map(
                    |block| match block.get("text").and_then(serde_json::Value::as_str) {
                        Some(text) if block["type"] == "text" => text.to_owned(),
                        _ => block.to_string(),
                    },
                )
                .collect::<Vec<_>>()
                .join("\n"),
            None => String::new(),
        };

        let is_error = tool_result.is_error.unwrap_or(false);
        let status = if is_error {
            ToolCallStatus::Failed
        } else {
            ToolCallStatus::Completed
        };

        // Build raw_output JSON
        let raw_output = serde_json::json!({
            "content": output,
            "is_error": is_error
        });

        // Build content based on tool type
        // A file the CLI's own tool edited goes out as a diff of the whole file; see
        // `EditSnapshots`. Anything else builds its content from the tool's name and input.
        let content = match self
            .edit_snapshots
            .diff(&tool_result.tool_use_id)
            .filter(|_| !is_error)
        {
            Some(diff) => vec![ToolCallContent::Diff(diff)],
            None => self.build_tool_result_content(&entry, &output, is_error),
        };

        let tool_call_id = ToolCallId::new(tool_result.tool_use_id.clone());
        let update_fields = ToolCallUpdateFields::new()
            .status(status)
            .content(content)
            .raw_output(raw_output);
        let update = ToolCallUpdate::new(tool_call_id, update_fields);

        let notification =
            SessionNotification::new(session_id.clone(), SessionUpdate::ToolCallUpdate(update));
        let notifications = vec![self.attach_request_id(notification)];

        // Note: Plan notification for TodoWrite is now sent at tool_use time
        // (in make_plan_from_todo_write), so we don't send it here anymore.
        // This matches TypeScript behavior: acp-agent.ts lines 1051-1058

        notifications
    }

    /// Make a Plan notification from TodoWrite tool_use
    ///
    /// This is called when we receive a TodoWrite tool_use, to send the Plan
    /// immediately (instead of waiting for tool_result).
    /// Reference: vendors/claude-code-acp/src/acp-agent.ts lines 1051-1058
    #[allow(clippy::unused_self)]
    pub(super) fn make_plan_from_todo_write(
        &self,
        session_id: &SessionId,
        tool_use: &ToolUseBlock,
    ) -> Option<SessionNotification> {
        // Extract todos from input
        let todos = tool_use.input.get("todos")?.as_array()?;

        let plan_entries: Vec<PlanEntry> = todos
            .iter()
            .filter_map(|todo| {
                let content = todo.get("content")?.as_str()?;
                let status_str = todo.get("status")?.as_str()?;

                // Convert TodoWrite status to PlanEntryStatus
                let status = match status_str {
                    "in_progress" => PlanEntryStatus::InProgress,
                    "completed" => PlanEntryStatus::Completed,
                    _ => PlanEntryStatus::Pending,
                };

                // TodoWrite doesn't have priority, default to Medium
                Some(PlanEntry::new(content, PlanEntryPriority::Medium, status))
            })
            .collect();

        if plan_entries.is_empty() {
            return None;
        }

        let plan = Plan::new(plan_entries);
        let notification = SessionNotification::new(session_id.clone(), SessionUpdate::Plan(plan));
        Some(self.attach_request_id(notification))
    }

    /// Build tool result content based on tool type
    ///
    /// For Edit/Write tools, returns Diff content.
    /// For Read tool, removes SYSTEM_REMINDER and wraps with markdown.
    /// For errors, wraps with markdown code block.
    /// Reference: vendors/claude-code-acp/src/tools.ts toolUpdateFromToolResult
    pub(super) fn build_tool_result_content(
        &self,
        entry: &ToolUseEntry,
        output: &str,
        is_error: bool,
    ) -> Vec<ToolCallContent> {
        // Strip mcp__acp__ prefix for matching
        let effective_name = entry.name.strip_prefix("mcp__acp__").unwrap_or(&entry.name);

        match effective_name {
            "Edit" if !is_error => {
                // Extract file_path, old_string, new_string from input
                let file_path = entry
                    .input
                    .get("file_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let old_string = entry
                    .input
                    .get("old_string")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let new_string = entry
                    .input
                    .get("new_string")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                if !file_path.is_empty() && !new_string.is_empty() {
                    // Create structured Diff content for Zed to render as visual diff
                    // Reference: vendors/claude-code-acp/src/tools.ts:460-507
                    let diff = Diff::new(file_path, new_string).old_text(old_string);
                    vec![ToolCallContent::Diff(diff)]
                } else {
                    vec![output.to_string().into()]
                }
            }
            "Write" if !is_error => {
                // Extract file_path and content from input
                let file_path = entry
                    .input
                    .get("file_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let content = entry
                    .input
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                if file_path.is_empty() {
                    vec![output.to_string().into()]
                } else {
                    // For Write, old_text is None (new file) or we don't have it
                    let diff = Diff::new(file_path, content);
                    vec![ToolCallContent::Diff(diff)]
                }
            }
            "Read" if !is_error => {
                // Remove SYSTEM_REMINDER and wrap with markdown
                // Reference: vendors/claude-code-acp/src/tools.ts:430-431
                let cleaned = remove_system_reminders(output);
                let wrapped = markdown_escape(&cleaned);
                vec![wrapped.into()]
            }
            _ if is_error => {
                // Wrap errors with markdown code block
                // Reference: vendors/claude-code-acp/src/tools.ts:553-556
                let wrapped = format!("```\n{output}\n```");
                vec![wrapped.into()]
            }
            _ => {
                // Default: text content
                vec![output.to_string().into()]
            }
        }
    }
}

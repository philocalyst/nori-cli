//! Claude SDK Message to ACP SessionNotification conversion
//!
//! Converts SDK messages (assistant, system, result, stream events)
//! into ACP session notifications for the client.

mod cards;
mod subagent;

use std::time::Instant;

use claude_code_agent_sdk::AssistantMessage;
use claude_code_agent_sdk::ContentBlock as SdkContentBlock;
use claude_code_agent_sdk::ImageBlock;
use claude_code_agent_sdk::ImageSource;
use claude_code_agent_sdk::Message;
use claude_code_agent_sdk::ResultMessage;
use claude_code_agent_sdk::StreamEvent;
use claude_code_agent_sdk::ToolResultBlock;
use claude_code_agent_sdk::ToolResultContent;
use claude_code_agent_sdk::ToolUseBlock;
use claude_code_agent_sdk::UserMessage;
use dashmap::DashMap;
use nori_acp_kit::schema::ContentBlock as AcpContentBlock;
use nori_acp_kit::schema::ContentChunk;
use nori_acp_kit::schema::Diff;
use nori_acp_kit::schema::ImageContent;
use nori_acp_kit::schema::Plan;
use nori_acp_kit::schema::PlanEntry;
use nori_acp_kit::schema::PlanEntryPriority;
use nori_acp_kit::schema::PlanEntryStatus;
use nori_acp_kit::schema::SessionId;
use nori_acp_kit::schema::SessionNotification;
use nori_acp_kit::schema::SessionUpdate;
use nori_acp_kit::schema::TextContent;
use nori_acp_kit::schema::ToolCall;
use nori_acp_kit::schema::ToolCallContent;
use nori_acp_kit::schema::ToolCallId;
use nori_acp_kit::schema::ToolCallLocation;
use nori_acp_kit::schema::ToolCallStatus;
use nori_acp_kit::schema::ToolCallUpdate;
use nori_acp_kit::schema::ToolCallUpdateFields;
use nori_acp_kit::schema::ToolKind as AcpToolKind;
use regex::Regex;

use crate::converter::EditSnapshots;
use crate::types::ToolKind;
use crate::types::ToolUseEntry;

use super::extract_tool_info;

/// Static regex for finding backtick sequences at start of lines
/// Used by markdown_escape to determine the appropriate escape sequence
#[allow(clippy::expect_used)] // The pattern is a constant that compiles.
static BACKTICK_REGEX: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r"(?m)^```+").expect("valid backtick regex"));

/// Static regex for removing SYSTEM_REMINDER blocks
/// Matches <system-reminder>...</system-reminder> including multiline content
#[allow(clippy::expect_used)] // The pattern is a constant that compiles.
static SYSTEM_REMINDER_REGEX: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?s)<system-reminder>.*?</system-reminder>").expect("valid system-reminder regex")
});

/// Wrap text in markdown code block with appropriate number of backticks
///
/// Ensures the text is safely wrapped by using more backticks than any sequence
/// found in the text itself.
///
/// Reference: vendors/claude-code-acp/src/tools.ts:591-599
fn markdown_escape(text: &str) -> String {
    let mut escape = "```".to_string();

    // Find all sequences of backticks at the start of lines
    for cap in BACKTICK_REGEX.captures_iter(text) {
        let m = &cap[0];
        while m.len() >= escape.len() {
            escape.push('`');
        }
    }

    // Build the final string
    let needs_newline = !text.ends_with('\n');
    format!(
        "{}\n{}{}{}",
        escape,
        text,
        if needs_newline { "\n" } else { "" },
        escape
    )
}

/// Remove SYSTEM_REMINDER tags and their content from text
///
/// Reference: vendors/claude-code-acp/src/tools.ts:430-431
fn remove_system_reminders(text: &str) -> String {
    SYSTEM_REMINDER_REGEX.replace_all(text, "").to_string()
}

/// Notification converter for transforming SDK messages to ACP notifications
///
/// Maintains a cache of tool uses to correlate tool_use blocks with their results.
#[derive(Debug)]
pub struct NotificationConverter {
    /// Cache of tool use entries, keyed by tool_use_id
    tool_use_cache: DashMap<String, ToolUseEntry>,
    /// The cached tool uses whose result has arrived.
    finished_tool_uses: dashmap::DashSet<String>,
    /// The tool uses already announced as a `tool_call`. The CLI shows each one twice, when it
    /// starts (with empty input) and when its input is complete; the second is an update.
    announced_tool_uses: dashmap::DashSet<String>,
    /// Current working directory for relative path display
    cwd: Option<std::path::PathBuf>,
    /// Optional request_id for tracking prompt requests
    request_id: Option<String>,
    /// The files the CLI's own editing tools are about to change, for reporting their diffs.
    edit_snapshots: std::sync::Arc<EditSnapshots>,
}

impl Default for NotificationConverter {
    fn default() -> Self {
        Self::new()
    }
}

impl NotificationConverter {
    /// Create a new notification converter
    pub fn new() -> Self {
        Self {
            tool_use_cache: DashMap::new(),
            finished_tool_uses: dashmap::DashSet::new(),
            announced_tool_uses: dashmap::DashSet::new(),
            cwd: None,
            request_id: None,
            edit_snapshots: std::sync::Arc::new(EditSnapshots::new(std::path::PathBuf::new())),
        }
    }

    /// Create a new notification converter with working directory
    ///
    /// # Arguments
    ///
    /// * `cwd` - The current working directory for computing relative paths
    pub fn with_cwd(cwd: std::path::PathBuf) -> Self {
        Self {
            tool_use_cache: DashMap::new(),
            finished_tool_uses: dashmap::DashSet::new(),
            announced_tool_uses: dashmap::DashSet::new(),
            edit_snapshots: std::sync::Arc::new(EditSnapshots::new(cwd.clone())),
            cwd: Some(cwd),
            request_id: None,
        }
    }

    /// Report the diffs of the edits whose files `snapshots` recorded before they ran.
    pub fn with_edit_snapshots(mut self, snapshots: std::sync::Arc<EditSnapshots>) -> Self {
        self.edit_snapshots = snapshots;
        self
    }

    /// Set the request_id for this converter
    ///
    /// The request_id will be attached to all SessionNotification instances
    /// created by this converter, allowing clients to track which responses
    /// correspond to which requests.
    ///
    /// # Arguments
    ///
    /// * `request_id` - The unique request identifier
    pub fn set_request_id(&mut self, request_id: String) {
        self.request_id = Some(request_id);
    }

    /// Clear the request_id
    pub fn clear_request_id(&mut self) {
        self.request_id = None;
    }

    /// Attach request_id to a notification if one is set
    fn attach_request_id(&self, notification: SessionNotification) -> SessionNotification {
        if let Some(ref req_id) = self.request_id {
            // Build Meta (serde_json::Map) with request_id
            let mut meta = serde_json::Map::new();
            meta.insert("request_id".to_string(), serde_json::json!(req_id));
            notification.meta(meta)
        } else {
            notification
        }
    }

    /// Convert a SDK Message to ACP session update notifications
    ///
    /// # Arguments
    ///
    /// * `message` - The SDK message to convert
    /// * `session_id` - The session ID for the notifications
    ///
    /// # Returns
    ///
    /// A vector of ACP SessionNotification objects
    pub fn convert_message(&self, message: &Message, session_id: &str) -> Vec<SessionNotification> {
        let start_time = Instant::now();

        // Determine message type for logging
        let message_type = match message {
            Message::Assistant(_) => "Assistant",
            Message::StreamEvent(_) => "StreamEvent",
            Message::Result(_) => "Result",
            Message::RateLimitEvent(_) => "RateLimitEvent",
            Message::System(_) => "System",
            Message::User(_) => "User",
            Message::ControlCancelRequest(_) => "ControlCancelRequest",
            Message::Unrecognized(_) => "Unrecognized",
        };

        let sid = SessionId::new(session_id.to_string());
        // A subagent's own messages are its card's, not the conversation's.
        if let Some(parent) = subagent::parent_of(message) {
            return self.convert_child_message(message, parent, &sid);
        }
        let notifications = match message {
            Message::Assistant(assistant) => self.convert_assistant_message(assistant, &sid),
            Message::StreamEvent(event) => self.convert_stream_event(event, &sid),
            Message::Result(result) => self.convert_result_message(result, &sid),
            Message::RateLimitEvent(_) => Vec::new(),
            Message::System(_) => {
                // System messages are typically internal, not sent as notifications
                vec![]
            }
            Message::User(user) => self.convert_user_message(user, &sid),
            Message::ControlCancelRequest(_) => {
                // Internal control messages
                vec![]
            }
            // A message type newer than this adapter: nothing it can show yet.
            Message::Unrecognized(_) => Vec::new(),
        };

        let elapsed = start_time.elapsed();
        let output_count = notifications.len();

        tracing::trace!(
            message_type = %message_type,
            session_id = %session_id,
            output_count = output_count,
            conversion_duration_us = elapsed.as_micros(),
            "Message conversion completed"
        );

        notifications
    }

    /// Convert a user message: Claude Code returns every tool's output in one, as `tool_result`
    /// blocks, so this is where tool calls finish. The ACP server's own tools (`mcp__acp__*`)
    /// report their completion as they run, so only the rest finish here. Anything else in the
    /// message is the CLI echoing input.
    fn convert_user_message(
        &self,
        user: &UserMessage,
        session_id: &SessionId,
    ) -> Vec<SessionNotification> {
        // The stream-json form keeps the blocks under `message.content`; the top-level field is
        // for messages the SDK built itself.
        let blocks = user.content.clone().or_else(|| {
            user.extra
                .get("message")
                .and_then(|message| message.get("content"))
                .and_then(|content| serde_json::from_value(content.clone()).ok())
        });
        blocks
            .unwrap_or_default()
            .iter()
            .filter_map(|block| match block {
                SdkContentBlock::ToolResult(result) => Some(result),
                _ => None,
            })
            .filter(|result| {
                self.get_tool_use(&result.tool_use_id)
                    .is_none_or(|entry| !entry.name.starts_with(crate::mcp::ACP_TOOL_PREFIX))
            })
            .flat_map(|result| self.make_tool_result(session_id, result))
            .collect()
    }

    /// Convert an assistant message
    ///
    /// Note: In streaming mode, Text and Thinking blocks are delivered via
    /// content_block_delta events (StreamEvent), so we skip them here to avoid
    /// sending the same content twice. Only ToolUse and ToolResult blocks are
    /// processed from non-streamed messages.
    fn convert_assistant_message(
        &self,
        assistant: &AssistantMessage,
        session_id: &SessionId,
    ) -> Vec<SessionNotification> {
        let mut notifications = Vec::new();

        for block in &assistant.message.content {
            match block {
                // Skip Text and Thinking blocks in streaming mode
                // They are delivered via StreamEvent::content_block_delta
                SdkContentBlock::Text(_) => {
                    // Skip - handled by stream events
                }
                SdkContentBlock::Thinking(_) => {
                    // Skip - handled by stream events
                }
                SdkContentBlock::ToolUse(tool_use) => {
                    // Cache the tool use for later correlation with result
                    self.cache_tool_use(tool_use);
                    // Special handling for TodoWrite: send Plan instead of ToolCall
                    // Reference: vendors/claude-code-acp/src/acp-agent.ts lines 1051-1058
                    let effective_name = tool_use
                        .name
                        .strip_prefix("mcp__acp__")
                        .unwrap_or(&tool_use.name);
                    if effective_name == "TodoWrite"
                        && let Some(notification) =
                            self.make_plan_from_todo_write(session_id, tool_use)
                    {
                        notifications.push(notification);
                        continue;
                    }
                    notifications.push(self.make_tool_call(session_id, tool_use));
                }
                SdkContentBlock::ToolResult(tool_result) => {
                    notifications.extend(self.make_tool_result(session_id, tool_result));
                }
                SdkContentBlock::Image(image) => {
                    // Convert SDK Image to ACP Image notification
                    // Reference: vendors/claude-code-acp/src/acp-agent.ts lines 1027-1035
                    notifications.push(self.make_image_message(session_id, image));
                }
            }
        }

        notifications
    }

    /// Convert a stream event (incremental updates)
    #[allow(clippy::unused_self)]
    fn convert_stream_event(
        &self,
        event: &StreamEvent,
        session_id: &SessionId,
    ) -> Vec<SessionNotification> {
        let event_type = event.event.get("type").and_then(|v| v.as_str());

        match event_type {
            Some("content_block_start") => {
                // Handle content_block_start - important for tool calls and results
                // When streaming is enabled, tool_use and tool_result blocks arrive via content_block_start
                if let Some(content_block) = event.event.get("content_block")
                    && let Some(block_type) = content_block.get("type").and_then(|v| v.as_str())
                {
                    // Handle tool_use types
                    // Reference: vendors/claude-code-acp/src/acp-agent.ts lines 1047-1049
                    if matches!(block_type, "tool_use" | "server_tool_use" | "mcp_tool_use") {
                        match serde_json::from_value::<ToolUseBlock>(content_block.clone()) {
                            Ok(tool_use) => {
                                self.cache_tool_use(&tool_use);
                                // Special handling for TodoWrite: send Plan instead of ToolCall
                                // Reference: vendors/claude-code-acp/src/acp-agent.ts lines 1051-1058
                                let effective_name = tool_use
                                    .name
                                    .strip_prefix("mcp__acp__")
                                    .unwrap_or(&tool_use.name);
                                if effective_name == "TodoWrite"
                                    && let Some(notification) =
                                        self.make_plan_from_todo_write(session_id, &tool_use)
                                {
                                    return vec![notification];
                                }
                                return vec![self.make_tool_call(session_id, &tool_use)];
                            }
                            Err(e) => {
                                tracing::error!(
                                    session_id = %session_id.0,
                                    block_type = %block_type,
                                    error = %e,
                                    "Failed to parse tool_use block"
                                );
                            }
                        }
                    }
                    // Handle tool_result types
                    // Reference: vendors/claude-code-acp/src/acp-agent.ts lines 1109-1116
                    else if matches!(
                        block_type,
                        "tool_result"
                            | "mcp_tool_result"
                            | "tool_search_tool_result"
                            | "web_fetch_tool_result"
                            | "web_search_tool_result"
                            | "code_execution_tool_result"
                            | "bash_code_execution_tool_result"
                            | "text_editor_code_execution_tool_result"
                    ) {
                        match serde_json::from_value::<ToolResultBlock>(content_block.clone()) {
                            Ok(tool_result) => {
                                return self.make_tool_result(session_id, &tool_result);
                            }
                            Err(e) => {
                                tracing::error!(
                                    session_id = %session_id.0,
                                    block_type = %block_type,
                                    error = %e,
                                    "Failed to parse tool_result block"
                                );
                            }
                        }
                    }
                    // Handle image type
                    // Reference: vendors/claude-code-acp/src/acp-agent.ts lines 1027-1035
                    else if block_type == "image" {
                        match serde_json::from_value::<ImageBlock>(content_block.clone()) {
                            Ok(image) => {
                                return vec![self.make_image_message(session_id, &image)];
                            }
                            Err(e) => {
                                tracing::error!(
                                    session_id = %session_id.0,
                                    block_type = %block_type,
                                    error = %e,
                                    "Failed to parse image block"
                                );
                            }
                        }
                    }
                    // Skip known non-notification types
                    // Reference: vendors/claude-code-acp/src/acp-agent.ts lines 1141-1148
                    else if matches!(
                        block_type,
                        "text"
                            | "thinking"
                            | "document"
                            | "search_result"
                            | "redacted_thinking"
                            | "input_json_delta"
                            | "citations_delta"
                            | "signature_delta"
                            | "container_upload"
                    ) {
                        // These are handled elsewhere or not needed as notifications
                    }
                    // Log unknown block types (like TS's unreachable)
                    else {
                        tracing::warn!(
                            session_id = %session_id.0,
                            block_type = %block_type,
                            content_block = ?content_block,
                            "Unknown content_block type in content_block_start"
                        );
                    }
                }
                vec![]
            }
            Some("content_block_delta") => {
                if let Some(delta) = event.event.get("delta") {
                    if let Some(delta_type) = delta.get("type").and_then(|v| v.as_str()) {
                        match delta_type {
                            "text_delta" => {
                                if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                                    return vec![self.make_agent_message_chunk(session_id, text)];
                                }
                            }
                            "thinking_delta" => {
                                if let Some(thinking) =
                                    delta.get("thinking").and_then(|v| v.as_str())
                                {
                                    return vec![
                                        self.make_agent_thought_chunk(session_id, thinking),
                                    ];
                                }
                            }
                            // Skip known delta types that don't need notifications
                            "input_json_delta" | "citations_delta" | "signature_delta" => {}
                            // Log unknown delta types
                            _ => {
                                tracing::debug!(
                                    session_id = %session_id.0,
                                    delta_type = %delta_type,
                                    "Unknown delta type in content_block_delta"
                                );
                            }
                        }
                    } else {
                        // Fallback for delta without explicit type field
                        if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                            return vec![self.make_agent_message_chunk(session_id, text)];
                        }
                        if let Some(thinking) = delta.get("thinking").and_then(|v| v.as_str()) {
                            return vec![self.make_agent_thought_chunk(session_id, thinking)];
                        }
                    }
                }
                vec![]
            }
            // No content needed for these events
            Some("content_block_stop" | "message_start" | "message_delta" | "message_stop") => {
                vec![]
            }
            // Log unknown event types (like TS's unreachable)
            Some(unknown_type) => {
                tracing::warn!(
                    session_id = %session_id.0,
                    event_type = %unknown_type,
                    "Unknown stream event type"
                );
                vec![]
            }
            None => vec![],
        }
    }

    /// Convert a result message
    fn convert_result_message(
        &self,
        result: &ResultMessage,
        session_id: &SessionId,
    ) -> Vec<SessionNotification> {
        // If there's an error, send it as a notification to inform the client
        if result.is_error {
            let error_text = result
                .result
                .as_deref()
                .unwrap_or("An error occurred during execution");

            let notification = SessionNotification::new(
                session_id.clone(),
                SessionUpdate::AgentMessageChunk(ContentChunk::new(AcpContentBlock::Text(
                    TextContent::new(format!("\n❌ Error: {error_text}\n")),
                ))),
            );

            vec![notification]
        } else {
            // Result messages update usage statistics but don't typically
            // generate notifications (the prompt response handles completion)
            vec![]
        }
    }

    /// Cache a tool use entry
    fn cache_tool_use(&self, tool_use: &ToolUseBlock) {
        let entry = ToolUseEntry::new(tool_use.name.clone(), tool_use.input.clone());
        self.tool_use_cache.insert(tool_use.id.clone(), entry);
    }

    /// Get a cached tool use entry
    pub fn get_tool_use(&self, tool_use_id: &str) -> Option<ToolUseEntry> {
        self.tool_use_cache.get(tool_use_id).map(|r| r.clone())
    }

    /// Note that the result of tool use `tool_use_id` arrived.
    fn finish_tool_use(&self, tool_use_id: &str) {
        self.finished_tool_uses.insert(tool_use_id.to_owned());
    }

    /// The tool uses of this turn that started and have no result: what a turn cut short (its
    /// CLI exited) leaves open, and has to end itself.
    pub fn unfinished_tool_uses(&self) -> Vec<String> {
        self.tool_use_cache
            .iter()
            .map(|entry| entry.key().clone())
            .filter(|id| !self.finished_tool_uses.contains(id))
            .collect()
    }

    /// Clear all cached tool uses
    pub fn clear_cache(&self) {
        self.tool_use_cache.clear();
        self.finished_tool_uses.clear();
        self.announced_tool_uses.clear();
        self.edit_snapshots.clear();
    }

    // === Notification builders ===
}

#[cfg(test)]
mod tests;

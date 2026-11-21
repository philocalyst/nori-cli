//! A Claude subagent's own messages: what its `Agent` (formerly `Task`) tool call does.
//!
//! The CLI streams a subagent's text, tool calls and tool results with the `Agent` call's id as
//! their `parent_tool_use_id`. They are not the conversation's: streamed into it, a subagent's
//! words would run into the parent's answer and its tools would read as the parent's. So nothing
//! of them is sent as such. The `Agent` call's card says what the subagent is doing now (its
//! description, then the tool it just started), and its answer arrives as the call's own result.

use claude_code_agent_sdk::ContentBlock as SdkContentBlock;
use claude_code_agent_sdk::Message;
use nori_acp_kit::schema::SessionId;
use nori_acp_kit::schema::SessionNotification;
use nori_acp_kit::schema::SessionUpdate;
use nori_acp_kit::schema::ToolCallId;
use nori_acp_kit::schema::ToolCallStatus;
use nori_acp_kit::schema::ToolCallUpdate;
use nori_acp_kit::schema::ToolCallUpdateFields;

use super::NotificationConverter;
use crate::converter::extract_tool_info;

/// The tool call a message belongs to when a subagent sent it.
pub(super) fn parent_of(message: &Message) -> Option<&str> {
    match message {
        Message::Assistant(assistant) => assistant.parent_tool_use_id.as_deref(),
        Message::StreamEvent(event) => event.parent_tool_use_id.as_deref(),
        Message::User(user) => user.parent_tool_use_id.as_deref(),
        Message::Result(_)
        | Message::RateLimitEvent(_)
        | Message::System(_)
        | Message::ControlCancelRequest(_)
        | Message::Unrecognized(_) => None,
    }
}

impl NotificationConverter {
    /// A subagent's message, as what its parent's card shows: the tool it started, if any.
    pub(super) fn convert_child_message(
        &self,
        message: &Message,
        parent: &str,
        session_id: &SessionId,
    ) -> Vec<SessionNotification> {
        let Message::Assistant(assistant) = message else {
            return Vec::new();
        };
        let Some(started) = assistant
            .message
            .content
            .iter()
            .rev()
            .find_map(|block| match block {
                SdkContentBlock::ToolUse(tool_use) => Some(tool_use),
                _ => None,
            })
        else {
            return Vec::new();
        };
        let doing = extract_tool_info(&started.name, &started.input, self.cwd.as_ref()).title;
        let task = self
            .get_tool_use(parent)
            .and_then(|entry| {
                entry
                    .input
                    .get("description")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "Subagent".to_owned());
        vec![SessionNotification::new(
            session_id.clone(),
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::new(parent.to_owned()),
                ToolCallUpdateFields::new()
                    .title(format!("{task} · {doing}"))
                    .status(ToolCallStatus::InProgress),
            )),
        )]
    }
}

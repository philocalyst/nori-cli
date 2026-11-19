//! Tool calls that are not shell commands or patches: Nori's host tools (as Codex dynamic
//! tools) and MCP tools.

use std::sync::Arc;

use crate::thread::client::SessionClient;
use crate::thread::prompt::PromptState;
use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem;
use codex_protocol::mcp::CallToolResult;
use codex_protocol::protocol::DynamicToolCallResponseEvent;
use codex_protocol::protocol::McpInvocation;
use nori_acp_kit::schema::AudioContent;
use nori_acp_kit::schema::Content;
use nori_acp_kit::schema::ContentBlock;
use nori_acp_kit::schema::Meta;
use nori_acp_kit::schema::ResourceLink;
use nori_acp_kit::schema::ToolCall;
use nori_acp_kit::schema::ToolCallContent;
use nori_acp_kit::schema::ToolCallStatus;
use nori_acp_kit::schema::ToolCallUpdate;
use nori_acp_kit::schema::ToolCallUpdateFields;

impl PromptState {
    pub(super) fn start_dynamic_tool_call(
        &self,
        client: &SessionClient,
        call_id: String,
        tool: String,
        arguments: serde_json::Value,
    ) {
        // With host tools the call is Nori's own: it says which, and Nori's code answers it.
        let call = match &self.host_tools {
            Some(_) => ToolCall::new(call_id.clone(), tool.clone())
                .meta(nori_host_tools::tool_call_meta(&tool)),
            None => ToolCall::new(call_id.clone(), format!("Tool: {tool}")),
        };
        client.send_tool_call(
            call.status(ToolCallStatus::InProgress)
                .raw_input(arguments.clone()),
        );
        crate::host_tools::answer(
            self.host_tools.clone(),
            Arc::clone(&self.thread),
            call_id,
            tool,
            arguments,
            client.session_id.to_string(),
            Arc::clone(&self.host_goal_contexts),
            self.host_tool_cancellation.clone(),
        );
    }

    pub(super) fn start_mcp_tool_call(
        &self,
        client: &SessionClient,
        call_id: String,
        invocation: McpInvocation,
    ) {
        let title = format!("Tool: {}/{}", invocation.server, invocation.tool);
        client.send_tool_call(
            ToolCall::new(call_id, title)
                .status(ToolCallStatus::InProgress)
                .raw_input(serde_json::json!(&invocation)),
        );
    }

    pub(super) fn end_dynamic_tool_call(
        &self,
        client: &SessionClient,
        mut event: DynamicToolCallResponseEvent,
    ) {
        let context = self
            .host_goal_contexts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&event.call_id);
        let context = context.filter(|context| {
            crate::host_tools::remove_goal_context(&mut event.content_items, context)
        });
        let raw_output = serde_json::json!(event);
        let DynamicToolCallResponseEvent {
            call_id,
            turn_id: _,
            tool: _,
            arguments: _,
            completed_at_ms: _,
            namespace: _,
            content_items,
            success,
            error,
            duration: _,
        } = event;

        let mut update = ToolCallUpdate::new(
            call_id,
            ToolCallUpdateFields::new()
                .status(if success {
                    ToolCallStatus::Completed
                } else {
                    ToolCallStatus::Failed
                })
                .raw_output(raw_output)
                .content(
                    content_items
                        .into_iter()
                        .map(|item| match item {
                            DynamicToolCallOutputContentItem::InputText { text } => {
                                ToolCallContent::Content(Content::new(text))
                            }
                            DynamicToolCallOutputContentItem::InputImage { image_url } => {
                                ToolCallContent::Content(Content::new(ContentBlock::ResourceLink(
                                    ResourceLink::new(image_url.clone(), image_url),
                                )))
                            }
                            DynamicToolCallOutputContentItem::InputAudio { audio_url } => {
                                let block = match audio_url
                                    .strip_prefix("data:")
                                    .and_then(|value| value.split_once(";base64,"))
                                {
                                    Some((mime, data)) => {
                                        ContentBlock::Audio(AudioContent::new(data, mime))
                                    }
                                    None => ContentBlock::ResourceLink(ResourceLink::new(
                                        audio_url.clone(),
                                        audio_url,
                                    )),
                                };
                                ToolCallContent::Content(Content::new(block))
                            }
                        })
                        .chain(error.map(|e| ToolCallContent::Content(Content::new(e))))
                        .collect::<Vec<_>>(),
                ),
        );
        if let Some(context) = context {
            update = update.meta(Meta::from_iter([(
                "nori/modelContext".to_owned(),
                serde_json::json!(context),
            )]));
        }
        client.send_tool_call_update(update);
    }

    pub(super) fn end_mcp_tool_call(
        &self,
        client: &SessionClient,
        call_id: String,
        result: Result<CallToolResult, String>,
    ) {
        let is_error = match result.as_ref() {
            Ok(result) => result.is_error.unwrap_or_default(),
            Err(_) => true,
        };
        let raw_output = match result.as_ref() {
            Ok(result) => serde_json::json!(result),
            Err(err) => serde_json::json!(err),
        };

        client.send_tool_call_update(ToolCallUpdate::new(
            call_id,
            ToolCallUpdateFields::new()
                .status(if is_error {
                    ToolCallStatus::Failed
                } else {
                    ToolCallStatus::Completed
                })
                .raw_output(raw_output)
                .content(
                    result
                        .ok()
                        .filter(|result| !result.content.is_empty())
                        .map(|result| {
                            result
                                .content
                                .into_iter()
                                .filter_map(|content| {
                                    serde_json::from_value::<ContentBlock>(content).ok()
                                })
                                .map(|content| ToolCallContent::Content(Content::new(content)))
                                .collect()
                        }),
                ),
        ));
    }
}

//! A recorded conversation sent back to the client as the updates it first saw (`session/load`).

use std::collections::HashMap;

use nori_acp_kit::AgentError;
use nori_acp_kit::Sink;
use nori_acp_kit::schema::ContentBlock;
use nori_acp_kit::schema::ContentChunk;
use nori_acp_kit::schema::ImageContent;
use nori_acp_kit::schema::SessionUpdate;
use nori_acp_kit::schema::ToolCall;
use nori_acp_kit::schema::ToolCallStatus;
use nori_acp_kit::schema::ToolCallUpdate;
use nori_acp_kit::schema::ToolCallUpdateFields;
use nori_llm::Message;
use nori_llm::Part;
use nori_llm::Role;

pub(crate) fn replay(sink: &Sink, messages: &[Message]) -> Result<(), AgentError> {
    let mut names: HashMap<String, String> = HashMap::new();
    for message in messages {
        for part in &message.parts {
            match (message.role, part) {
                (Role::User, Part::Text { text }) => {
                    sink.update(SessionUpdate::UserMessageChunk(ContentChunk::new(
                        ContentBlock::from(text.clone()),
                    )))?
                }
                (Role::User, Part::Image { media_type, data }) => {
                    sink.update(SessionUpdate::UserMessageChunk(ContentChunk::new(
                        ContentBlock::Image(ImageContent::new(data.clone(), media_type.clone())),
                    )))?;
                }
                (Role::Assistant, Part::Text { text }) => sink.text(text.clone())?,
                (Role::Assistant, Part::Reasoning(reasoning)) if !reasoning.text.is_empty() => {
                    sink.thought(reasoning.text.clone())?;
                }
                (_, Part::ToolCall(call)) => {
                    names.insert(call.id.clone(), call.name.clone());
                    sink.tool_call(
                        ToolCall::new(call.id.clone(), call.name.clone())
                            .status(ToolCallStatus::InProgress)
                            .raw_input(call.input.clone()),
                    )?;
                }
                (_, Part::ToolResult(result)) => {
                    let text = result.text_content();
                    let status = if result.is_error {
                        ToolCallStatus::Failed
                    } else {
                        ToolCallStatus::Completed
                    };
                    let mut fields = ToolCallUpdateFields::new().status(status);
                    if !text.is_empty() {
                        fields = fields.content(vec![text.into()]);
                    }
                    if let Some(name) = names.get(&result.id) {
                        fields = fields.title(name.clone());
                    }
                    sink.tool_update(ToolCallUpdate::new(result.id.clone(), fields))?;
                }
                (Role::Assistant, Part::Reasoning(_) | Part::Image { .. })
                | (Role::User, Part::Reasoning(_)) => {}
            }
        }
    }
    Ok(())
}

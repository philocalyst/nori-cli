//! Agent activity: message chunks, plans and tool calls.
//!
//! Activity is accounted to the open turn. With no turn open it opens an `Agent` turn, so
//! nothing the agent does is dropped or warned about.

use nori_protocol::AgentTurnCause;
use nori_protocol::acp::v1 as acp;

use super::super::session_reducer::ReduceOutput;
use super::ledger;
use super::quiescence;
use crate::normalized::ClientEvent;
use crate::normalized::ClientEventNormalizer;
use crate::normalized::session_runtime::SessionRuntime;
use nori_protocol::MessageFlow;
use nori_protocol::MessageStream;

pub(in crate::backend) fn reduce(
    runtime: &mut SessionRuntime,
    update: &acp::SessionUpdate,
    normalizer: &mut ClientEventNormalizer,
    out: &mut ReduceOutput,
) {
    let turn = ledger::agent_turn(runtime, AgentTurnCause::Unprompted, out);
    quiescence::touch(turn, out);
    let chunk = match update {
        acp::SessionUpdate::AgentMessageChunk(chunk) => Some((chunk, MessageStream::Answer)),
        acp::SessionUpdate::AgentThoughtChunk(chunk) => Some((chunk, MessageStream::Reasoning)),
        acp::SessionUpdate::UserMessageChunk(chunk) => Some((chunk, MessageStream::User)),
        _ => None,
    };
    if let Some((chunk, stream)) = chunk
        && let acp::ContentBlock::Text(text) = &chunk.content
    {
        // Prose before a warning the backend carried in the stream is still the agent's; the
        // warning itself never becomes part of any message.
        let prose = text
            .text
            .split_once(nori_protocol::AGENT_WARNING_PREFIX)
            .map_or(text.text.as_str(), |(prose, _)| prose);
        if !prose.is_empty() {
            let message_id = chunk.message_id.as_ref().map(ToString::to_string);
            turn.work
                .messages
                .push(stream, message_id.as_deref(), prose);
        }
    }
    if nori_protocol::update_flow(update) == MessageFlow::Boundary {
        turn.work.messages.close();
    }
    match update {
        acp::SessionUpdate::ToolCall(tool_call) => {
            turn.work
                .track_tool(&tool_call.tool_call_id.to_string(), Some(tool_call.status));
        }
        acp::SessionUpdate::ToolCallUpdate(tool_update) => {
            turn.work.track_tool(
                &tool_update.tool_call_id.to_string(),
                tool_update.fields.status,
            );
        }
        _ => {}
    }
    let owner_request_id = turn.request_id().map(ToString::to_string);

    out.events
        .extend(
            normalizer
                .push_session_update(update)
                .into_iter()
                .map(|event| match event {
                    ClientEvent::ToolSnapshot(mut snapshot) => {
                        if snapshot.owner_request_id.is_none() {
                            snapshot.owner_request_id = owner_request_id.clone();
                        }
                        ClientEvent::ToolSnapshot(snapshot)
                    }
                    other => other,
                }),
        );
}

/// Only these updates are agent activity; anything else a newer protocol adds does not open a
/// turn.
pub(in crate::backend) fn is_turn_activity(update: &acp::SessionUpdate) -> bool {
    matches!(
        update,
        acp::SessionUpdate::AgentMessageChunk(_)
            | acp::SessionUpdate::AgentThoughtChunk(_)
            | acp::SessionUpdate::UserMessageChunk(_)
            | acp::SessionUpdate::Plan(_)
            | acp::SessionUpdate::ToolCall(_)
            | acp::SessionUpdate::ToolCallUpdate(_)
    )
}

//! Agent activity: message chunks, plans and tool calls.
//!
//! Activity is accounted to the open turn. With no turn open it opens an `Agent` turn, so
//! nothing the agent does is dropped or warned about.

use nori_protocol::AgentTurnCause;
use nori_protocol::acp::v1 as acp;

use super::super::session_reducer::ReduceOutput;
use super::ledger;
use super::loops;
use super::loops::CallObservation;
use super::quiescence;
use crate::normalized::ClientEvent;
use crate::normalized::ClientEventNormalizer;
use crate::normalized::session_runtime::SessionRuntime;
use crate::normalized::turn::TurnOwner;
use nori_protocol::MessageFlow;
use nori_protocol::MessageStream;
use nori_workflow::LoopAction;

pub(in crate::backend) fn reduce(
    runtime: &mut SessionRuntime,
    update: &acp::SessionUpdate,
    normalizer: &mut ClientEventNormalizer,
    out: &mut ReduceOutput,
) {
    let policy = runtime.loops.policy;
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
    // A replayed history, or a turn already being cancelled, is not examined for loops.
    let watching = policy.action != LoopAction::Off
        && !turn.is_cancelling()
        && !matches!(turn.owner, TurnOwner::Replay { .. });
    let mut found_loop = None;
    match update {
        acp::SessionUpdate::ToolCall(tool_call) => {
            let call_id = tool_call.tool_call_id.to_string();
            let subagent = nori_host_tools::is_subagent(tool_call.meta.as_ref());
            turn.work
                .track_tool(&call_id, Some(tool_call.status), subagent);
            if watching {
                found_loop = turn.work.loops.observe(
                    &policy,
                    CallObservation {
                        call_id: &call_id,
                        kind: Some(tool_call.kind),
                        title: Some(&tool_call.title),
                        raw_input: tool_call.raw_input.as_ref(),
                    },
                );
            }
        }
        acp::SessionUpdate::ToolCallUpdate(tool_update) => {
            let call_id = tool_update.tool_call_id.to_string();
            let subagent = nori_host_tools::is_subagent(tool_update.meta.as_ref());
            turn.work
                .track_tool(&call_id, tool_update.fields.status, subagent);
            if watching {
                found_loop = turn.work.loops.observe(
                    &policy,
                    CallObservation {
                        call_id: &call_id,
                        kind: tool_update.fields.kind,
                        title: tool_update.fields.title.as_deref(),
                        raw_input: tool_update.fields.raw_input.as_ref(),
                    },
                );
            }
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
    if let Some(found) = found_loop {
        loops::react(runtime, &found, out);
    }
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

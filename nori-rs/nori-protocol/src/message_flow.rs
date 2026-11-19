//! What an event does to the message being streamed.
//!
//! This is the one definition of a message boundary. A message is a run of consecutive text
//! chunks of one stream with nothing visible between them. The session's turn assembles
//! messages with it, and the harness marks response boundaries in the journal with it, so the
//! two can never disagree about where one message ends.

use serde::Deserialize;
use serde::Serialize;

use crate::AGENT_WARNING_PREFIX;
use crate::AcpEvent;
use crate::NoriEvent;
use crate::SessionEvent;
use crate::acp::v1 as acp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageStream {
    User,
    Answer,
    Reasoning,
}

/// How an event relates to the open message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageFlow {
    /// A chunk of this stream. It continues the open message if that is the same stream.
    Chunk(MessageStream),
    /// Something visible between messages in this actor's own timeline: its tool call, plan or
    /// request, or a notice the harness shows at this point of its turn. It ends the open
    /// message, and the next chunk starts a new one.
    Boundary,
    /// Not part of the message's timeline, so it does not split a message: session facts nobody
    /// sees in the transcript (usage, mode changes), and activity of *other* actors. A child's
    /// state update or a delivered task result arrives while the parent's answer is still
    /// streaming and is shown after the message; ending the message there would split prose
    /// mid-sentence. The rule is about whose timeline an event belongs to, not whether it is
    /// visible.
    Transparent,
}

/// `_meta.nori.interrupted`: the session closed a tool call its turn left unfinished. Such an
/// update is bookkeeping, never agent activity: it must not open a turn, count as activity,
/// start a presentation turn, or split a message.
pub fn is_interruption_close(update: &acp::SessionUpdate) -> bool {
    let acp::SessionUpdate::ToolCallUpdate(update) = update else {
        return false;
    };
    update
        .meta
        .as_ref()
        .and_then(|meta| meta.get("nori"))
        .and_then(|nori| nori.get("interrupted"))
        .is_some()
}

pub fn update_flow(update: &acp::SessionUpdate) -> MessageFlow {
    if is_interruption_close(update) {
        return MessageFlow::Transparent;
    }
    let chunk = |chunk: &acp::ContentChunk, stream| match &chunk.content {
        // A warning the backend carried in the text stream ends the prose before it.
        acp::ContentBlock::Text(text) if text.text.contains(AGENT_WARNING_PREFIX) => {
            MessageFlow::Boundary
        }
        acp::ContentBlock::Text(_) => MessageFlow::Chunk(stream),
        _ => MessageFlow::Boundary,
    };
    match update {
        acp::SessionUpdate::AgentMessageChunk(content) => chunk(content, MessageStream::Answer),
        acp::SessionUpdate::AgentThoughtChunk(content) => chunk(content, MessageStream::Reasoning),
        acp::SessionUpdate::UserMessageChunk(content) => chunk(content, MessageStream::User),
        acp::SessionUpdate::Plan(_)
        | acp::SessionUpdate::ToolCall(_)
        | acp::SessionUpdate::ToolCallUpdate(_) => MessageFlow::Boundary,
        _ => MessageFlow::Transparent,
    }
}

pub fn message_flow(event: &SessionEvent) -> MessageFlow {
    match event {
        SessionEvent::Acp(AcpEvent::Notification(acp::AgentNotification::SessionNotification(
            notification,
        ))) => update_flow(&notification.update),
        SessionEvent::Acp(AcpEvent::Request { .. }) => MessageFlow::Boundary,
        SessionEvent::Acp(AcpEvent::Response { response, .. }) => match response {
            Ok(acp::AgentResponse::PromptResponse(_)) => MessageFlow::Boundary,
            Ok(_) | Err(_) => MessageFlow::Transparent,
        },
        SessionEvent::Acp(AcpEvent::Notification(_) | AcpEvent::ChildNotification(_)) => {
            MessageFlow::Transparent
        }
        SessionEvent::Nori(event) => match event {
            NoriEvent::ContextCompacted(_)
            | NoriEvent::SessionForked(_)
            | NoriEvent::SessionEnded(_)
            | NoriEvent::Undo(_)
            | NoriEvent::UserShell(_)
            | NoriEvent::HookOutput(_)
            | NoriEvent::Notice(_)
            | NoriEvent::RequestFailed(_)
            | NoriEvent::RouteChanged(_)
            | NoriEvent::RetryScheduled(_)
            | NoriEvent::AttemptSuperseded(_)
            | NoriEvent::SubscriptionExhausted(_)
            | NoriEvent::AgentContinued(_)
            | NoriEvent::RequestDeclined(_)
            | NoriEvent::PromptUnanswered(_) => MessageFlow::Boundary,
            NoriEvent::SessionStarted(_)
            | NoriEvent::SessionReady(_)
            | NoriEvent::ConversationResponseBoundary(_)
            | NoriEvent::SessionPhaseChanged(_)
            | NoriEvent::QueueChanged(_)
            | NoriEvent::SubagentQueueChanged { .. }
            | NoriEvent::ReplayStarted(_)
            | NoriEvent::ReplayFinished
            | NoriEvent::GoalChanged(_)
            | NoriEvent::CapabilitiesChanged(_)
            | NoriEvent::PromptSummaryUpdated(_)
            | NoriEvent::QueueRestored(_)
            // Activity of other actors never lands inside the words of this one: the client
            // shows it after the message, so it does not end the message.
            | NoriEvent::Subagent(_)
            | NoriEvent::TaskResultsDelivered(_)
            | NoriEvent::Monitor(_)
            | NoriEvent::AgentSilent(_)
            | NoriEvent::AgentHeard
            | NoriEvent::RequestWithdrawn(_) => MessageFlow::Transparent,
        },
    }
}

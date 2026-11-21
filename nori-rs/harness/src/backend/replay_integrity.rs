//! What a replayed journal says about a session that did not end cleanly.
//!
//! A session that crashed or was quit mid-turn leaves a journal with tool calls that never
//! finished and, perhaps, a prompt that never got an answer. Replaying it as written would show
//! spinners nothing can finish. Instead the replay closes each unfinished tool call as
//! interrupted, exactly as the live session would have if it had lived to see the turn end, and
//! offers the unanswered prompt back.

use nori_protocol::AcpEvent;
use nori_protocol::NoriEvent;
use nori_protocol::PromptUnanswered;
use nori_protocol::SessionEvent;
use nori_protocol::acp::v1 as acp;

pub(super) struct ReplayIntegrity {
    session_id: String,
    open_tools: Vec<String>,
    unanswered: Option<String>,
}

impl ReplayIntegrity {
    pub(super) fn new(session_id: String) -> Self {
        Self {
            session_id,
            open_tools: Vec::new(),
            unanswered: None,
        }
    }

    /// Note an event of the journal as it is replayed.
    pub(super) fn observe(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::Acp(AcpEvent::Notification(
                acp::AgentNotification::SessionNotification(notification),
            )) => match &notification.update {
                acp::SessionUpdate::ToolCall(call) => {
                    // The agent did something about the prompt: it was not left unanswered.
                    self.unanswered = None;
                    self.track(call.tool_call_id.to_string(), Some(call.status));
                }
                acp::SessionUpdate::ToolCallUpdate(update) => {
                    self.track(update.tool_call_id.to_string(), update.fields.status);
                }
                acp::SessionUpdate::AgentMessageChunk(_)
                | acp::SessionUpdate::AgentThoughtChunk(_) => self.unanswered = None,
                _ => {}
            },
            // Any outcome of the prompt, including a failure the user saw, answers it.
            SessionEvent::Acp(AcpEvent::Response {
                response: Ok(acp::AgentResponse::PromptResponse(_)) | Err(_),
                ..
            })
            | SessionEvent::Nori(NoriEvent::RequestFailed(_)) => self.unanswered = None,
            _ => {}
        }
    }

    fn track(&mut self, call_id: String, status: Option<acp::ToolCallStatus>) {
        let position = self.open_tools.iter().position(|open| *open == call_id);
        match (status, position) {
            (Some(acp::ToolCallStatus::Completed | acp::ToolCallStatus::Failed), Some(index)) => {
                self.open_tools.remove(index);
            }
            (Some(acp::ToolCallStatus::Completed | acp::ToolCallStatus::Failed), None) => {}
            (_, None) => self.open_tools.push(call_id),
            (_, Some(_)) => {}
        }
    }

    /// A new user prompt begins: whatever the turn before it left running is over.
    pub(super) fn user_prompt(&mut self, text: &str) -> Vec<SessionEvent> {
        let closes = self.close_open_tools();
        self.unanswered = Some(text.to_owned());
        closes
    }

    /// The journal ends. `offer_prompt` is whether a prompt that never got an answer is offered
    /// back (only the user's own conversation has one).
    pub(super) fn finish(&mut self, offer_prompt: bool) -> Vec<SessionEvent> {
        let mut events = self.close_open_tools();
        if offer_prompt && let Some(text) = self.unanswered.take() {
            events.push(SessionEvent::Nori(NoriEvent::PromptUnanswered(
                PromptUnanswered { text },
            )));
        }
        events
    }

    fn close_open_tools(&mut self) -> Vec<SessionEvent> {
        let meta = acp::Meta::from_iter([(
            "nori".to_string(),
            serde_json::json!({ "interrupted": "connection_lost" }),
        )]);
        std::mem::take(&mut self.open_tools)
            .into_iter()
            .map(|call_id| {
                let update = acp::SessionUpdate::ToolCallUpdate(
                    acp::ToolCallUpdate::new(
                        acp::ToolCallId::new(call_id),
                        acp::ToolCallUpdateFields::new().status(acp::ToolCallStatus::Failed),
                    )
                    .meta(meta.clone()),
                );
                SessionEvent::Acp(AcpEvent::Notification(
                    acp::AgentNotification::SessionNotification(acp::SessionNotification::new(
                        self.session_id.clone(),
                        update,
                    )),
                ))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn answer() -> SessionEvent {
        SessionEvent::Acp(AcpEvent::Notification(
            acp::AgentNotification::SessionNotification(acp::SessionNotification::new(
                "session",
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    acp::ContentBlock::Text(acp::TextContent::new("answer")),
                )),
            )),
        ))
    }

    #[test]
    fn a_prompt_the_agent_began_answering_is_not_offered_back() {
        let mut integrity = ReplayIntegrity::new("session".into());
        integrity.user_prompt("question");
        integrity.observe(&answer());
        assert!(integrity.finish(true).is_empty());
    }

    #[test]
    fn a_prompt_with_no_output_after_it_is_offered_back() {
        let mut integrity = ReplayIntegrity::new("session".into());
        integrity.user_prompt("question");
        let offered = integrity.finish(true);
        assert!(matches!(
            offered.as_slice(),
            [SessionEvent::Nori(NoriEvent::PromptUnanswered(unanswered))]
                if unanswered.text == "question"
        ));
    }
}

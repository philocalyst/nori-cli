//! Conflation of agent text deltas.
//!
//! Between the agent process and the screen a delta crosses several tasks. When the screen is
//! slower than the agent, deltas queue up, and handling each one separately would make the cost of
//! a frame depend on the token rate. Merging adjacent text deltas of one message before they reach
//! the UI makes it depend on the number of messages instead, and changes nothing the user sees: the
//! text, its order, and its position relative to every other event are preserved.

use nori_protocol::AcpEvent;
use nori_protocol::SessionEvent;
use nori_protocol::acp::v1 as acp;

/// Merge runs of adjacent text chunks of the same message and stream.
pub(crate) fn conflate(events: Vec<SessionEvent>) -> Vec<SessionEvent> {
    let mut out: Vec<SessionEvent> = Vec::with_capacity(events.len());
    for event in events {
        if let Some(previous) = out.last_mut()
            && merge_text_chunk(previous, &event)
        {
            continue;
        }
        out.push(event);
    }
    out
}

/// A text chunk of the agent's message or thoughts that carries nothing but text.
fn plain_text_chunk(event: &SessionEvent) -> Option<(&acp::SessionNotification, bool, &str)> {
    let SessionEvent::Acp(AcpEvent::Notification(acp::AgentNotification::SessionNotification(
        notification,
    ))) = event
    else {
        return None;
    };
    if notification.meta.is_some() {
        return None;
    }
    let (chunk, is_thought) = match &notification.update {
        acp::SessionUpdate::AgentMessageChunk(chunk) => (chunk, false),
        acp::SessionUpdate::AgentThoughtChunk(chunk) => (chunk, true),
        _ => return None,
    };
    let acp::ContentBlock::Text(text) = &chunk.content else {
        return None;
    };
    (chunk.meta.is_none() && text.annotations.is_none() && text.meta.is_none()).then_some((
        notification,
        is_thought,
        text.text.as_str(),
    ))
}

/// Append `next` to `previous` when both are plain text chunks of one message.
fn merge_text_chunk(previous: &mut SessionEvent, next: &SessionEvent) -> bool {
    let Some((previous_notification, previous_is_thought, previous_text)) =
        plain_text_chunk(previous)
    else {
        return false;
    };
    let Some((next_notification, next_is_thought, next_text)) = plain_text_chunk(next) else {
        return false;
    };
    // A notice the adapter carried in the text stream is a unit of its own. Merged into the text
    // around it, its marker would no longer start a chunk and the notice would be shown as part
    // of the model's words.
    if previous_text.contains(nori_protocol::AGENT_WARNING_PREFIX)
        || next_text.contains(nori_protocol::AGENT_WARNING_PREFIX)
    {
        return false;
    }
    let same_message = |notification: &acp::SessionNotification| match &notification.update {
        acp::SessionUpdate::AgentMessageChunk(chunk)
        | acp::SessionUpdate::AgentThoughtChunk(chunk) => chunk.message_id.clone(),
        _ => None,
    };
    if previous_is_thought != next_is_thought
        || previous_notification.session_id != next_notification.session_id
        || same_message(previous_notification) != same_message(next_notification)
    {
        return false;
    }
    let next_text = next_text.to_owned();
    let SessionEvent::Acp(AcpEvent::Notification(acp::AgentNotification::SessionNotification(
        notification,
    ))) = previous
    else {
        return false;
    };
    if let acp::SessionUpdate::AgentMessageChunk(chunk)
    | acp::SessionUpdate::AgentThoughtChunk(chunk) = &mut notification.update
        && let acp::ContentBlock::Text(text) = &mut chunk.content
    {
        text.text.push_str(&next_text);
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn chunk(kind: &str, message_id: Option<&str>, text: &str) -> SessionEvent {
        let mut chunk =
            acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(text)));
        if let Some(message_id) = message_id {
            chunk = chunk.message_id(message_id);
        }
        let update = match kind {
            "answer" => acp::SessionUpdate::AgentMessageChunk(chunk),
            "thought" => acp::SessionUpdate::AgentThoughtChunk(chunk),
            _ => acp::SessionUpdate::UserMessageChunk(chunk),
        };
        SessionEvent::Acp(AcpEvent::Notification(
            acp::AgentNotification::SessionNotification(acp::SessionNotification::new("s", update)),
        ))
    }

    fn describe(events: &[SessionEvent]) -> Vec<String> {
        events
            .iter()
            .map(|event| match plain_text_chunk(event) {
                Some((notification, is_thought, text)) => {
                    let message = match &notification.update {
                        acp::SessionUpdate::AgentMessageChunk(chunk)
                        | acp::SessionUpdate::AgentThoughtChunk(chunk) => chunk
                            .message_id
                            .as_ref()
                            .map(ToString::to_string)
                            .unwrap_or_default(),
                        _ => String::new(),
                    };
                    format!(
                        "{}:{message}:{text}",
                        if is_thought { "thought" } else { "answer" }
                    )
                }
                None => "other".to_string(),
            })
            .collect()
    }

    #[test]
    fn adjacent_chunks_of_one_message_merge() {
        let merged = conflate(vec![
            chunk("answer", Some("m"), "Hel"),
            chunk("answer", Some("m"), "lo "),
            chunk("answer", Some("m"), "world"),
        ]);
        assert_eq!(describe(&merged), vec!["answer:m:Hello world".to_string()]);
    }

    #[test]
    fn a_change_of_message_stream_or_session_breaks_the_run() {
        let merged = conflate(vec![
            chunk("answer", Some("a"), "one "),
            chunk("answer", Some("b"), "two "),
            chunk("thought", Some("b"), "three "),
            chunk("answer", Some("b"), "four"),
        ]);
        assert_eq!(
            describe(&merged),
            vec![
                "answer:a:one ".to_string(),
                "answer:b:two ".to_string(),
                "thought:b:three ".to_string(),
                "answer:b:four".to_string(),
            ]
        );
    }

    #[test]
    fn other_events_keep_their_position_and_split_runs() {
        let merged = conflate(vec![
            chunk("answer", None, "a"),
            chunk("user", None, "u"),
            chunk("answer", None, "b"),
            chunk("answer", None, "c"),
        ]);
        assert_eq!(
            describe(&merged),
            vec![
                "answer::a".to_string(),
                "other".to_string(),
                "answer::bc".to_string()
            ]
        );
    }

    #[test]
    fn chunks_with_metadata_are_never_merged() {
        let mut with_meta = chunk("answer", Some("m"), "x");
        if let SessionEvent::Acp(AcpEvent::Notification(
            acp::AgentNotification::SessionNotification(notification),
        )) = &mut with_meta
        {
            notification.meta = Some(acp::Meta::new());
        }
        let merged = conflate(vec![chunk("answer", Some("m"), "a"), with_meta]);
        assert_eq!(merged.len(), 2);
    }
}

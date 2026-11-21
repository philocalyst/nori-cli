use super::*;

/// Character threshold above which we log a warning about transcript summary
/// size. The summary is not truncated — the agent-side "prompt too long"
/// rejection is the real guard — but large summaries are worth logging.
const TRANSCRIPT_SUMMARY_WARN_CHARS: usize = 200_000;

/// Recover the agent-to-client ACP notification stream stored in a v3
/// transcript. For v1/v2 transcripts, synthesize message notifications from
/// the stable user/assistant entries so old sessions remain replayable.
pub fn transcript_to_replay_session_events(
    transcript: &crate::transcript::Transcript,
) -> Vec<nori_protocol::SessionEvent> {
    replay_session_events(transcript, false)
}

pub(crate) fn transcript_to_child_replay_session_events(
    transcript: &crate::transcript::Transcript,
) -> Vec<nori_protocol::SessionEvent> {
    replay_session_events(transcript, true)
}

fn replay_session_events(
    transcript: &crate::transcript::Transcript,
    preserve_child_responses: bool,
) -> Vec<nori_protocol::SessionEvent> {
    let has_raw_notifications = transcript.entries.iter().any(|line| {
        matches!(
            &line.entry,
            crate::transcript::TranscriptEntry::SessionEvent(entry)
                if !entry.historical_evidence && matches!(
                    entry.event,
                    nori_protocol::SessionEvent::Acp(
                        nori_protocol::AcpEvent::Notification(
                            nori_protocol::acp::v1::AgentNotification::SessionNotification(_)
                        )
                    )
                )
        )
    });
    let canonical_user_message_ids = transcript
        .entries
        .iter()
        .filter_map(|line| {
            let crate::transcript::TranscriptEntry::SessionEvent(entry) = &line.entry else {
                return None;
            };
            if entry.historical_evidence {
                return None;
            }
            let nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
                nori_protocol::acp::v1::AgentNotification::SessionNotification(notification),
            )) = &entry.event
            else {
                return None;
            };
            let nori_protocol::acp::v1::SessionUpdate::UserMessageChunk(chunk) =
                &notification.update
            else {
                return None;
            };
            chunk.message_id.as_ref().map(ToString::to_string)
        })
        .collect::<std::collections::HashSet<_>>();

    let session_id = transcript
        .meta
        .acp_session_id
        .clone()
        .unwrap_or_else(|| format!("transcript-{}", transcript.meta.session_id));
    let mut timing = super::replay_timing::TurnTiming::default();
    let mut integrity = super::replay_integrity::ReplayIntegrity::new(session_id.clone());
    let mut replay = Vec::new();
    let mut response_end = None;
    let first_id = transcript.event_ids.first().copied();
    for (index, line) in transcript.entries.iter().enumerate() {
        if let crate::transcript::TranscriptEntry::SessionEvent(entry) = &line.entry {
            // These are live control signals. The journal retains them, but replaying them
            // would retry a historical request or show an obsolete provider failure.
            if matches!(
                entry.event,
                nori_protocol::SessionEvent::Nori(
                    nori_protocol::NoriEvent::SubscriptionExhausted(_)
                        | nori_protocol::NoriEvent::RetryScheduled(_)
                        | nori_protocol::NoriEvent::RequestFailed(_)
                )
            ) {
                continue;
            }
            // One definition of where a message ends, shared with the live turn.
            let flow = nori_protocol::message_flow(&entry.event);
            let assistant_chunk =
                flow == nori_protocol::MessageFlow::Chunk(nori_protocol::MessageStream::Answer);
            let boundary = match flow {
                nori_protocol::MessageFlow::Boundary => true,
                nori_protocol::MessageFlow::Chunk(stream) => {
                    stream != nori_protocol::MessageStream::Answer
                }
                nori_protocol::MessageFlow::Transparent => false,
            };
            let boundary = boundary
                || (preserve_child_responses
                    && matches!(
                        &entry.event,
                        nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Response {
                            response: Err(_),
                            ..
                        })
                    ));
            if boundary && let (Some(start), Some(end)) = (first_id, response_end.take()) {
                replay.push(nori_protocol::SessionEvent::Nori(
                    nori_protocol::NoriEvent::ConversationResponseBoundary(
                        nori_protocol::ConversationResponseBoundary {
                            transcript_id: transcript.meta.session_id.clone(),
                            start,
                            end,
                        },
                    ),
                ));
            }
            if assistant_chunk && !entry.historical_evidence {
                response_end = transcript.event_ids.get(index).copied();
            }
            if entry.historical_evidence {
                continue;
            }
            integrity.observe(&entry.event);
            match &entry.event {
                // What the user saw and later events depend on: kept, as plain notices where
                // replaying the live event would change the session's present state.
                nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::RequestDeclined(_)) => {
                    replay.push(entry.event.clone());
                    continue;
                }
                nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::RouteChanged(
                    changed,
                )) => {
                    replay.push(replayed_notice(format!(
                        "Continued on {} instead of {}: {}.",
                        changed.to,
                        changed.from,
                        changed.cause.label()
                    )));
                    continue;
                }
                // The failed attempt's output stays in the history; this marks it, as it did live.
                nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::AttemptSuperseded(
                    superseded,
                )) => {
                    replay.push(replayed_notice(match superseded.by {
                        nori_protocol::SupersededBy::Retry => {
                            "Retried: the output above came from a failed attempt.".to_owned()
                        }
                        nori_protocol::SupersededBy::Route => {
                            "Continued on another route: the output above came from a failed attempt."
                                .to_owned()
                        }
                    }));
                    continue;
                }
                // The session that continued the conversation on another agent says nothing the
                // route change did not: the user saw one marker live and sees one here.
                nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::SessionStarted(
                    started,
                )) if started.swap.is_some() => continue,
                nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::SessionEnded(
                    ended,
                )) if ended.reason == nori_protocol::SessionEndReason::ConnectionLost => {
                    if let Some(message) = &ended.message {
                        replay.push(replayed_notice(message.clone()));
                    }
                    continue;
                }
                nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Response { .. })
                    if preserve_child_responses =>
                {
                    replay.push(entry.event.clone());
                    continue;
                }
                nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::Subagent(wire)) => {
                    let mut wire = wire.clone();
                    if let nori_protocol::SubagentWire::Update(update) = &mut wire
                        && matches!(
                            &update.state,
                            nori_protocol::Patch::Set(nori_protocol::SubagentState::Running)
                        )
                    {
                        update.state =
                            nori_protocol::Patch::Set(nori_protocol::SubagentState::Idle {
                                interrupted: true,
                            });
                    }
                    replay.push(nori_protocol::SessionEvent::Nori(
                        nori_protocol::NoriEvent::Subagent(wire),
                    ));
                    continue;
                }
                nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::ChildNotification(_)) => {
                    replay.push(entry.event.clone());
                    continue;
                }
                _ => {}
            }
            timing.observe(entry);
            if has_raw_notifications
                && let nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
                    nori_protocol::acp::v1::AgentNotification::SessionNotification(notification),
                )) = &entry.event
            {
                replay.push(timing.replayed(notification, entry.observed_at.as_deref()));
                continue;
            }
        }

        let updates: Vec<_> = match &line.entry {
            crate::transcript::TranscriptEntry::User(user) => {
                replay.extend(integrity.user_prompt(&user.content));
                timing.start_turn(&line.ts);
                if canonical_user_message_ids.contains(&user.id) {
                    Vec::new()
                } else {
                    vec![nori_protocol::acp::v1::SessionUpdate::UserMessageChunk(
                        nori_protocol::acp::v1::ContentChunk::new(
                            nori_protocol::acp::v1::ContentBlock::Text(
                                nori_protocol::acp::v1::TextContent::new(user.content.clone()),
                            ),
                        ),
                    )]
                }
            }
            crate::transcript::TranscriptEntry::Assistant(assistant) if !has_raw_notifications => {
                assistant
                    .content
                    .iter()
                    .map(|block| match block {
                        ContentBlock::Text { text } => {
                            nori_protocol::acp::v1::SessionUpdate::AgentMessageChunk(
                                nori_protocol::acp::v1::ContentChunk::new(
                                    nori_protocol::acp::v1::ContentBlock::Text(
                                        nori_protocol::acp::v1::TextContent::new(text.clone()),
                                    ),
                                ),
                            )
                        }
                        ContentBlock::Thinking { thinking } => {
                            nori_protocol::acp::v1::SessionUpdate::AgentThoughtChunk(
                                nori_protocol::acp::v1::ContentChunk::new(
                                    nori_protocol::acp::v1::ContentBlock::Text(
                                        nori_protocol::acp::v1::TextContent::new(thinking.clone()),
                                    ),
                                ),
                            )
                        }
                    })
                    .collect()
            }
            _ => Vec::new(),
        };
        replay.extend(updates.into_iter().map(|update| {
            nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
                nori_protocol::acp::v1::AgentNotification::SessionNotification(
                    nori_protocol::acp::v1::SessionNotification::new(session_id.clone(), update),
                ),
            ))
        }));
    }
    if let (Some(start), Some(end)) = (first_id, response_end) {
        replay.push(nori_protocol::SessionEvent::Nori(
            nori_protocol::NoriEvent::ConversationResponseBoundary(
                nori_protocol::ConversationResponseBoundary {
                    transcript_id: transcript.meta.session_id.clone(),
                    start,
                    end,
                },
            ),
        ));
    }
    // A journal that stops mid-turn: what it left running is interrupted, and the prompt it
    // never answered is offered back.
    replay.extend(integrity.finish(!preserve_child_responses));
    replay
}

fn replayed_notice(message: String) -> nori_protocol::SessionEvent {
    nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::Notice(nori_protocol::Notice {
        message,
    }))
}

/// Convert a loaded transcript into normalized replay events suitable for ACP
/// session resume. The replay stream is intentionally static: it reconstructs
/// user/assistant history and completed normalized artifacts without reviving
/// live approval or turn-lifecycle state.
pub fn transcript_to_replay_client_events(
    transcript: &crate::transcript::Transcript,
) -> Vec<crate::normalized::ClientEvent> {
    let mut replay = Vec::new();

    for line in &transcript.entries {
        match &line.entry {
            crate::transcript::TranscriptEntry::User(user) => {
                replay.push(crate::normalized::ClientEvent::ReplayEntry(
                    crate::normalized::ReplayEntry::UserMessage {
                        text: user.content.clone(),
                    },
                ));
            }
            crate::transcript::TranscriptEntry::Assistant(assistant) => {
                for block in &assistant.content {
                    match block {
                        ContentBlock::Text { text } if !text.is_empty() => {
                            replay.push(crate::normalized::ClientEvent::ReplayEntry(
                                crate::normalized::ReplayEntry::AssistantMessage {
                                    text: text.clone(),
                                },
                            ))
                        }
                        ContentBlock::Thinking { thinking } if !thinking.is_empty() => {
                            replay.push(crate::normalized::ClientEvent::ReplayEntry(
                                crate::normalized::ReplayEntry::ReasoningMessage {
                                    text: thinking.clone(),
                                },
                            ))
                        }
                        _ => {}
                    }
                }
            }
            crate::transcript::TranscriptEntry::ClientEvent(client_event) => {
                if let Some(replay_entry) = replay_entry_from_client_event(&client_event.event) {
                    replay.push(crate::normalized::ClientEvent::ReplayEntry(replay_entry));
                } else if should_pass_through_replay_client_event(&client_event.event) {
                    replay.push(client_event.event.clone());
                }
            }
            crate::transcript::TranscriptEntry::SessionEvent(session_event) => match &session_event
                .event
            {
                nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::GoalChanged(Some(
                    goal,
                ))) => {
                    replay.push(crate::normalized::ClientEvent::ThreadGoalUpdated(
                        crate::normalized::ThreadGoalUpdated { goal: goal.clone() },
                    ));
                }
                nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::GoalChanged(None)) => {
                    replay.push(crate::normalized::ClientEvent::ThreadGoalCleared);
                }
                _ => {}
            },
            _ => {}
        }
    }

    replay
}

pub fn client_events_to_replay_client_events(
    client_events: Vec<crate::normalized::ClientEvent>,
) -> Vec<crate::normalized::ClientEvent> {
    let mut replay = Vec::new();
    let mut current_stream: Option<crate::normalized::MessageStream> = None;
    let mut current_text = String::new();

    let flush_message = |replay: &mut Vec<crate::normalized::ClientEvent>,
                         stream: &mut Option<crate::normalized::MessageStream>,
                         text: &mut String| {
        let Some(stream) = stream.take() else {
            return;
        };
        if text.is_empty() {
            return;
        }

        let text = std::mem::take(text);
        let entry = match stream {
            crate::normalized::MessageStream::User => {
                crate::normalized::ReplayEntry::UserMessage { text }
            }
            crate::normalized::MessageStream::Answer => {
                crate::normalized::ReplayEntry::AssistantMessage { text }
            }
            crate::normalized::MessageStream::Reasoning => {
                crate::normalized::ReplayEntry::ReasoningMessage { text }
            }
        };
        replay.push(crate::normalized::ClientEvent::ReplayEntry(entry));
    };

    for event in client_events {
        match event {
            crate::normalized::ClientEvent::MessageDelta(message_delta) => {
                if current_stream.as_ref() != Some(&message_delta.stream) {
                    flush_message(&mut replay, &mut current_stream, &mut current_text);
                    current_stream = Some(message_delta.stream);
                }
                current_text.push_str(&message_delta.delta);
            }
            other => {
                flush_message(&mut replay, &mut current_stream, &mut current_text);
                if let Some(replay_entry) = replay_entry_from_client_event(&other) {
                    replay.push(crate::normalized::ClientEvent::ReplayEntry(replay_entry));
                } else if should_pass_through_replay_client_event(&other) {
                    replay.push(other);
                }
            }
        }
    }

    flush_message(&mut replay, &mut current_stream, &mut current_text);
    replay
}

/// Convert a loaded transcript into a human-readable summary string suitable
/// for injecting into the first prompt via `pending_compact_summary`.
///
/// The summary captures user messages, assistant responses, and the recorded
/// tool lifecycle in order, including tool input and output.
///
/// No truncation is applied — the full transcript is preserved so the agent
/// retains as much context as possible on resume. If the resulting prompt
/// exceeds the model's context window, the agent will reject it with a
/// "prompt too long" error, which is handled gracefully by the caller.
pub fn transcript_to_summary(transcript: &crate::transcript::Transcript) -> String {
    let mut seen_tool_calls = std::collections::HashSet::new();
    let raw_tool_ids: std::collections::HashSet<String> = transcript
        .entries
        .iter()
        .filter_map(|line| {
            let crate::transcript::TranscriptEntry::SessionEvent(entry) = &line.entry else {
                return None;
            };
            if entry.historical_evidence {
                return None;
            }
            let nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
                nori_protocol::acp::v1::AgentNotification::SessionNotification(notification),
            )) = &entry.event
            else {
                return None;
            };
            match &notification.update {
                nori_protocol::acp::v1::SessionUpdate::ToolCall(call) => {
                    Some(call.tool_call_id.to_string())
                }
                nori_protocol::acp::v1::SessionUpdate::ToolCallUpdate(update) => {
                    Some(update.tool_call_id.to_string())
                }
                _ => None,
            }
        })
        .collect();
    let mut summary = String::new();
    let mut pending_raw_assistant = String::new();

    for line in &transcript.entries {
        if let crate::transcript::TranscriptEntry::SessionEvent(session_event) = &line.entry
            && !session_event.historical_evidence
            && let nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
                nori_protocol::acp::v1::AgentNotification::SessionNotification(notification),
            )) = &session_event.event
            && let nori_protocol::acp::v1::SessionUpdate::AgentMessageChunk(chunk) =
                &notification.update
            && let nori_protocol::acp::v1::ContentBlock::Text(text) = &chunk.content
        {
            pending_raw_assistant.push_str(&text.text);
            continue;
        }

        if !pending_raw_assistant.is_empty() {
            summary.push_str(&format!("Assistant: {pending_raw_assistant}\n"));
            pending_raw_assistant.clear();
        }

        match &line.entry {
            crate::transcript::TranscriptEntry::SessionEvent(entry)
                if !entry.historical_evidence =>
            {
                if let nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
                    nori_protocol::acp::v1::AgentNotification::SessionNotification(notification),
                )) = &entry.event
                    && matches!(
                        &notification.update,
                        nori_protocol::acp::v1::SessionUpdate::ToolCall(_)
                            | nori_protocol::acp::v1::SessionUpdate::ToolCallUpdate(_)
                    )
                {
                    match serde_json::to_string(&notification.update) {
                        Ok(event) => summary.push_str(&format!("Tool event: {event}\n")),
                        Err(error) => {
                            tracing::warn!(%error, "could not serialize tool event in transcript context")
                        }
                    }
                }
            }
            crate::transcript::TranscriptEntry::User(user) => {
                summary.push_str(&format!("User: {}\n", user.content));
            }
            crate::transcript::TranscriptEntry::Assistant(assistant) => {
                let text: String = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        ContentBlock::Thinking { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                if !text.is_empty() {
                    summary.push_str(&format!("Assistant: {text}\n"));
                }
            }
            crate::transcript::TranscriptEntry::ToolCall(tool)
                if !raw_tool_ids.contains(&tool.call_id) =>
            {
                summary.push_str(&format!("[Tool: {}] Input: {}\n", tool.name, tool.input));
            }
            crate::transcript::TranscriptEntry::ToolResult(tool)
                if !raw_tool_ids.contains(&tool.call_id) =>
            {
                summary.push_str(&format!(
                    "[Tool result: {}] {}\n",
                    tool.call_id, tool.output
                ));
            }
            crate::transcript::TranscriptEntry::ClientEvent(client_event) => {
                match &client_event.event {
                    crate::normalized::ClientEvent::ToolSnapshot(tool_snapshot)
                        if !raw_tool_ids.contains(&tool_snapshot.call_id)
                            && seen_tool_calls.insert(tool_snapshot.call_id.clone()) =>
                    {
                        summary.push_str(&format!("[Tool: {}]\n", tool_snapshot.title));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    if !pending_raw_assistant.is_empty() {
        summary.push_str(&format!("Assistant: {pending_raw_assistant}\n"));
    }

    if summary.len() > TRANSCRIPT_SUMMARY_WARN_CHARS {
        warn!(
            "Transcript summary is very large ({} chars). \
             If the agent rejects it as too long, try /compact or start a new session.",
            summary.len()
        );
    }

    summary
}

fn replay_entry_from_client_event(
    event: &crate::normalized::ClientEvent,
) -> Option<crate::normalized::ReplayEntry> {
    match event {
        crate::normalized::ClientEvent::ToolSnapshot(snapshot)
            if matches!(
                snapshot.phase,
                crate::normalized::ToolPhase::Completed | crate::normalized::ToolPhase::Failed
            ) =>
        {
            Some(crate::normalized::ReplayEntry::ToolSnapshot {
                snapshot: Box::new(snapshot.clone()),
            })
        }
        crate::normalized::ClientEvent::ToolSnapshot(_) => None,
        crate::normalized::ClientEvent::PlanSnapshot(snapshot) => {
            Some(crate::normalized::ReplayEntry::PlanSnapshot {
                snapshot: snapshot.clone(),
            })
        }
        crate::normalized::ClientEvent::ApprovalRequest(_)
        | crate::normalized::ClientEvent::MessageDelta(_)
        | crate::normalized::ClientEvent::SessionPhaseChanged(_)
        | crate::normalized::ClientEvent::ToolsInterrupted(_)
        | crate::normalized::ClientEvent::PromptCompleted(_)
        | crate::normalized::ClientEvent::LoadCompleted
        | crate::normalized::ClientEvent::QueueChanged(_)
        | crate::normalized::ClientEvent::ContextCompacted(_)
        | crate::normalized::ClientEvent::ReplayEntry(_)
        | crate::normalized::ClientEvent::AgentCommandsUpdate(_)
        | crate::normalized::ClientEvent::SessionCapabilitiesChanged(_)
        | crate::normalized::ClientEvent::SessionUpdateInfo(_)
        | crate::normalized::ClientEvent::SessionConfigUpdate(_)
        | crate::normalized::ClientEvent::SessionModeChanged(_)
        | crate::normalized::ClientEvent::ThreadGoalUpdated(_)
        | crate::normalized::ClientEvent::ThreadGoalCleared
        | crate::normalized::ClientEvent::Warning(_) => None,
    }
}

fn should_pass_through_replay_client_event(event: &crate::normalized::ClientEvent) -> bool {
    matches!(
        event,
        crate::normalized::ClientEvent::SessionUpdateInfo(_)
            | crate::normalized::ClientEvent::ThreadGoalUpdated(_)
            | crate::normalized::ClientEvent::ThreadGoalCleared
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use pretty_assertions::assert_eq;

    use super::*;
    use crate::transcript::AssistantEntry;
    use crate::transcript::ClientEventEntry;
    use crate::transcript::ContentBlock;
    use crate::transcript::SessionEventEntry;
    use crate::transcript::SessionMetaEntry;
    use crate::transcript::Transcript;
    use crate::transcript::TranscriptEntry;
    use crate::transcript::TranscriptLine;
    use crate::transcript::UserEntry;

    fn make_transcript(entries: Vec<TranscriptEntry>) -> Transcript {
        let meta = SessionMetaEntry {
            session_id: "session-1".into(),
            project_id: "project-1".into(),
            started_at: "2025-01-01T00:00:00.000Z".into(),
            cwd: PathBuf::from("/repo"),
            agent: Some("claude-code".into()),
            cli_version: "0.1.0".into(),
            git: None,
            acp_session_id: None,
            forked_from: None,
            delegated: false,
        };

        let mut lines = vec![TranscriptLine::new(TranscriptEntry::SessionMeta(
            meta.clone(),
        ))];
        lines.extend(entries.into_iter().map(TranscriptLine::new));

        Transcript {
            meta,
            entries: lines,
            backend_binding: None,
            requires_fresh_binding: false,
            event_ids: Vec::new(),
            left_out: 0,
        }
    }

    #[test]
    fn replay_does_not_redeliver_historical_fallback_controls() {
        let request_id = nori_protocol::acp::v1::RequestId::from(42);
        let exhausted = nori_protocol::SessionEvent::Nori(
            nori_protocol::NoriEvent::SubscriptionExhausted(nori_protocol::SubscriptionExhausted {
                request_id: request_id.clone(),
                echo_id: None,
                models: vec!["provider/model".into()],
                ranked: false,
                content: Vec::new(),
                context: None,
                original_error: "subscription exhausted".into(),
            }),
        );
        let failed = nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::RequestFailed(
            nori_protocol::RequestFailure {
                request_id: Some(request_id),
                message: "subscription exhausted".into(),
                kind: nori_protocol::RequestFailureKind::Retryable,
            },
        ));
        let transcript = make_transcript(vec![
            TranscriptEntry::SessionEvent(SessionEventEntry::new(exhausted)),
            TranscriptEntry::SessionEvent(SessionEventEntry::new(failed)),
        ]);
        assert!(transcript_to_replay_session_events(&transcript).is_empty());
        assert_eq!(transcript.entries.len(), 3);
    }

    #[test]
    fn canonical_replay_restores_response_source_range() {
        let update = nori_protocol::acp::v1::SessionUpdate::AgentMessageChunk(
            nori_protocol::acp::v1::ContentChunk::new(nori_protocol::acp::v1::ContentBlock::Text(
                nori_protocol::acp::v1::TextContent::new("answer"),
            )),
        );
        let event = nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
            nori_protocol::acp::v1::AgentNotification::SessionNotification(
                nori_protocol::acp::v1::SessionNotification::new("session", update),
            ),
        ));
        let mut transcript = make_transcript(vec![TranscriptEntry::SessionEvent(
            SessionEventEntry::new(event),
        )]);
        transcript.event_ids = vec![1, 7];
        let replay = transcript_to_replay_session_events(&transcript);
        assert!(
            matches!(replay.last(), Some(nori_protocol::SessionEvent::Nori(
            nori_protocol::NoriEvent::ConversationResponseBoundary(boundary)
        )) if boundary.transcript_id == "session-1" && boundary.start == 1 && boundary.end == 7)
        );
    }

    #[test]
    fn v3_replay_preserves_user_turns_between_raw_acp_notifications() {
        let raw_assistant_event =
            nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
                nori_protocol::acp::v1::AgentNotification::SessionNotification(
                    nori_protocol::acp::v1::SessionNotification::new(
                        "recorded-session",
                        nori_protocol::acp::v1::SessionUpdate::AgentMessageChunk(
                            nori_protocol::acp::v1::ContentChunk::new(
                                nori_protocol::acp::v1::ContentBlock::Text(
                                    nori_protocol::acp::v1::TextContent::new("Stored answer"),
                                ),
                            ),
                        ),
                    ),
                ),
            ));
        let transcript = make_transcript(vec![
            TranscriptEntry::User(UserEntry {
                id: "user-1".into(),
                content: "Stored question".into(),
                attachments: vec![],
            }),
            TranscriptEntry::SessionEvent(SessionEventEntry::new(raw_assistant_event.clone())),
        ]);

        let replay = transcript_to_replay_session_events(&transcript);

        assert_eq!(replay.len(), 2);
        assert!(matches!(
            &replay[0],
            nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
                nori_protocol::acp::v1::AgentNotification::SessionNotification(notification)
            )) if matches!(
                &notification.update,
                nori_protocol::acp::v1::SessionUpdate::UserMessageChunk(chunk)
                    if matches!(
                        &chunk.content,
                        nori_protocol::acp::v1::ContentBlock::Text(text)
                            if text.text == "Stored question"
                    )
            )
        ));
        // The answer is replayed as it was recorded, saying how long its turn had run by then.
        let mut replayed = serde_json::to_value(&replay[1]).unwrap();
        let meta = replayed
            .pointer_mut("/event/update")
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|update| update.remove("_meta"));
        assert!(
            meta.is_some_and(|meta| meta.get(nori_protocol::TURN_ELAPSED_META_KEY).is_some()),
            "{replayed}"
        );
        assert_eq!(replayed, serde_json::to_value(raw_assistant_event).unwrap());
    }

    fn at(seconds: i64) -> String {
        (chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00.000Z").unwrap()
            + chrono::TimeDelta::seconds(seconds))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    fn observed(event: nori_protocol::SessionEvent, seconds: i64) -> TranscriptLine {
        let mut entry = SessionEventEntry::new(event);
        entry.observed_at = Some(at(seconds));
        TranscriptLine {
            ts: at(seconds),
            v: 3,
            entry: TranscriptEntry::SessionEvent(entry),
        }
    }

    fn notification(update: serde_json::Value) -> nori_protocol::SessionEvent {
        nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
            nori_protocol::acp::v1::AgentNotification::SessionNotification(
                serde_json::from_value(serde_json::json!({ "sessionId": "s", "update": update }))
                    .unwrap(),
            ),
        ))
    }

    fn text(text: &str) -> nori_protocol::SessionEvent {
        notification(serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": text },
        }))
    }

    #[test]
    fn each_run_of_replayed_text_says_how_long_its_turn_had_run_without_the_waits() {
        let mut transcript = make_transcript(Vec::new());
        transcript.entries.push(TranscriptLine {
            ts: at(0),
            v: 3,
            entry: TranscriptEntry::User(UserEntry {
                id: "user-1".into(),
                content: "Do it".into(),
                attachments: vec![],
            }),
        });
        let permission: nori_protocol::acp::v1::RequestPermissionRequest =
            serde_json::from_value(serde_json::json!({
                "sessionId": "s",
                "toolCall": { "toolCallId": "t1" },
                "options": [],
            }))
            .unwrap();
        transcript.entries.extend([
            observed(text("Looking. "), 1),
            observed(text("Still looking. "), 2),
            observed(
                notification(serde_json::json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": "t1",
                    "title": "Edit a file",
                })),
                3,
            ),
            observed(
                nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Request {
                    request_id: nori_protocol::acp::v1::RequestId::Str("ask".into()),
                    request: nori_protocol::acp::v1::AgentRequest::RequestPermissionRequest(
                        permission,
                    ),
                }),
                4,
            ),
            // The user answered ten seconds later.
            observed(
                notification(serde_json::json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "t1",
                    "status": "completed",
                })),
                14,
            ),
            observed(text("Done."), 16),
        ]);

        let elapsed: Vec<Option<std::time::Duration>> =
            transcript_to_replay_session_events(&transcript)
                .iter()
                .filter_map(|event| match event {
                    nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
                        nori_protocol::acp::v1::AgentNotification::SessionNotification(
                            notification,
                        ),
                    )) => match &notification.update {
                        nori_protocol::acp::v1::SessionUpdate::AgentMessageChunk(chunk) => {
                            Some(nori_protocol::turn_elapsed_in_meta(chunk.meta.as_ref()))
                        }
                        _ => None,
                    },
                    _ => None,
                })
                .collect();

        assert_eq!(
            elapsed,
            vec![
                Some(std::time::Duration::from_secs(1)),
                None,
                Some(std::time::Duration::from_secs(6)),
            ]
        );
    }

    #[test]
    fn v3_replay_uses_canonical_raw_user_chunks_without_synthesizing_a_duplicate() {
        let message_id = nori_protocol::acp::v1::MessageId::new("user-1");
        let raw_user_events = vec![
            nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
                nori_protocol::acp::v1::AgentNotification::SessionNotification(
                    nori_protocol::acp::v1::SessionNotification::new(
                        "recorded-session",
                        nori_protocol::acp::v1::SessionUpdate::UserMessageChunk(
                            nori_protocol::acp::v1::ContentChunk::new(
                                nori_protocol::acp::v1::ContentBlock::Text(
                                    nori_protocol::acp::v1::TextContent::new("Stored question"),
                                ),
                            )
                            .message_id(message_id.clone()),
                        ),
                    ),
                ),
            )),
            nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
                nori_protocol::acp::v1::AgentNotification::SessionNotification(
                    nori_protocol::acp::v1::SessionNotification::new(
                        "recorded-session",
                        nori_protocol::acp::v1::SessionUpdate::UserMessageChunk(
                            nori_protocol::acp::v1::ContentChunk::new(
                                nori_protocol::acp::v1::ContentBlock::Image(
                                    nori_protocol::acp::v1::ImageContent::new(
                                        "aW1hZ2U=",
                                        "image/png",
                                    ),
                                ),
                            )
                            .message_id(message_id),
                        ),
                    ),
                ),
            )),
        ];
        let transcript = make_transcript(vec![
            TranscriptEntry::User(UserEntry {
                id: "user-1".into(),
                content: "Stored question".into(),
                attachments: vec![],
            }),
            TranscriptEntry::SessionEvent(SessionEventEntry::new(raw_user_events[0].clone())),
            TranscriptEntry::SessionEvent(SessionEventEntry::new(raw_user_events[1].clone())),
        ]);

        // The journal ends on this prompt with no answer, so (docs/turn-lifecycle.md, Resume) it
        // is offered back after the one canonical user turn; no second user turn is synthesized.
        let expected = raw_user_events
            .into_iter()
            .chain([nori_protocol::SessionEvent::Nori(
                nori_protocol::NoriEvent::PromptUnanswered(nori_protocol::PromptUnanswered {
                    text: "Stored question".into(),
                }),
            )])
            .map(|event| serde_json::to_value(event).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            transcript_to_replay_session_events(&transcript)
                .into_iter()
                .map(|event| serde_json::to_value(event).unwrap())
                .collect::<Vec<_>>(),
            expected
        );
    }

    #[test]
    fn v3_summary_includes_raw_acp_assistant_text() {
        let transcript = make_transcript(vec![
            TranscriptEntry::User(UserEntry {
                id: "user-1".into(),
                content: "Stored question".into(),
                attachments: vec![],
            }),
            TranscriptEntry::SessionEvent(SessionEventEntry::new(
                nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
                    nori_protocol::acp::v1::AgentNotification::SessionNotification(
                        nori_protocol::acp::v1::SessionNotification::new(
                            "recorded-session",
                            nori_protocol::acp::v1::SessionUpdate::AgentMessageChunk(
                                nori_protocol::acp::v1::ContentChunk::new(
                                    nori_protocol::acp::v1::ContentBlock::Text(
                                        nori_protocol::acp::v1::TextContent::new("Stored answer"),
                                    ),
                                ),
                            ),
                        ),
                    ),
                )),
            )),
        ]);

        assert_eq!(
            transcript_to_summary(&transcript),
            "User: Stored question\nAssistant: Stored answer\n"
        );
    }

    #[test]
    fn transcript_replay_client_events_preserve_user_assistant_and_tool_snapshot() {
        let transcript = make_transcript(vec![
            TranscriptEntry::User(UserEntry {
                id: "user-1".into(),
                content: "Inspect the repo".into(),
                attachments: vec![],
            }),
            TranscriptEntry::Assistant(AssistantEntry {
                id: "assistant-1".into(),
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "Need to inspect files".into(),
                    },
                    ContentBlock::Text {
                        text: "I found the ACP bridge.".into(),
                    },
                ],
                agent: Some("claude-code".into()),
            }),
            TranscriptEntry::ClientEvent(ClientEventEntry {
                event: crate::normalized::ClientEvent::ToolSnapshot(
                    crate::normalized::ToolSnapshot {
                        call_id: "tool-1".into(),
                        title: "Read Cargo.toml".into(),
                        kind: crate::normalized::ToolKind::Read,
                        phase: crate::normalized::ToolPhase::Completed,
                        locations: vec![],
                        invocation: Some(crate::normalized::Invocation::Read {
                            path: PathBuf::from("Cargo.toml"),
                        }),
                        artifacts: vec![],
                        raw_input: None,
                        raw_output: None,
                        owner_request_id: None,
                    },
                ),
            }),
            TranscriptEntry::ClientEvent(ClientEventEntry {
                event: crate::normalized::ClientEvent::MessageDelta(
                    crate::normalized::MessageDelta {
                        stream: crate::normalized::MessageStream::Answer,
                        delta: "duplicate streamed text".into(),
                    },
                ),
            }),
        ]);

        let replay = transcript_to_replay_client_events(&transcript);

        assert_eq!(
            replay,
            vec![
                crate::normalized::ClientEvent::ReplayEntry(
                    crate::normalized::ReplayEntry::UserMessage {
                        text: "Inspect the repo".into(),
                    }
                ),
                crate::normalized::ClientEvent::ReplayEntry(
                    crate::normalized::ReplayEntry::ReasoningMessage {
                        text: "Need to inspect files".into(),
                    },
                ),
                crate::normalized::ClientEvent::ReplayEntry(
                    crate::normalized::ReplayEntry::AssistantMessage {
                        text: "I found the ACP bridge.".into(),
                    },
                ),
                crate::normalized::ClientEvent::ReplayEntry(
                    crate::normalized::ReplayEntry::ToolSnapshot {
                        snapshot: Box::new(crate::normalized::ToolSnapshot {
                            call_id: "tool-1".into(),
                            title: "Read Cargo.toml".into(),
                            kind: crate::normalized::ToolKind::Read,
                            phase: crate::normalized::ToolPhase::Completed,
                            locations: vec![],
                            invocation: Some(crate::normalized::Invocation::Read {
                                path: PathBuf::from("Cargo.toml"),
                            }),
                            artifacts: vec![],
                            raw_input: None,
                            raw_output: None,
                            owner_request_id: None,
                        }),
                    }
                ),
            ]
        );
    }

    #[test]
    fn client_events_to_replay_client_events_buffers_user_deltas_and_preserves_info_updates() {
        let replay = client_events_to_replay_client_events(vec![
            crate::normalized::ClientEvent::MessageDelta(crate::normalized::MessageDelta {
                stream: crate::normalized::MessageStream::User,
                delta: "Resume".into(),
            }),
            crate::normalized::ClientEvent::MessageDelta(crate::normalized::MessageDelta {
                stream: crate::normalized::MessageStream::User,
                delta: " this session".into(),
            }),
            crate::normalized::ClientEvent::SessionUpdateInfo(
                crate::normalized::SessionUpdateInfo {
                    kind: crate::normalized::SessionUpdateKind::SessionInfo,
                    message: "Session info updated: title=\"Resume chat\"".into(),
                    hint: None,
                    usage: None,
                },
            ),
            crate::normalized::ClientEvent::MessageDelta(crate::normalized::MessageDelta {
                stream: crate::normalized::MessageStream::Answer,
                delta: "Loaded.".into(),
            }),
        ]);

        assert_eq!(
            replay,
            vec![
                crate::normalized::ClientEvent::ReplayEntry(
                    crate::normalized::ReplayEntry::UserMessage {
                        text: "Resume this session".into(),
                    }
                ),
                crate::normalized::ClientEvent::SessionUpdateInfo(
                    crate::normalized::SessionUpdateInfo {
                        kind: crate::normalized::SessionUpdateKind::SessionInfo,
                        message: "Session info updated: title=\"Resume chat\"".into(),
                        hint: None,
                        usage: None,
                    }
                ),
                crate::normalized::ClientEvent::ReplayEntry(
                    crate::normalized::ReplayEntry::AssistantMessage {
                        text: "Loaded.".into(),
                    },
                ),
            ]
        );
    }

    #[test]
    fn client_events_to_replay_client_events_preserves_goal_updates() {
        let goal_event = crate::normalized::ClientEvent::ThreadGoalUpdated(
            crate::normalized::ThreadGoalUpdated {
                goal: nori_protocol::ThreadGoal {
                    objective: "Keep the north star".to_string(),
                    status: nori_protocol::ThreadGoalStatus::Active,
                    tokens_used: 42,
                    time_used_seconds: 7,
                    created_at: 100,
                    updated_at: 107,
                },
            },
        );
        let replay = client_events_to_replay_client_events(vec![goal_event.clone()]);

        assert_eq!(replay, vec![goal_event]);
    }

    #[test]
    fn client_events_to_replay_client_events_preserves_goal_clears() {
        let replay = client_events_to_replay_client_events(vec![
            crate::normalized::ClientEvent::ThreadGoalCleared,
        ]);

        assert_eq!(
            replay,
            vec![crate::normalized::ClientEvent::ThreadGoalCleared]
        );
    }

    #[test]
    fn client_events_to_replay_client_events_preserves_mixed_message_delta_order() {
        let replay = client_events_to_replay_client_events(vec![
            crate::normalized::ClientEvent::MessageDelta(crate::normalized::MessageDelta {
                stream: crate::normalized::MessageStream::Answer,
                delta: "CI is green.".into(),
            }),
            crate::normalized::ClientEvent::MessageDelta(crate::normalized::MessageDelta {
                stream: crate::normalized::MessageStream::Reasoning,
                delta: "Preparing PR.".into(),
            }),
            crate::normalized::ClientEvent::MessageDelta(crate::normalized::MessageDelta {
                stream: crate::normalized::MessageStream::Answer,
                delta: "The PR is up.".into(),
            }),
        ]);

        assert_eq!(
            replay,
            vec![
                crate::normalized::ClientEvent::ReplayEntry(
                    crate::normalized::ReplayEntry::AssistantMessage {
                        text: "CI is green.".into(),
                    },
                ),
                crate::normalized::ClientEvent::ReplayEntry(
                    crate::normalized::ReplayEntry::ReasoningMessage {
                        text: "Preparing PR.".into(),
                    },
                ),
                crate::normalized::ClientEvent::ReplayEntry(
                    crate::normalized::ReplayEntry::AssistantMessage {
                        text: "The PR is up.".into(),
                    },
                ),
            ]
        );
    }

    #[test]
    fn transcript_to_replay_client_events_preserves_session_update_info() {
        let transcript = make_transcript(vec![TranscriptEntry::ClientEvent(ClientEventEntry {
            event: crate::normalized::ClientEvent::SessionUpdateInfo(
                crate::normalized::SessionUpdateInfo {
                    kind: crate::normalized::SessionUpdateKind::Usage,
                    message: "Session usage: 128 / 4096 tokens".into(),
                    hint: None,
                    usage: Some(crate::normalized::session_runtime::SessionUsageState {
                        used_tokens: 128,
                        total_tokens: 4096,
                        cost_display: None,
                    }),
                },
            ),
        })]);

        let replay = transcript_to_replay_client_events(&transcript);

        assert_eq!(
            replay,
            vec![crate::normalized::ClientEvent::SessionUpdateInfo(
                crate::normalized::SessionUpdateInfo {
                    kind: crate::normalized::SessionUpdateKind::Usage,
                    message: "Session usage: 128 / 4096 tokens".into(),
                    hint: None,
                    usage: Some(crate::normalized::session_runtime::SessionUsageState {
                        used_tokens: 128,
                        total_tokens: 4096,
                        cost_display: None,
                    }),
                },
            )]
        );
    }
}

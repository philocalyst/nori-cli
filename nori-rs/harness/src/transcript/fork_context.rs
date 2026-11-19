//! Serialize an exact visible event-log slice as historical context for a child.

use super::types::TranscriptEntry;
use super::types::TranscriptLine;
use nori_conversation::Event;
use nori_conversation::Record;
use serde_json::Value;
use serde_json::json;

/// Materialize only conversation entries; provider bindings and control commands remain inert
/// in the archived source context. Appending these entries assigns fresh child-local record IDs.
pub(crate) fn replay_entries_from_records(records: &[Record], source: &str) -> Vec<TranscriptLine> {
    replay_entries(records, source, false)
}

pub(crate) fn replay_entries_with_children_from_records(
    records: &[Record],
    source: &str,
) -> Vec<TranscriptLine> {
    replay_entries(records, source, true)
}

fn replay_entries(records: &[Record], source: &str, include_children: bool) -> Vec<TranscriptLine> {
    let mut tool_ids = std::collections::HashMap::new();
    let mut tool_number = 0_i64;
    records
        .iter()
        .filter_map(|record| {
            let Event::Operation { name, args } = &record.event else {
                return None;
            };
            if name != "transcript-entry" {
                return None;
            }
            let mut line: TranscriptLine = serde_json::from_str(args).ok()?;
            let keep = match &line.entry {
                TranscriptEntry::User(_) | TranscriptEntry::Assistant(_) => true,
                TranscriptEntry::SessionEvent(entry) if !entry.historical_evidence => {
                    (include_children
                        && matches!(
                            &entry.event,
                            nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::Subagent(
                                _
                            )) | nori_protocol::SessionEvent::Acp(
                                nori_protocol::AcpEvent::ChildNotification(_)
                            )
                        ))
                        || matches!(&entry.event,
                nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
                    nori_protocol::acp::v1::AgentNotification::SessionNotification(notification)
                )) if matches!(notification.update,
                    nori_protocol::acp::v1::SessionUpdate::UserMessageChunk(_)
                    | nori_protocol::acp::v1::SessionUpdate::AgentMessageChunk(_)
                    | nori_protocol::acp::v1::SessionUpdate::AgentThoughtChunk(_)
                    | nori_protocol::acp::v1::SessionUpdate::ToolCall(_)
                    | nori_protocol::acp::v1::SessionUpdate::ToolCallUpdate(_)))
                        || matches!(
                            &entry.event,
                            nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Response {
                                response: Ok(
                                    nori_protocol::acp::v1::AgentResponse::PromptResponse(_)
                                ),
                                ..
                            }) | nori_protocol::SessionEvent::Acp(
                                nori_protocol::AcpEvent::Response {
                                    response: Err(_),
                                    ..
                                }
                            )
                        )
                }
                _ => false,
            };
            if keep
                && let TranscriptEntry::SessionEvent(entry) = &mut line.entry
                && let nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
                    nori_protocol::acp::v1::AgentNotification::SessionNotification(notification),
                )) = &mut entry.event
            {
                match &mut notification.update {
                    nori_protocol::acp::v1::SessionUpdate::ToolCall(call) => {
                        tool_number += 1;
                        let original = call.tool_call_id.to_string();
                        let replacement = format!("fork:{source}:{tool_number}:{original}");
                        tool_ids.insert(original, replacement.clone());
                        call.tool_call_id = replacement.into();
                    }
                    nori_protocol::acp::v1::SessionUpdate::ToolCallUpdate(update) => {
                        let original = update.tool_call_id.to_string();
                        let replacement = tool_ids.entry(original.clone()).or_insert_with(|| {
                            tool_number += 1;
                            format!("fork:{source}:{tool_number}:{original}")
                        });
                        update.tool_call_id = replacement.clone().into();
                    }
                    _ => {}
                }
            }
            keep.then_some(line)
        })
        .collect()
}

/// Copy only child bindings that existed in the selected visible history.
pub(crate) fn child_bindings_from_records(records: &[Record]) -> Vec<(String, String)> {
    let mut bindings = std::collections::BTreeMap::new();
    for record in records {
        if let Event::Command { name, payload } = &record.event
            && (name.starts_with("native-child-journal:")
                || name.starts_with("managed-child-journal:"))
        {
            if payload == "null" {
                bindings.remove(name);
            } else {
                bindings.insert(name.clone(), payload.clone());
            }
        }
    }
    bindings.into_iter().collect()
}

/// Keep each projected record and its full payload in order, including tool activity.
pub(crate) fn context_from_records(records: Vec<Record>) -> serde_json::Result<String> {
    let attachments: Vec<nori_protocol::acp::v1::ContentBlock> =
        replay_entries_from_records(&records, "context")
            .into_iter()
            .filter_map(|line| match line.entry {
                TranscriptEntry::SessionEvent(entry) => match entry.event {
                    nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
                        nori_protocol::acp::v1::AgentNotification::SessionNotification(
                            notification,
                        ),
                    )) => match notification.update {
                        nori_protocol::acp::v1::SessionUpdate::UserMessageChunk(chunk)
                            if !matches!(
                                chunk.content,
                                nori_protocol::acp::v1::ContentBlock::Text(_)
                            ) =>
                        {
                            Some(chunk.content)
                        }
                        _ => None,
                    },
                    _ => None,
                },
                _ => None,
            })
            .collect();
    let events: Vec<Value> = records
        .into_iter()
        .filter(|record| !matches!(&record.event, Event::Command { name, .. } if name == "fork-context" || name == "fork-source" || name.starts_with("native-child-journal:") || name.starts_with("managed-child-journal:") || name == "actor-delta" || name == "actor-snapshot"))
        .filter(|record| !matches!(&record.event, Event::Operation { name, args } if name == "transcript-entry" && serde_json::from_str::<TranscriptLine>(args).is_ok_and(|line| matches!(line.entry, TranscriptEntry::SessionEvent(entry) if matches!(entry.event, nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::ChildNotification(_)))))))
        .map(|record| {
            let event = match record.event {
                Event::Message { role, content } => {
                    json!({"type": "message", "role": role, "content": content})
                }
                Event::ToolCall { id, name, input } => {
                    json!({"type": "tool_call", "id": id, "name": name, "input": input})
                }
                Event::ToolResult { id, output } => {
                    json!({"type": "tool_result", "id": id, "output": output})
                }
                Event::Rewind { head } => json!({"type": "rewind", "head": head}),
                Event::Fork { source } => {
                    json!({"type": "fork", "start": source.start, "end": source.end})
                }
                Event::Operation { name, args } => {
                    json!({"type": "operation", "name": name, "args": args})
                }
                Event::Command { name, payload } => {
                    json!({"type": "command", "name": name, "payload": payload})
                }
                Event::SessionEvent { payload } => {
                    json!({"type": "session_event", "payload": payload})
                }
                Event::Unknown { form } => json!({"type": "unknown", "form": form}),
            };
            json!({"id": record.id, "command_id": record.command_id, "event": event})
        })
        .collect();
    let mut context = format!(
        "The following is the selected conversation event history, in order. Some records describe the same event in different forms.\n{}",
        Value::Array(events)
    );
    if !attachments.is_empty() {
        context.push_str("\n<nori-typed-history>");
        context.push_str(&serde_json::to_string(&attachments)?);
        context.push_str("</nori-typed-history>");
    }
    Ok(context)
}

/// Recover selected non-text user content from a fork checkpoint for the provider wire.
pub(crate) fn take_typed_history(
    checkpoint: &mut String,
) -> Vec<nori_protocol::acp::v1::ContentBlock> {
    const START: &str = "\n<nori-typed-history>";
    const END: &str = "</nori-typed-history>\n</summary>\n</conversation-checkpoint>";
    let Some((prefix, tail)) = checkpoint.rsplit_once(START) else {
        return Vec::new();
    };
    let Some(encoded) = tail.strip_suffix(END) else {
        return Vec::new();
    };
    let Ok(content) = serde_json::from_str(encoded) else {
        return Vec::new();
    };
    *checkpoint = format!("{prefix}\n</summary>\n</conversation-checkpoint>");
    content
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn selected_typed_content_and_failed_receipts_survive_projection() {
        use nori_protocol::AcpEvent;
        use nori_protocol::SessionEvent;
        use nori_protocol::acp::v1 as acp;
        let image = acp::ContentBlock::Image(acp::ImageContent::new("aW1hZ2U=", "image/png"));
        let notification = acp::SessionNotification::new(
            acp::SessionId::new("child"),
            acp::SessionUpdate::UserMessageChunk(acp::ContentChunk::new(image.clone())),
        );
        let events = [
            SessionEvent::Acp(AcpEvent::Notification(
                acp::AgentNotification::SessionNotification(notification),
            )),
            SessionEvent::Acp(AcpEvent::Response {
                request_id: acp::RequestId::Str("failed".into()),
                response: Err(acp::Error::internal_error()),
            }),
        ];
        let records: Vec<_> = events
            .into_iter()
            .enumerate()
            .map(|(index, event)| Record {
                id: index as i64 + 1,
                command_id: None,
                event: Event::Operation {
                    name: "transcript-entry".into(),
                    args: serde_json::to_string(&TranscriptLine::new(
                        TranscriptEntry::SessionEvent(super::super::types::SessionEventEntry::new(
                            event,
                        )),
                    ))
                    .unwrap(),
                },
            })
            .collect();
        assert_eq!(replay_entries_from_records(&records, "source").len(), 2);
        let mut checkpoint =
            crate::compact::checkpoint(&context_from_records(records).unwrap(), None);
        assert_eq!(take_typed_history(&mut checkpoint), vec![image]);
        assert!(!checkpoint.contains("<nori-typed-history>"));
    }

    #[test]
    fn nested_images_and_previous_checkpoints_are_not_injected_into_parent_input() {
        use nori_protocol::AcpEvent;
        use nori_protocol::SessionEvent;
        use nori_protocol::acp::v1 as acp;
        let image = acp::ContentBlock::Image(acp::ImageContent::new("Y2hpbGQ=", "image/png"));
        let notification = acp::SessionNotification::new(
            acp::SessionId::new("nested"),
            acp::SessionUpdate::UserMessageChunk(acp::ContentChunk::new(image)),
        );
        let line = TranscriptLine::new(TranscriptEntry::SessionEvent(
            super::super::types::SessionEventEntry::new(SessionEvent::Acp(
                AcpEvent::ChildNotification(notification),
            )),
        ));
        let records = vec![
            Record {
                id: 1,
                command_id: None,
                event: Event::Operation {
                    name: "transcript-entry".into(),
                    args: serde_json::to_string(&line).unwrap(),
                },
            },
            Record {
                id: 2,
                command_id: None,
                event: Event::Command {
                    name: "fork-context".into(),
                    payload: "old checkpoint".into(),
                },
            },
        ];
        let context = context_from_records(records).unwrap();
        assert!(!context.contains("old checkpoint"));
        assert!(!context.contains("Y2hpbGQ="));
        let mut checkpoint = crate::compact::checkpoint(&context, None);
        assert!(take_typed_history(&mut checkpoint).is_empty());
    }

    #[test]
    fn keeps_projected_events_and_full_payloads_in_order() {
        let context = context_from_records(vec![
            Record {
                id: 3,
                command_id: None,
                event: Event::Message {
                    role: "user".into(),
                    content: "question".into(),
                },
            },
            Record {
                id: 7,
                command_id: None,
                event: Event::ToolResult {
                    id: "t".into(),
                    output: "full result".into(),
                },
            },
        ])
        .unwrap();
        let json = context.lines().last().unwrap();
        let events: Value = serde_json::from_str(json).unwrap();
        assert_eq!(
            events,
            json!([
                {"id": 3, "command_id": null, "event": {"type": "message", "role": "user", "content": "question"}},
                {"id": 7, "command_id": null, "event": {"type": "tool_result", "id": "t", "output": "full result"}}
            ])
        );
    }
}

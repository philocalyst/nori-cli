//! Render an event-log slice as JSON within a byte budget.
//!
//! A history that fits is rendered whole. One that does not is walked from the end that matters
//! (the newest records, or for a page the oldest) and rendered at two fidelities: records near
//! that end keep long text up to a generous cut, records further in are lean, with tool
//! payloads cut hard and event noise (provider session events, bindings, commands) dropped.
//! Whatever is left out is counted in one `omitted` marker, every cut string says how many
//! bytes it lost, and no record, however large, may exceed a quarter of the budget.
//! The budget itself is owned by [`crate::context_budget`].

use super::types::TranscriptEntry;
use super::types::TranscriptLine;
use nori_conversation::Event;
use nori_conversation::Record;
use serde_json::Value;
use serde_json::json;

/// Which end of the slice the budget protects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Keep {
    /// The newest records: a fork or restore continues where the history ended.
    Newest,
    /// The oldest records: a page read forward from where its caller asked.
    Oldest,
}

/// What a record is, which decides how much of it survives a cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// What people and agents said, and how a turn ended.
    Text,
    /// A tool's input or output: bulky, and the first thing to cut.
    Tool,
    /// Bindings, commands, provider session events: only worth their bytes when there is room.
    Noise,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fidelity {
    Generous,
    Lean,
}

/// The rendered slice and what it covers.
#[derive(Debug)]
pub(crate) struct Digest {
    /// A JSON array of records, with an `omitted` marker element when anything was left out.
    pub(crate) json: String,
    /// Event IDs of the oldest and newest record rendered.
    pub(crate) included: Option<(i64, i64)>,
    /// How many records of the slice are not in `json`.
    pub(crate) omitted: usize,
    /// For [`Keep::Oldest`], the first record that did not fit, where the next page starts.
    pub(crate) next_id: Option<i64>,
}

/// The shape a record has in the rendered history.
fn event_value(event: &Event) -> Value {
    match event {
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
    }
}

fn record_value(record: &Record, event: Value) -> Value {
    json!({"id": record.id, "command_id": record.command_id, "event": event})
}

/// What kind of record a transcript entry is, from the entry itself.
fn entry_class(line: &TranscriptLine) -> Class {
    use nori_protocol::AcpEvent;
    use nori_protocol::SessionEvent;
    use nori_protocol::acp::v1 as acp;
    match &line.entry {
        TranscriptEntry::User(_) | TranscriptEntry::Assistant(_) => Class::Text,
        TranscriptEntry::ToolCall(_)
        | TranscriptEntry::ToolResult(_)
        | TranscriptEntry::PatchApply(_) => Class::Tool,
        TranscriptEntry::SessionMeta(_) | TranscriptEntry::ClientEvent(_) => Class::Noise,
        TranscriptEntry::SessionEvent(entry) => match &entry.event {
            SessionEvent::Acp(AcpEvent::Response { .. }) => Class::Text,
            SessionEvent::Acp(AcpEvent::Notification(
                acp::AgentNotification::SessionNotification(notification),
            )) => match &notification.update {
                acp::SessionUpdate::UserMessageChunk(_)
                | acp::SessionUpdate::AgentMessageChunk(_) => Class::Text,
                acp::SessionUpdate::ToolCall(_) | acp::SessionUpdate::ToolCallUpdate(_) => {
                    Class::Tool
                }
                _ => Class::Noise,
            },
            _ => Class::Noise,
        },
    }
}

/// One record, understood: its class, its rendered shape, and for a transcript entry the
/// parsed entry, so that its text can be cut inside the JSON it is stored as.
struct Prepared {
    class: Class,
    event: Value,
    entry: Option<Value>,
}

fn prepare(record: &Record) -> Prepared {
    let mut event = event_value(&record.event);
    let (class, entry) = match &record.event {
        Event::Message { .. } | Event::Rewind { .. } | Event::Fork { .. } => (Class::Text, None),
        Event::ToolCall { .. } | Event::ToolResult { .. } => (Class::Tool, None),
        Event::Operation { name, args } if name == "transcript-entry" => {
            match serde_json::from_str::<TranscriptLine>(args) {
                Ok(line) => (entry_class(&line), serde_json::from_str::<Value>(args).ok()),
                Err(_) => (Class::Noise, None),
            }
        }
        Event::Operation { .. }
        | Event::Command { .. }
        | Event::SessionEvent { .. }
        | Event::Unknown { .. } => (Class::Noise, None),
    };
    if entry.is_some() {
        event["args"] = Value::Null;
    }
    Prepared {
        class,
        event,
        entry,
    }
}

/// Cut every string inside `value` to `cap` bytes.
fn clip_strings(value: &mut Value, cap: usize) {
    match value {
        Value::String(text) => {
            if text.len() > cap {
                let clipped = crate::context_budget::clip(text, cap, 4).into_owned();
                *text = clipped;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| clip_strings(item, cap)),
        Value::Object(fields) => fields
            .values_mut()
            .for_each(|field| clip_strings(field, cap)),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// How long a string may be, by what the record is and how far in it sits, for a budget.
fn string_cap(class: Class, fidelity: Fidelity, max_bytes: usize) -> Option<usize> {
    let generous_text = (max_bytes / 8).clamp(4 * 1024, 128 * 1024);
    match (class, fidelity) {
        (Class::Text, Fidelity::Generous) => Some(generous_text),
        (Class::Text, Fidelity::Lean) => Some(generous_text.min(8 * 1024)),
        (Class::Tool, Fidelity::Generous) => Some(generous_text / 8),
        (Class::Tool, Fidelity::Lean) => Some(512),
        (Class::Noise, Fidelity::Generous) => Some(1024),
        (Class::Noise, Fidelity::Lean) => None,
    }
}

/// `prepared` as one JSON record at most `record_max` bytes long: its strings are cut to `cap`,
/// then to half of that again until it fits, and a record that cannot be made to fit is named
/// and nothing more.
fn render(record: &Record, prepared: &Prepared, mut cap: usize, record_max: usize) -> String {
    loop {
        let mut event = prepared.event.clone();
        clip_strings(&mut event, cap);
        // The entry is cut inside its own JSON, which is then stored as the string it was.
        if let Some(entry) = &prepared.entry {
            let mut entry = entry.clone();
            clip_strings(&mut entry, cap);
            event["args"] = Value::String(entry.to_string());
        }
        let text = record_value(record, event).to_string();
        if text.len() <= record_max {
            return text;
        }
        if cap < 64 {
            return record_value(
                record,
                json!({"type": "omitted", "note": "too large to include", "bytes": text.len()}),
            )
            .to_string();
        }
        cap /= 2;
    }
}

const OMITTED_NOTE: &str = "left out to keep this history within its size budget; long text inside the records kept is cut where it says '[… N bytes omitted]'. If you have the conversation_context tool, use it to read more of this conversation.";

fn marker(omitted: usize, keep: Keep) -> String {
    let side = match keep {
        Keep::Newest => "earlier",
        Keep::Oldest => "later",
    };
    json!({"type": "omitted", "records": omitted, "note": format!("{omitted} {side} records {OMITTED_NOTE}")}).to_string()
}

/// The records of `records`, oldest first, as a JSON array of at most about `max_bytes`.
pub(crate) fn digest(records: &[&Record], max_bytes: usize, keep: Keep) -> Digest {
    let ids = |taken: &[usize]| {
        taken
            .first()
            .zip(taken.last())
            .map(|(first, last)| (records[*first].id, records[*last].id))
    };
    // A history that fits is rendered as it is.
    let raw: usize = records
        .iter()
        .map(|record| match &record.event {
            Event::Message { content, .. } => content.len(),
            Event::ToolCall { input, .. } => input.len(),
            Event::ToolResult { output, .. } => output.len(),
            Event::Operation { args, .. } => args.len(),
            Event::Command { payload, .. } | Event::SessionEvent { payload } => payload.len(),
            Event::Unknown { form } => form.len(),
            Event::Rewind { .. } | Event::Fork { .. } => 0,
        })
        .sum();
    if raw <= max_bytes / 2 {
        let items: Vec<String> = records
            .iter()
            .map(|record| record_value(record, event_value(&record.event)).to_string())
            .collect();
        let json = format!("[{}]", items.join(","));
        if json.len() <= max_bytes {
            let all: Vec<usize> = (0..records.len()).collect();
            return Digest {
                json,
                included: ids(&all),
                omitted: 0,
                next_id: None,
            };
        }
    }

    let room = max_bytes.saturating_sub(OMITTED_NOTE.len() + 128);
    let generous_until = room / 2;
    let record_max = max_bytes / 4;
    let order: Vec<usize> = match keep {
        Keep::Newest => (0..records.len()).rev().collect(),
        Keep::Oldest => (0..records.len()).collect(),
    };
    let mut taken: Vec<(usize, String)> = Vec::new();
    let mut spent = 0;
    let mut next_id = None;
    for index in order {
        let record = records[index];
        let prepared = prepare(record);
        let fidelity = if spent < generous_until {
            Fidelity::Generous
        } else {
            Fidelity::Lean
        };
        let Some(cap) = string_cap(prepared.class, fidelity, max_bytes) else {
            continue;
        };
        let text = render(record, &prepared, cap, record_max);
        if spent + text.len() + 1 > room {
            next_id = Some(record.id);
            break;
        }
        spent += text.len() + 1;
        taken.push((index, text));
    }
    taken.sort_by_key(|(index, _)| *index);
    let omitted = records.len() - taken.len();
    let indices: Vec<usize> = taken.iter().map(|(index, _)| *index).collect();
    let mut items: Vec<String> = taken.into_iter().map(|(_, text)| text).collect();
    if omitted > 0 {
        match keep {
            Keep::Newest => items.insert(0, marker(omitted, keep)),
            Keep::Oldest => items.push(marker(omitted, keep)),
        }
    }
    Digest {
        json: format!("[{}]", items.join(",")),
        included: ids(&indices),
        omitted,
        next_id: match keep {
            Keep::Newest => None,
            Keep::Oldest => next_id,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn message(id: i64, role: &str, content: String) -> Record {
        Record {
            id,
            command_id: None,
            event: Event::Message {
                role: role.into(),
                content,
            },
        }
    }

    fn tool_result(id: i64, output: String) -> Record {
        Record {
            id,
            command_id: None,
            event: Event::ToolResult {
                id: format!("call-{id}"),
                output,
            },
        }
    }

    /// A conversation of `turns` exchanges, each with a tool result of `output_bytes`.
    fn conversation(turns: i64, output_bytes: usize) -> Vec<Record> {
        (0..turns)
            .flat_map(|turn| {
                [
                    message(turn * 3 + 1, "user", format!("question {turn}")),
                    tool_result(turn * 3 + 2, "o".repeat(output_bytes)),
                    message(turn * 3 + 3, "assistant", format!("answer {turn}")),
                ]
            })
            .collect()
    }

    #[test]
    fn a_history_that_fits_is_rendered_whole() {
        let records = conversation(2, 10);
        let refs: Vec<&Record> = records.iter().collect();
        let digest = digest(&refs, 64 * 1024, Keep::Newest);
        assert_eq!(digest.omitted, 0);
        assert_eq!(digest.included, Some((1, 6)));
        let value: Value = serde_json::from_str(&digest.json).unwrap();
        assert_eq!(value.as_array().unwrap().len(), 6);
        assert!(!digest.json.contains("omitted"));
    }

    #[test]
    fn hundreds_of_turns_with_megabyte_outputs_stay_in_budget_and_keep_the_newest_exchange() {
        let budget = 128 * 1024;
        let records = conversation(300, 3 * 1024 * 1024);
        let refs: Vec<&Record> = records.iter().collect();
        let digest = digest(&refs, budget, Keep::Newest);
        assert!(digest.json.len() <= budget, "{}", digest.json.len());
        let value: Value = serde_json::from_str(&digest.json).unwrap();
        let items = value.as_array().unwrap();
        assert_eq!(items[0]["type"], "omitted");
        let newest = items.last().unwrap();
        assert_eq!(newest["id"], 900);
        assert_eq!(newest["event"]["content"], "answer 299");
        assert!(digest.json.contains("question 299"));
        assert!(digest.json.contains("bytes omitted]"));
        assert!(digest.omitted > 0);
        assert_eq!(digest.included.map(|(_, last)| last), Some(900));
    }

    #[test]
    fn one_record_larger_than_the_budget_is_cut_not_dropped() {
        let budget = 64 * 1024;
        let records = [
            message(1, "user", "first".into()),
            message(2, "assistant", "é".repeat(20 * 1024 * 1024)),
        ];
        let refs: Vec<&Record> = records.iter().collect();
        let digest = digest(&refs, budget, Keep::Newest);
        assert!(digest.json.len() <= budget, "{}", digest.json.len());
        let value: Value = serde_json::from_str(&digest.json).unwrap();
        let newest = value.as_array().unwrap().last().unwrap();
        assert_eq!(newest["id"], 2);
        assert!(
            newest["event"]["content"]
                .as_str()
                .unwrap()
                .contains("bytes omitted]")
        );
    }

    #[test]
    fn transcript_entries_are_cut_inside_their_own_json() {
        let line = serde_json::json!({
            "ts": "2026-01-01T00:00:00.000Z",
            "v": 3,
            "type": "user",
            "id": "u1",
            "content": "u".repeat(5 * 1024 * 1024),
        });
        let records = [Record {
            id: 1,
            command_id: None,
            event: Event::Operation {
                name: "transcript-entry".into(),
                args: line.to_string(),
            },
        }];
        assert!(serde_json::from_str::<TranscriptLine>(&line.to_string()).is_ok());
        let refs: Vec<&Record> = records.iter().collect();
        let digest = digest(&refs, 64 * 1024, Keep::Newest);
        assert!(digest.json.len() <= 64 * 1024, "{}", digest.json.len());
        let value: Value = serde_json::from_str(&digest.json).unwrap();
        let args = value[0]["event"]["args"].as_str().unwrap();
        // Still a transcript entry a reader can parse.
        assert!(serde_json::from_str::<TranscriptLine>(args).is_ok());
    }

    #[test]
    fn a_page_keeps_its_oldest_records_and_says_where_the_next_begins() {
        let records = conversation(100, 20 * 1024);
        let refs: Vec<&Record> = records.iter().collect();
        let digest = digest(&refs, 64 * 1024, Keep::Oldest);
        assert!(digest.json.len() <= 64 * 1024, "{}", digest.json.len());
        let (first, last) = digest.included.unwrap();
        assert_eq!(first, 1);
        assert_eq!(digest.next_id, Some(last + 1));
        let value: Value = serde_json::from_str(&digest.json).unwrap();
        assert_eq!(value.as_array().unwrap().last().unwrap()["type"], "omitted");
    }
}

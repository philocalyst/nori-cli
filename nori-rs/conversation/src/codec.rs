use crate::Event;
use crate::Record;
use crate::SliceRef;
use nori_pkl::native::Entry;
use nori_pkl::native::Expr;
use nori_pkl::native::parse_document;
use serde_json::Value;
use serde_json::json;
use std::io;

/// The kinds of record this version understands; any other is kept as an unknown event.
const KINDS: [&str; 8] = [
    "message",
    "tool-call",
    "tool-result",
    "rewind",
    "fork",
    "operation",
    "command",
    "session-event",
];

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid conversation record")
}

// Each property is a complete Pkl object with a typed operation name and a
// JSON payload encoded as a Pkl string. The double encoding ensures all JSON
// control escapes remain ordinary literal backslashes in Pkl.
pub(crate) fn encode(record: &Record) -> io::Result<String> {
    let (kind, data) = match &record.event {
        Event::Message { role, content } => ("message", json!({"role": role, "content": content})),
        Event::ToolCall { id, name, input } => {
            ("tool-call", json!({"id": id, "name": name, "input": input}))
        }
        Event::ToolResult { id, output } => ("tool-result", json!({"id": id, "output": output})),
        Event::Rewind { head } => ("rewind", json!({"head": head})),
        Event::Fork { source } => ("fork", json!({"start": source.start, "end": source.end})),
        Event::Operation { name, args } => ("operation", json!({"name": name, "args": args})),
        Event::Command { name, payload } => ("command", json!({"name": name, "payload": payload})),
        Event::SessionEvent { payload } => ("session-event", json!({"payload": payload})),
        Event::Unknown { form } => return Ok(format!("{form}\n")),
    };
    let payload = serde_json::to_string(&data.to_string()).map_err(io::Error::other)?;
    let command = record
        .command_id
        .as_ref()
        .map_or_else(String::new, |id| format!("; commandId = \"{id}\""));
    Ok(format!(
        "event_{} = new {{ kind = \"{kind}\"; data = {payload}{command} }}\n",
        record.id
    ))
}

pub(crate) fn decode(line: &str) -> io::Result<Record> {
    // Nearly every line is one this module wrote, in exactly the shape `encode` writes it; read
    // that shape directly, and give everything else to the Pkl parser.
    if let Some(record) = decode_canonical(line) {
        return record;
    }
    // Inspect literal syntax only. Importing or evaluating a journal would
    // allow replay to repeat effects; whitespace and comments are harmless.
    let document =
        parse_document(line).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if !document.module().imports.is_empty()
        || document.module().amends.is_some()
        || document.module().extends.is_some()
        || document.module().body.len() != 1
    {
        return Err(invalid());
    }
    let Entry::Property(property) = &document.module().body[0] else {
        return Err(invalid());
    };
    let id: i64 = property
        .name
        .strip_prefix("event_")
        .ok_or_else(invalid)?
        .parse()
        .map_err(|_| invalid())?;
    if id <= 0 {
        return Err(invalid());
    }
    let Some(Expr::New(_, body, _)) = &property.value else {
        return Err(invalid());
    };
    let kind = literal_string(body, "kind")?.ok_or_else(invalid)?;
    let command_id = literal_string(body, "commandId")?;
    let event = if KINDS.contains(&kind.as_str()) {
        let payload = literal_string(body, "data")?.ok_or_else(invalid)?;
        event_of(&kind, &payload)?
    } else {
        Event::Unknown {
            form: line.to_owned(),
        }
    };
    Ok(Record {
        id,
        command_id,
        event,
    })
}

fn literal_string(body: &[Entry], name: &str) -> io::Result<Option<String>> {
    let mut fields = body.iter().filter_map(|entry| match entry {
        Entry::Property(property) if property.name.as_ref() == name => Some(property),
        _ => None,
    });
    let Some(field) = fields.next() else {
        return Ok(None);
    };
    if fields.next().is_some() {
        return Err(invalid());
    }
    match field.value.as_ref() {
        Some(Expr::String(value)) => Ok(Some(value.to_string())),
        _ => Err(invalid()),
    }
}

/// The event a record of `kind` carries in its JSON `payload`.
fn event_of(kind: &str, payload: &str) -> io::Result<Event> {
    let data: Value = serde_json::from_str(payload).map_err(|_| invalid())?;
    let string = |key: &str| {
        data.get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(invalid)
    };
    let integer = |key: &str| data.get(key).and_then(Value::as_i64).ok_or_else(invalid);
    Ok(match kind {
        "message" => Event::Message {
            role: string("role")?,
            content: string("content")?,
        },
        "tool-call" => Event::ToolCall {
            id: string("id")?,
            name: string("name")?,
            input: string("input")?,
        },
        "tool-result" => Event::ToolResult {
            id: string("id")?,
            output: string("output")?,
        },
        "rewind" => Event::Rewind {
            head: match data.get("head") {
                Some(Value::Null) => None,
                Some(value) => Some(value.as_i64().ok_or_else(invalid)?),
                None => return Err(invalid()),
            },
        },
        "fork" => Event::Fork {
            source: SliceRef {
                start: integer("start")?,
                end: integer("end")?,
            },
        },
        "operation" => Event::Operation {
            name: string("name")?,
            args: string("args")?,
        },
        "command" => Event::Command {
            name: string("name")?,
            payload: string("payload")?,
        },
        "session-event" => Event::SessionEvent {
            payload: string("payload")?,
        },
        _ => return Err(invalid()),
    })
}

/// A line in exactly the shape [`encode`] writes, read without a Pkl parser:
/// `event_N = new { kind = "K"; data = "…"[; commandId = "C"] }`. `None` for any other shape,
/// which the parser then reads. The data literal holds only `\\` and `\"` escapes, so it is a
/// JSON string literal as well as a Pkl one.
fn decode_canonical(line: &str) -> Option<io::Result<Record>> {
    let rest = line.strip_prefix("event_")?;
    let digits = rest.find(|c: char| !c.is_ascii_digit())?;
    let id: i64 = rest[..digits].parse().ok().filter(|id| *id > 0)?;
    let rest = rest[digits..].strip_prefix(" = new { kind = \"")?;
    let (kind, rest) = rest.split_once('"')?;
    let rest = rest.strip_prefix("; data = ")?;
    let literal_end = json_string_end(rest)?;
    let (literal, rest) = rest.split_at(literal_end);
    let (command_id, rest) = match rest.strip_prefix("; commandId = \"") {
        Some(rest) => {
            let (id, rest) = rest.split_once('"')?;
            (Some(id.to_owned()), rest)
        }
        None => (None, rest),
    };
    if rest != " }" {
        return None;
    }
    let payload: String = serde_json::from_str(literal).ok()?;
    if !KINDS.contains(&kind) {
        // A future operation is kept as written.
        return Some(Ok(Record {
            id,
            command_id,
            event: Event::Unknown {
                form: line.to_owned(),
            },
        }));
    }
    Some(event_of(kind, &payload).map(|event| Record {
        id,
        command_id,
        event,
    }))
}

/// The length of the JSON string literal `text` starts with, including both quotes.
fn json_string_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'"') {
        return None;
    }
    let mut index = 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => return Some(index + 1),
            _ => index += 1,
        }
    }
    None
}

use crate::Event;
use crate::EventId;
use crate::Kind;
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
        event_of(Kind::of(&kind), &payload)?
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

/// The fields a record's JSON payload may carry; which ones a kind needs is up to the kind.
#[derive(serde::Deserialize)]
struct Data {
    role: Option<String>,
    content: Option<String>,
    id: Option<String>,
    name: Option<String>,
    input: Option<String>,
    output: Option<String>,
    args: Option<String>,
    payload: Option<String>,
}

/// The event a record of `kind` carries in its JSON `payload`.
fn event_of(kind: Kind, payload: &str) -> io::Result<Event> {
    match kind {
        Kind::Rewind | Kind::Fork => {
            let data: Value = serde_json::from_str(payload).map_err(|_| invalid())?;
            let integer = |key: &str| data.get(key).and_then(Value::as_i64).ok_or_else(invalid);
            Ok(if kind == Kind::Rewind {
                Event::Rewind {
                    head: match data.get("head") {
                        Some(Value::Null) => None,
                        Some(value) => Some(value.as_i64().ok_or_else(invalid)?),
                        None => return Err(invalid()),
                    },
                }
            } else {
                Event::Fork {
                    source: SliceRef {
                        start: integer("start")?,
                        end: integer("end")?,
                    },
                }
            })
        }
        Kind::Message
        | Kind::ToolCall
        | Kind::ToolResult
        | Kind::Operation
        | Kind::Command
        | Kind::SessionEvent => {
            // Typed straight from the text: no map of values is built for the megabytes a
            // record can carry.
            let data: Data = serde_json::from_str(payload).map_err(|_| invalid())?;
            let field = |value: Option<String>| value.ok_or_else(invalid);
            Ok(match kind {
                Kind::Message => Event::Message {
                    role: field(data.role)?,
                    content: field(data.content)?,
                },
                Kind::ToolCall => Event::ToolCall {
                    id: field(data.id)?,
                    name: field(data.name)?,
                    input: field(data.input)?,
                },
                Kind::ToolResult => Event::ToolResult {
                    id: field(data.id)?,
                    output: field(data.output)?,
                },
                Kind::Operation => Event::Operation {
                    name: field(data.name)?,
                    args: field(data.args)?,
                },
                Kind::Command => Event::Command {
                    name: field(data.name)?,
                    payload: field(data.payload)?,
                },
                Kind::SessionEvent => Event::SessionEvent {
                    payload: field(data.payload)?,
                },
                Kind::Rewind | Kind::Fork | Kind::Unknown => return Err(invalid()),
            })
        }
        Kind::Unknown => Err(invalid()),
    }
}

/// A line in exactly the shape [`encode`] writes, read without a Pkl parser:
/// `event_N = new { kind = "K"; data = "…"[; commandId = "C"] }`. `None` for any other shape,
/// which the parser then reads. The data literal holds only `\\` and `\"` escapes, so it is a
/// JSON string literal as well as a Pkl one.
fn decode_canonical(line: &str) -> Option<io::Result<Record>> {
    let located = locate(line)?;
    let command_id = located.command_id.map(str::to_owned);
    if located.kind == Kind::Unknown {
        // A future operation is kept as written.
        return Some(Ok(Record {
            id: located.id,
            command_id,
            event: Event::Unknown {
                form: line.to_owned(),
            },
        }));
    }
    let payload: String = serde_json::from_str(located.literal).ok()?;
    Some(event_of(located.kind, &payload).map(|event| Record {
        id: located.id,
        command_id,
        event,
    }))
}

/// What a record line says about itself, read without decoding its payload.
pub(crate) struct Located<'a> {
    pub(crate) id: EventId,
    pub(crate) kind: Kind,
    /// The operation's or command's name.
    pub(crate) name: Option<&'a str>,
    pub(crate) command_id: Option<&'a str>,
    /// The data literal, quotes and escapes included.
    pub(crate) literal: &'a str,
}

/// Read a line written in exactly the shape [`encode`] writes for its id, kind, name and command
/// id, from its first and last few bytes: the payload is not read. `None` for any other shape,
/// which [`decode`] then reads whole.
///
/// A name is the data's first or last key (`{"name":…,"args":…}` in insertion order,
/// `{"args":…,"name":…}` sorted). A command id cannot be confused with the data: every quote in
/// the data literal is escaped, so `= "` appears only after `commandId`.
pub(crate) fn locate(line: &str) -> Option<Located<'_>> {
    let rest = line.strip_prefix("event_")?;
    let digits = rest.find(|c: char| !c.is_ascii_digit())?;
    let id: EventId = rest[..digits].parse().ok().filter(|id| *id > 0)?;
    let rest = rest[digits..].strip_prefix(" = new { kind = \"")?;
    let (kind, rest) = rest.split_once('"')?;
    let kind = Kind::of(kind);
    let quoted = rest.strip_prefix("; data = ")?;
    let data = quoted.strip_prefix('"')?;
    let tail = &line[line.floor_char_boundary(line.len().saturating_sub(160))..];
    let (data, command_id) = match tail.rfind("; commandId = \"") {
        Some(at) => {
            let id = tail[at + "; commandId = \"".len()..].strip_suffix("\" }")?;
            if id.is_empty()
                || !id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
            {
                return None;
            }
            let end = data.len().checked_sub(tail.len() - at)?;
            (&data[..end], Some(id))
        }
        None => (data.strip_suffix(" }")?, None),
    };
    let data = data.strip_suffix('"')?;
    let name = match kind {
        Kind::Operation | Kind::Command => {
            const KEY: &str = "\\\"name\\\":\\\"";
            if let Some(rest) = data
                .strip_prefix('{')
                .and_then(|data| data.strip_prefix(KEY))
            {
                Some(&rest[..rest.find('\\')?])
            } else {
                let rest = data.strip_suffix("\\\"}")?;
                let window = rest.floor_char_boundary(rest.len().saturating_sub(128));
                let name = &rest[window + rest[window..].rfind(KEY)? + KEY.len()..];
                (!name.contains('\\')).then_some(name)
            }
        }
        Kind::Message
        | Kind::ToolCall
        | Kind::ToolResult
        | Kind::Rewind
        | Kind::Fork
        | Kind::SessionEvent
        | Kind::Unknown => None,
    };
    Some(Located {
        id,
        kind,
        name,
        command_id,
        literal: &quoted[..data.len() + 2],
    })
}

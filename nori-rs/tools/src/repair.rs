//! Turning a model's tool call into typed arguments.
//!
//! Models call these tools with other harnesses' argument names (`path`, `filePath`, `old_str`),
//! stringified numbers and booleans, or a JSON array sent as a string. The repairs here are the
//! unambiguous ones only: renaming a known alias to the canonical key (never overriding a
//! canonical key that is present) and coercing a value to the type the schema declares.

use serde::Deserialize;
use serde_json::Map;
use serde_json::Value;

use crate::error::ToolError;
use crate::spec::ToolKind;

#[derive(Debug, Deserialize)]
pub(crate) struct ReadArgs {
    pub file_path: String,
    #[serde(default)]
    pub offset: Option<i64>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WriteArgs {
    pub file_path: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EditArgs {
    pub file_path: String,
    pub old_string: String,
    pub new_string: String,
    #[serde(default)]
    pub replace_all: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EditItem {
    pub old_string: String,
    pub new_string: String,
    #[serde(default)]
    pub replace_all: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MultiEditArgs {
    pub file_path: String,
    pub edits: Vec<EditItem>,
}

pub(crate) enum ToolCall {
    Read(ReadArgs),
    Write(WriteArgs),
    Edit(EditArgs),
    MultiEdit(MultiEditArgs),
    ApplyPatch(String),
}

const FILE_PATH: &[&str] = &[
    "path",
    "filePath",
    "filepath",
    "file",
    "filename",
    "file_name",
    "target_file",
    "target_path",
];
const OLD_STRING: &[&str] = &[
    "old_str",
    "oldString",
    "old_text",
    "oldText",
    "old",
    "search",
    "find",
];
const NEW_STRING: &[&str] = &[
    "new_str",
    "newString",
    "new_text",
    "newText",
    "new",
    "replace",
    "replacement",
];
const REPLACE_ALL: &[&str] = &["replaceAll", "allow_multiple", "all"];
const CONTENT: &[&str] = &["contents", "file_text", "fileText", "text", "data"];
const OFFSET: &[&str] = &["start_line", "startLine", "line_offset"];
const LIMIT: &[&str] = &["max_lines", "maxLines", "num_lines"];
const PATCH: &[&str] = &["patch", "patchText", "patch_text", "diff", "content"];

/// Move the first present alias to `canonical` when `canonical` is absent.
fn rename(object: &mut Map<String, Value>, canonical: &str, aliases: &[&str]) {
    if object.contains_key(canonical) {
        return;
    }
    for alias in aliases {
        if let Some(value) = object.remove(*alias) {
            object.insert(canonical.to_owned(), value);
            return;
        }
    }
}

fn coerce_int(object: &mut Map<String, Value>, key: &str) {
    let Some(value) = object.get(key) else {
        return;
    };
    let replacement = match value {
        Value::String(text) => text.trim().parse::<i64>().ok(),
        Value::Number(number) if number.as_i64().is_none() => {
            number.as_f64().map(|float| float.trunc() as i64)
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => {
            None
        }
    };
    if let Some(number) = replacement {
        object.insert(key.to_owned(), Value::from(number));
    } else if object.get(key).is_some_and(Value::is_null) {
        object.remove(key);
    }
}

fn coerce_bool(object: &mut Map<String, Value>, key: &str) {
    if let Some(Value::String(text)) = object.get(key) {
        match text.trim().to_ascii_lowercase().as_str() {
            "true" => {
                object.insert(key.to_owned(), Value::Bool(true));
            }
            "false" => {
                object.insert(key.to_owned(), Value::Bool(false));
            }
            _ => {}
        }
    }
}

fn repair_edit_object(object: &mut Map<String, Value>) {
    rename(object, "old_string", OLD_STRING);
    rename(object, "new_string", NEW_STRING);
    rename(object, "replace_all", REPLACE_ALL);
    coerce_bool(object, "replace_all");
}

fn repair_object(kind: ToolKind, object: &mut Map<String, Value>) {
    match kind {
        ToolKind::Read => {
            rename(object, "file_path", FILE_PATH);
            rename(object, "offset", OFFSET);
            rename(object, "limit", LIMIT);
            // `end_line` with a start line is a range; turn it into a limit.
            let end = object
                .remove("end_line")
                .or_else(|| object.remove("endLine"));
            coerce_int(object, "offset");
            coerce_int(object, "limit");
            let end = end.and_then(|end| {
                end.as_i64()
                    .or_else(|| end.as_str().and_then(|text| text.trim().parse().ok()))
            });
            if !object.contains_key("limit")
                && let Some(end) = end
                && let Some(start) = object.get("offset").and_then(Value::as_i64)
                && let Some(limit) = end.checked_sub(start).and_then(|span| span.checked_add(1))
                && limit > 0
            {
                object.insert("limit".to_owned(), Value::from(limit));
            }
        }
        ToolKind::Write => {
            rename(object, "file_path", FILE_PATH);
            rename(object, "content", CONTENT);
        }
        ToolKind::Edit => {
            rename(object, "file_path", FILE_PATH);
            repair_edit_object(object);
        }
        ToolKind::MultiEdit => {
            rename(object, "file_path", FILE_PATH);
            rename(object, "edits", &["changes"]);
            if let Some(Value::String(text)) = object.get("edits")
                && let Ok(parsed @ Value::Array(_)) = serde_json::from_str::<Value>(text)
            {
                object.insert("edits".to_owned(), parsed);
            }
            if let Some(Value::Array(items)) = object.get_mut("edits") {
                for item in items {
                    if let Value::Object(item) = item {
                        repair_edit_object(item);
                    }
                }
            }
        }
        ToolKind::ApplyPatch => {}
    }
}

fn invalid(kind: ToolKind, detail: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs {
        tool: kind.name(),
        detail: detail.into(),
    }
}

/// The patch text from the call's arguments: a bare string or an object holding it.
fn patch_text(args: Value) -> Result<String, ToolError> {
    match args {
        Value::String(text) => Ok(text),
        Value::Object(mut object) => {
            rename(&mut object, "input", PATCH);
            match object.remove("input") {
                Some(Value::String(text)) => Ok(text),
                Some(_) | None => Err(invalid(
                    ToolKind::ApplyPatch,
                    "pass the whole patch text (from `*** Begin Patch` to `*** End Patch`) as `input`",
                )),
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) => Err(invalid(
            ToolKind::ApplyPatch,
            "pass the whole patch text as a string",
        )),
    }
}

/// Anthropic's `str_replace_based_edit_tool` multiplexes several operations on `command`.
fn text_editor_kind(object: &Map<String, Value>) -> Result<ToolKind, ToolError> {
    match object.get("command").and_then(Value::as_str) {
        Some("view") => Ok(ToolKind::Read),
        Some("str_replace") => Ok(ToolKind::Edit),
        Some("create") => Ok(ToolKind::Write),
        Some(other) => Err(invalid(
            ToolKind::Edit,
            format!(
                "command `{other}` is not supported; use `read`, `edit` (old_string/new_string), \
                 `multi_edit` or `write`"
            ),
        )),
        None => Err(invalid(
            ToolKind::Edit,
            "missing `command` (view, str_replace or create)",
        )),
    }
}

fn is_text_editor(name: &str) -> bool {
    let name = name.rsplit("__").next().unwrap_or(name);
    matches!(name, "str_replace_based_edit_tool" | "str_replace_editor")
}

pub(crate) fn parse_call(name: &str, args: Value) -> Result<ToolCall, ToolError> {
    // Some providers send an object's arguments as a JSON string.
    let args = match args {
        Value::String(text) if !matches!(ToolKind::resolve(name), Some(ToolKind::ApplyPatch)) => {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        }
        other => other,
    };

    let kind = if is_text_editor(name) {
        let Value::Object(object) = &args else {
            return Err(invalid(ToolKind::Edit, "arguments must be a JSON object"));
        };
        text_editor_kind(object)?
    } else {
        ToolKind::resolve(name).ok_or_else(|| ToolError::UnknownTool {
            name: name.to_owned(),
            available: ToolKind::ALL
                .iter()
                .map(|kind| kind.name())
                .collect::<Vec<_>>()
                .join(", "),
        })?
    };

    if kind == ToolKind::ApplyPatch {
        return patch_text(args).map(ToolCall::ApplyPatch);
    }
    let Value::Object(mut object) = args else {
        return Err(invalid(kind, "arguments must be a JSON object"));
    };
    if is_text_editor(name) {
        object.remove("command");
        if kind == ToolKind::Write {
            rename(&mut object, "content", &["file_text"]);
        }
    }
    repair_object(kind, &mut object);
    let value = Value::Object(object);

    let expected = |error: serde_json::Error| {
        let schema = kind.spec().input_schema;
        let required = schema["required"].to_string();
        invalid(kind, format!("{error} (required arguments: {required})"))
    };
    let parsed = match kind {
        ToolKind::Read => serde_json::from_value(value).map(ToolCall::Read),
        ToolKind::Write => serde_json::from_value(value).map(ToolCall::Write),
        ToolKind::Edit => serde_json::from_value(value).map(ToolCall::Edit),
        ToolKind::MultiEdit => serde_json::from_value(value).map(ToolCall::MultiEdit),
        ToolKind::ApplyPatch => return patch_text(value).map(ToolCall::ApplyPatch),
    };
    parsed.map_err(expected)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    fn edit(name: &str, args: Value) -> EditArgs {
        match parse_call(name, args).expect("parse") {
            ToolCall::Edit(edit) => edit,
            _ => panic!("not an edit"),
        }
    }

    #[test]
    fn other_harnesses_argument_names_are_renamed() {
        let parsed = edit(
            "str_replace",
            json!({"filePath": "a.rs", "oldString": "x", "newString": "y", "replaceAll": "true"}),
        );
        assert_eq!(parsed.file_path, "a.rs");
        assert_eq!(parsed.old_string, "x");
        assert_eq!(parsed.new_string, "y");
        assert!(parsed.replace_all);
    }

    #[test]
    fn canonical_keys_win_over_aliases() {
        let parsed = edit(
            "edit",
            json!({"file_path": "a", "path": "b", "old_string": "x", "new_string": "y"}),
        );
        assert_eq!(parsed.file_path, "a");
    }

    #[test]
    fn multi_edit_accepts_a_stringified_array_and_aliased_items() {
        let args = json!({
            "path": "a.rs",
            "edits": "[{\"old_str\": \"a\", \"new_str\": \"b\"}]"
        });
        let ToolCall::MultiEdit(parsed) = parse_call("MultiEdit", args).expect("parse") else {
            panic!("not multi_edit");
        };
        assert_eq!(parsed.edits.len(), 1);
        assert_eq!(parsed.edits[0].new_string, "b");
    }

    #[test]
    fn read_coerces_numbers_and_converts_a_line_range() {
        let ToolCall::Read(parsed) = parse_call(
            "read_file",
            json!({"path": "a", "start_line": "10", "end_line": 19}),
        )
        .expect("parse") else {
            panic!("not read");
        };
        assert_eq!(parsed.offset, Some(10));
        assert_eq!(parsed.limit, Some(10));
    }

    #[test]
    fn extreme_line_ranges_do_not_overflow() {
        let ToolCall::Read(parsed) = parse_call(
            "read",
            json!({"path": "a", "start_line": i64::MIN, "end_line": i64::MAX}),
        )
        .expect("parse") else {
            panic!("not read");
        };
        assert_eq!(parsed.limit, None);
    }

    #[test]
    fn anthropic_text_editor_commands_map_to_tools() {
        let ToolCall::Edit(parsed) = parse_call(
            "str_replace_based_edit_tool",
            json!({"command": "str_replace", "path": "a", "old_str": "x", "new_str": "y"}),
        )
        .expect("parse") else {
            panic!("not edit");
        };
        assert_eq!(parsed.new_string, "y");
        assert!(parse_call("str_replace_based_edit_tool", json!({"command": "insert"})).is_err());
    }

    #[test]
    fn patch_text_comes_from_a_string_or_an_object() {
        for args in [
            json!("*** Begin Patch"),
            json!({"input": "*** Begin Patch"}),
            json!({"patch": "*** Begin Patch"}),
        ] {
            let ToolCall::ApplyPatch(text) = parse_call("apply_patch", args).expect("parse") else {
                panic!("not a patch");
            };
            assert_eq!(text, "*** Begin Patch");
        }
    }

    #[test]
    fn missing_arguments_say_what_is_required() {
        let error = parse_call("edit", json!({"file_path": "a"}))
            .err()
            .expect("error");
        assert!(error.to_string().contains("required arguments"), "{error}");
    }

    #[test]
    fn unknown_tool_lists_the_available_ones() {
        let error = parse_call("bash", json!({})).err().expect("error");
        assert!(error.to_string().contains("multi_edit"), "{error}");
    }
}

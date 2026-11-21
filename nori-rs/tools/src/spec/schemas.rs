//! JSON Schemas of the tool inputs.

use serde_json::Value;
use serde_json::json;

use super::ToolKind;

fn edit_item_properties() -> Value {
    json!({
        "old_string": {
            "type": "string",
            "description": "The exact text to find, including indentation. Must match exactly one place unless replace_all is true. Never include read's line-number prefixes."
        },
        "new_string": {
            "type": "string",
            "description": "The text to replace old_string with. Must differ from old_string. Written literally: no placeholders."
        },
        "replace_all": {
            "type": "boolean",
            "description": "Replace every occurrence of old_string instead of requiring a unique match. Defaults to false."
        }
    })
}

pub(super) fn input_schema(kind: ToolKind) -> Value {
    match kind {
        ToolKind::Read => json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The file or directory to read: absolute, or relative to the working directory."
                },
                "offset": {
                    "type": "integer",
                    "description": "The line (or directory entry) to start reading from, 1-based. Defaults to 1."
                },
                "limit": {
                    "type": "integer",
                    "description": "The maximum number of lines (or entries) to read. Defaults to, and is capped at, 2000."
                }
            },
            "required": ["file_path"]
        }),
        ToolKind::Write => json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The file to create or overwrite. Missing parent directories are created."
                },
                "content": {
                    "type": "string",
                    "description": "The complete content of the file, written literally."
                }
            },
            "required": ["file_path", "content"]
        }),
        ToolKind::Edit => {
            let mut properties = edit_item_properties();
            properties["file_path"] = json!({
                "type": "string",
                "description": "The file to edit: absolute, or relative to the working directory. Use a path that does not exist, with an empty old_string, to create a file."
            });
            json!({
                "type": "object",
                "properties": properties,
                "required": ["file_path", "old_string", "new_string"]
            })
        }
        ToolKind::MultiEdit => json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The file to edit: absolute, or relative to the working directory."
                },
                "edits": {
                    "type": "array",
                    "description": "The edits, applied in order; each sees the result of the earlier ones. All are applied or none.",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": edit_item_properties(),
                        "required": ["old_string", "new_string"]
                    }
                }
            },
            "required": ["file_path", "edits"]
        }),
        ToolKind::ApplyPatch => json!({
            "type": "object",
            "properties": {
                "input": {
                    "type": "string",
                    "description": "The entire patch, from `*** Begin Patch` to `*** End Patch`."
                }
            },
            "required": ["input"]
        }),
    }
}

//! What a tool call returns: text for the model plus structured detail for the host.

use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;

use crate::edit::MatchTier;
use crate::error::ToolError;

/// How a file changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Created,
    Modified,
    Deleted,
}

/// One file's change, in the shape a host needs for an ACP diff or an undo journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: PathBuf,
    pub kind: ChangeKind,
    /// The whole file before the change; `None` for a created file.
    pub old_text: Option<String>,
    /// The whole file after the change; `None` for a deleted file.
    pub new_text: Option<String>,
    /// A unified diff with three lines of context.
    pub unified_diff: String,
    pub lines_added: i64,
    pub lines_removed: i64,
}

/// A short structured summary of an edit, for the UI and for tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditSummary {
    pub lines_added: i64,
    pub lines_removed: i64,
    /// How many places were replaced, across all edits of the call.
    pub replacements: i64,
    /// The loosest tier any edit needed; `None` when nothing was matched (a create or write).
    pub tier: Option<MatchTier>,
    /// False when the call left the file untouched.
    pub changed: bool,
    /// Plain-language notes the model also sees, such as "re-indented by +4 spaces".
    pub notes: Vec<String>,
}

/// An image the model should look at (a `read` of a png, say).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageAttachment {
    pub mime_type: String,
    pub data: Vec<u8>,
}

/// The result of one tool call. Maps directly onto a host tool output: `text` and `is_error`
/// are the model's view; `changes`, `summary` and `images` let a host render diffs and media.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub text: String,
    pub is_error: bool,
    pub changes: Vec<FileChange>,
    pub summary: Option<EditSummary>,
    pub images: Vec<ImageAttachment>,
}

impl ToolResult {
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
            changes: Vec::new(),
            summary: None,
            images: Vec::new(),
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            is_error: true,
            ..Self::ok(text)
        }
    }

    pub fn from_error(error: &ToolError) -> Self {
        Self::error(error.to_string())
    }

    /// The first change's unified diff, when the call changed a file.
    pub fn diff(&self) -> Option<&str> {
        self.changes
            .first()
            .map(|change| change.unified_diff.as_str())
    }

    /// Structured detail without the file bodies, suitable for MCP `structuredContent`.
    pub fn structured(&self) -> Value {
        let changes: Vec<Value> = self
            .changes
            .iter()
            .map(|change| {
                json!({
                    "path": change.path,
                    "kind": change.kind,
                    "lines_added": change.lines_added,
                    "lines_removed": change.lines_removed,
                    "unified_diff": change.unified_diff,
                })
            })
            .collect();
        json!({"changes": changes, "summary": self.summary})
    }
}

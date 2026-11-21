//! Typed tool errors. Every variant renders to text that tells the model how to recover.

use std::path::Path;
use std::path::PathBuf;

use thiserror::Error;

use crate::edit::EditError;
use crate::patch::PatchError;

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("Unknown tool `{name}`. Available tools: {available}.")]
    UnknownTool { name: String, available: String },

    #[error("Invalid arguments for `{tool}`: {detail}")]
    InvalidArgs { tool: &'static str, detail: String },

    #[error("{}", not_found_text(path, suggestions))]
    NotFound {
        path: PathBuf,
        suggestions: Vec<PathBuf>,
    },

    #[error(
        "{} is a directory, not a file. Use `read` to list its entries, or pass a file inside it.",
        path.display()
    )]
    IsDirectory { path: PathBuf },

    #[error(
        "You have not read {path} in this session, so it was not changed. Call `read` on it \
         first, then retry. (This protects edits the user or another tool made that you have \
         not seen.)",
        path = path.display()
    )]
    NotRead { path: PathBuf },

    #[error(
        "{path} changed on disk since you last read it (the user, a formatter or another tool \
         edited it), so it was not changed. Call `read` on it again to see the current contents, \
         then redo your change against what you read.",
        path = path.display()
    )]
    ChangedSinceRead { path: PathBuf },

    #[error(
        "{path} is a binary file ({bytes} bytes) and cannot be read or edited as text. Use a \
         shell command such as `file` or `xxd | head` to inspect it.",
        path = path.display()
    )]
    Binary { path: PathBuf, bytes: i64 },

    #[error(
        "{path} is not valid UTF-8 text, so it cannot be edited without corrupting it. Use a \
         shell command (iconv, sed) for files in another encoding.",
        path = path.display()
    )]
    NotUtf8 { path: PathBuf },

    #[error(
        "{path} is too large ({bytes} bytes; the limit is {limit}). Use `grep` to find what you \
         need, or a shell command such as `sed -n '100,200p'` to view a slice.",
        path = path.display()
    )]
    TooLarge {
        path: PathBuf,
        bytes: i64,
        limit: i64,
    },

    #[error(
        "offset {offset} is past the end of {path}, which has {total_lines} lines. Use an offset \
         between 1 and {total_lines}.",
        path = path.display()
    )]
    OffsetPastEnd {
        path: PathBuf,
        offset: i64,
        total_lines: i64,
    },

    #[error(
        "{path} already exists. To change it use `edit` (after `read`), `write` to replace it \
         whole, or an `*** Update File` hunk; `*** Add File` only creates new files.",
        path = path.display()
    )]
    AlreadyExists { path: PathBuf },

    #[error("{path}: {source}", path = path.display())]
    Edit {
        path: PathBuf,
        #[source]
        source: EditError,
    },

    #[error(transparent)]
    Patch(#[from] PatchError),

    #[error("Could not {op} {path}: {source}", path = path.display())]
    Io {
        op: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cancelled by the user")]
    Cancelled,

    #[error("Internal error: {0}")]
    Internal(String),
}

fn not_found_text(path: &Path, suggestions: &[PathBuf]) -> String {
    let mut text = format!("File not found: {}", path.display());
    if suggestions.is_empty() {
        text.push_str("\nNo similarly named files were found nearby. Use `glob`/`grep` or `read` on the parent directory to locate it.");
    } else {
        text.push_str("\nDid you mean one of these?");
        for suggestion in suggestions {
            text.push_str("\n  ");
            text.push_str(&suggestion.display().to_string());
        }
    }
    text
}

impl ToolError {
    pub(crate) fn io(op: &'static str, path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            op,
            path: path.to_path_buf(),
            source,
        }
    }
}

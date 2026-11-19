//! What a file held before one of the CLI's own file-editing tools ran, so that the tool's result
//! can be reported to the client as a diff of the whole file.
//!
//! Undo and rewind record only the edits an agent reports as ACP diffs. The tools that stand in
//! for the CLI's Edit and Write report theirs themselves (`mcp/tools`); the CLI's own
//! `MultiEdit` and `NotebookEdit` run inside the CLI. The PreToolUse hook reads the file before
//! such a tool runs ([`EditSnapshots::record`]), and the converter reads it again when the result
//! arrives ([`EditSnapshots::diff`]). Nothing is inferred from the tool's input except which file
//! it names.

use std::path::Path;
use std::path::PathBuf;

use dashmap::DashMap;
use nori_acp_kit::schema::Diff;
use serde_json::Value;

use crate::mcp::ACP_TOOL_PREFIX;
use crate::mcp::MAX_JOURNALED_BYTES;

/// The file a CLI-owned editing tool names: `file_path` for MultiEdit, `notebook_path` for
/// NotebookEdit. `None` for any other tool, and for the `mcp__acp__` tools, which report their own
/// diffs.
fn edited_file(tool_name: &str, input: &Value, cwd: &Path) -> Option<PathBuf> {
    if tool_name.starts_with(ACP_TOOL_PREFIX) {
        return None;
    }
    let key = match tool_name {
        "MultiEdit" => "file_path",
        "NotebookEdit" => "notebook_path",
        _ => return None,
    };
    let path = PathBuf::from(input.get(key)?.as_str()?);
    Some(if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    })
}

/// A file's text, or `None` when it does not exist. A file that is too large or not text yields
/// `Err`: it cannot be journaled.
fn read_text(path: &Path) -> Result<Option<String>, ()> {
    match std::fs::metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(()),
        Ok(metadata) if metadata.len() > MAX_JOURNALED_BYTES => Err(()),
        Ok(_) => std::fs::read_to_string(path).map(Some).map_err(|_| ()),
    }
}

/// The files' text before each journaled tool call of the running turn, by tool call id.
#[derive(Debug)]
pub struct EditSnapshots {
    cwd: PathBuf,
    before: DashMap<String, (PathBuf, Option<String>)>,
}

impl EditSnapshots {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            before: DashMap::new(),
        }
    }

    /// Remember the file `tool_name` is about to edit. Does nothing for other tools.
    pub async fn record(&self, tool_use_id: &str, tool_name: &str, input: &Value) {
        let Some(path) = edited_file(tool_name, input, &self.cwd) else {
            return;
        };
        let read = {
            let path = path.clone();
            tokio::task::spawn_blocking(move || read_text(&path)).await
        };
        match read {
            Ok(Ok(text)) => {
                self.before.insert(tool_use_id.to_owned(), (path, text));
            }
            Ok(Err(())) | Err(_) => tracing::warn!(
                tool_name,
                path = %path.display(),
                "File not journaled: too large or not text"
            ),
        }
    }

    /// The diff of the call's file, now that the tool has run. `None` when no snapshot was taken
    /// (not a journaled tool, or the file could not be read).
    pub fn diff(&self, tool_use_id: &str) -> Option<Diff> {
        let (_, (path, before)) = self.before.remove(tool_use_id)?;
        let after = read_text(&path).ok()??;
        Some(Diff::new(path, after).old_text(before))
    }

    /// Forget every snapshot: the turn is over.
    pub fn clear(&self) {
        self.before.clear();
    }
}

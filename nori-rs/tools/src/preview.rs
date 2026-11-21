//! The dry run of a tool call, for permission dialogs.

use std::path::PathBuf;

use crate::result::ChangeKind;

/// What kind of permission a call needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionAction {
    /// The call only reads.
    Read,
    /// The call creates, changes, moves or deletes files.
    Edit,
}

/// A path a call touches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewPath {
    /// Absolute and normalised.
    pub path: PathBuf,
    /// Outside the working directory and every configured extra root.
    pub external: bool,
}

/// One file the call would change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewChange {
    pub path: PathBuf,
    pub kind: ChangeKind,
    /// The file before; `None` when it does not exist yet.
    pub old_text: Option<String>,
    /// The file after; empty for a deletion.
    pub new_text: String,
}

/// What [`Workspace::preview`](crate::Workspace::preview) returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub action: PermissionAction,
    pub paths: Vec<PreviewPath>,
    /// Empty for a read, and for an edit that would change nothing.
    pub changes: Vec<PreviewChange>,
}

impl Preview {
    pub fn any_external(&self) -> bool {
        self.paths.iter().any(|path| path.external)
    }
}

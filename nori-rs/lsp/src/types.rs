//! Plain Nori types the request APIs return. Lines and columns are 1-based; a column counts
//! Unicode scalar values (what an editor shows), never UTF-16 units.

use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

/// A place in a file the caller names. `symbol` pins the column: agents are poor at counting
/// columns, so when `symbol` is given the position is the occurrence of it on `line` (or on the
/// nearest line that has one) and `column`, if present, only breaks ties.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub path: PathBuf,
    pub line: i32,
    pub column: Option<i32>,
    pub symbol: Option<String>,
}

impl Target {
    pub fn new(path: impl Into<PathBuf>, line: i32) -> Self {
        Self {
            path: path.into(),
            line,
            column: None,
            symbol: None,
        }
    }

    pub fn column(mut self, column: i32) -> Self {
        self.column = Some(column);
        self
    }

    pub fn symbol(mut self, symbol: impl Into<String>) -> Self {
        self.symbol = Some(symbol.into());
        self
    }
}

/// Where a [`Target`] landed after resolving `symbol` and clamping to the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedTarget {
    pub path: PathBuf,
    pub line: i32,
    pub column: i32,
    /// The word under the position, when there is one.
    pub word: String,
    /// True when the position differs from what the caller asked for.
    pub adjusted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub line: i32,
    pub column: i32,
    pub end_line: i32,
    pub end_column: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub path: PathBuf,
    pub range: Range,
    /// The source line at `range.line`, trimmed.
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hover {
    /// Markdown as the server wrote it.
    pub text: String,
    pub range: Option<Range>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    pub name: String,
    pub kind: String,
    pub detail: Option<String>,
    /// The enclosing symbol, for workspace symbols and flat document symbols.
    pub container: Option<String>,
    pub path: PathBuf,
    pub range: Range,
    pub children: Vec<Symbol>,
}

/// One text replacement, in file coordinates (1-based, end exclusive).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextEdit {
    pub range: Range,
    pub new_text: String,
}

/// The line a rename changes, before and after, for a preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineChange {
    pub line: i32,
    pub before: String,
    pub after: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEdit {
    pub path: PathBuf,
    pub edits: Vec<TextEdit>,
    pub preview: Vec<LineChange>,
}

/// A computed `WorkspaceEdit`. Nothing here has been applied.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EditPreview {
    pub files: Vec<FileEdit>,
    /// File creations, renames and deletions the server asked for, as sentences.
    pub resource_operations: Vec<String>,
}

impl EditPreview {
    pub fn edit_count(&self) -> i32 {
        let total: usize = self.files.iter().map(|file| file.edits.len()).sum();
        i32::try_from(total).unwrap_or(i32::MAX)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallDirection {
    Incoming,
    Outgoing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallItem {
    pub name: String,
    pub kind: String,
    pub detail: Option<String>,
    pub path: PathBuf,
    pub range: Range,
    pub snippet: String,
    /// Where the call happens (inside the caller for incoming, inside the root for outgoing).
    pub call_sites: Vec<Range>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeAction {
    pub title: String,
    pub kind: Option<String>,
    pub preferred: bool,
    /// Why the server says the action cannot run, if it does.
    pub disabled: Option<String>,
    pub edit: Option<EditPreview>,
    /// The server command the action would run, if it has no edit.
    pub command: Option<String>,
}

/// How the definition-like requests differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GotoKind {
    Definition,
    TypeDefinition,
    Implementation,
    Declaration,
}

impl GotoKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Definition => "definition",
            Self::TypeDefinition => "type definition",
            Self::Implementation => "implementation",
            Self::Declaration => "declaration",
        }
    }
}

/// A server of one root, as the status line and `lsp_diagnostics` see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerStatus {
    pub name: String,
    pub root: PathBuf,
    pub state: ServerState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ServerState {
    Starting,
    Ready {
        busy: bool,
    },
    /// Crashed or failed to start. `retry_in_seconds` is `None` once restarts are exhausted.
    Failed {
        reason: String,
        retry_in_seconds: Option<i64>,
    },
}

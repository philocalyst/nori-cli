//! Tool-related types for ACP notifications

use serde::Deserialize;
use serde::Serialize;

/// Tool kind for categorizing tools in UI
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    /// File read operations
    Read,
    /// File edit/write operations
    Edit,
    /// Command execution
    Execute,
    /// Search operations (grep, glob)
    Search,
    /// Network fetch operations
    Fetch,
    /// Thinking/planning operations
    Think,
    /// Mode switching operations
    SwitchMode,
    /// Other/unknown tool types
    #[default]
    Other,
}

/// Location information for tool calls
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallLocation {
    /// File path or location identifier
    pub path: String,

    /// Optional line number
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,

    /// Optional column number
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
}

impl ToolCallLocation {
    /// Create a new location with just a path
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            line: None,
            column: None,
        }
    }
}

/// Tool information for display in UI
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolInfo {
    /// Human-readable title for the tool call
    pub title: String,

    /// Tool kind/category
    pub kind: ToolKind,

    /// Content to display (e.g., command output, file content preview)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ToolInfoContent>,

    /// Locations affected by this tool call
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locations: Option<Vec<ToolCallLocation>>,
}

/// Content type for tool info display
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolInfoContent {
    /// Text content
    Text { text: String },
    /// Diff content
    Diff { diff: String },
    /// Terminal output
    Terminal { output: String },
}

impl ToolInfo {
    /// Create a new tool info
    pub fn new(title: impl Into<String>, kind: ToolKind) -> Self {
        Self {
            title: title.into(),
            kind,
            content: Vec::new(),
            locations: None,
        }
    }

    /// Add a location
    pub fn with_location(mut self, path: impl Into<String>) -> Self {
        self.locations
            .get_or_insert_with(Vec::new)
            .push(ToolCallLocation::new(path));
        self
    }
}

/// Cached tool use entry
///
/// Used to correlate tool_use blocks with their tool_result blocks.
#[derive(Debug, Clone)]
pub struct ToolUseEntry {
    /// Tool name
    pub name: String,

    /// Tool input parameters
    pub input: serde_json::Value,
}

impl ToolUseEntry {
    /// Create a new tool use entry
    pub fn new(name: String, input: serde_json::Value) -> Self {
        Self { name, input }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_kind_serialization() {
        assert_eq!(serde_json::to_string(&ToolKind::Read).unwrap(), "\"read\"");
        assert_eq!(
            serde_json::to_string(&ToolKind::Execute).unwrap(),
            "\"execute\""
        );
    }
}

//! Write tool: create or overwrite a file.
//!
//! The behaviour, description and schema are `nori_tools`' (shared with every other Nori agent
//! path); this adapter only maps the result onto the ACP tool result and its diff.

use async_trait::async_trait;

use super::base::Tool;
use crate::mcp::registry::ToolContext;
use crate::mcp::registry::ToolResult;

/// The Write tool, run by the session's `nori_tools` workspace
#[derive(Debug)]
pub struct WriteTool {
    spec: nori_tools::ToolSpec,
}

impl WriteTool {
    /// Create a new Write tool instance
    pub fn new() -> Self {
        Self {
            spec: nori_tools::ToolKind::Write.spec(),
        }
    }
}

impl Default for WriteTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "Write"
    }

    fn description(&self) -> &str {
        &self.spec.description
    }

    fn input_schema(&self) -> serde_json::Value {
        self.spec.input_schema.clone()
    }

    // Permission checking is handled at the SDK level: `mcp_message` calls the `can_use_tool`
    // callback before it executes an MCP tool.
    async fn execute(&self, input: serde_json::Value, context: &ToolContext) -> ToolResult {
        ToolResult::from_file_tool(context.workspace().call(self.spec.name, input).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tools::ReadTool;
    use serde_json::json;
    use tempfile::TempDir;

    #[tokio::test]
    async fn creates_a_file_and_its_parents_with_no_old_text() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("nested/dir/new_file.txt");

        let context = ToolContext::new("test", temp_dir.path());
        let result = WriteTool::new()
            .execute(
                json!({"file_path": file_path.to_str().unwrap(), "content": "Hello\n"}),
                &context,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "Hello\n");
        let diff = result.diff.expect("a reported diff");
        assert_eq!(diff.old_text, None);
        assert_eq!(diff.new_text, "Hello\n");
    }

    #[tokio::test]
    async fn overwriting_reports_what_was_replaced() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("existing.txt");
        std::fs::write(&file_path, "Old\n").unwrap();

        let context = ToolContext::new("test", temp_dir.path());
        let read = ReadTool::new()
            .execute(json!({"file_path": file_path.to_str().unwrap()}), &context)
            .await;
        assert!(!read.is_error, "{}", read.content);
        let result = WriteTool::new()
            .execute(
                json!({"file_path": file_path.to_str().unwrap(), "content": "New\n"}),
                &context,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        let diff = result.diff.expect("a reported diff");
        assert_eq!(diff.old_text.as_deref(), Some("Old\n"));
        assert_eq!(diff.new_text, "New\n");
    }

    #[tokio::test]
    async fn overwriting_an_unread_file_is_refused() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("existing.txt");
        std::fs::write(&file_path, "Old\n").unwrap();

        let context = ToolContext::new("test", temp_dir.path());
        let result = WriteTool::new()
            .execute(
                json!({"file_path": file_path.to_str().unwrap(), "content": "New\n"}),
                &context,
            )
            .await;
        assert!(result.is_error);
        assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "Old\n");
    }
}

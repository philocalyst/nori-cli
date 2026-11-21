//! Read tool: file contents with line numbers.
//!
//! The behaviour, description and schema are `nori_tools`' (shared with every other Nori agent
//! path); this adapter only maps the result onto the ACP tool result and its diff.

use async_trait::async_trait;

use super::base::Tool;
use crate::mcp::registry::ToolContext;
use crate::mcp::registry::ToolResult;

/// The Read tool, run by the session's `nori_tools` workspace
#[derive(Debug)]
pub struct ReadTool {
    spec: nori_tools::ToolSpec,
}

impl ReadTool {
    /// Create a new Read tool instance
    pub fn new() -> Self {
        Self {
            spec: nori_tools::ToolKind::Read.spec(),
        }
    }
}

impl Default for ReadTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "Read"
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
    use serde_json::json;
    use tempfile::TempDir;

    #[tokio::test]
    async fn reads_numbered_lines_with_offset_and_limit() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        let body: String = (1..=10).map(|i| format!("Line {i}\n")).collect();
        std::fs::write(&file_path, body).unwrap();

        let context = ToolContext::new("test", temp_dir.path());
        let result = ReadTool::new()
            .execute(
                json!({"file_path": file_path.to_str().unwrap(), "offset": 3, "limit": 2}),
                &context,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("Line 3"));
        assert!(result.content.contains("Line 4"));
        assert!(!result.content.contains("Line 5"));
        assert!(result.diff.is_none());
    }

    #[tokio::test]
    async fn image_read_preserves_attachment() {
        let temp_dir = TempDir::new().unwrap();
        std::fs::write(temp_dir.path().join("image.png"), [137, 80, 78, 71]).unwrap();
        let context = ToolContext::new("test", temp_dir.path());
        let result = ReadTool::new()
            .execute(json!({"file_path": "image.png"}), &context)
            .await;
        assert!(!result.is_error);
        assert_eq!(result.images.len(), 1);
        assert_eq!(result.images[0].mime_type, "image/png");
        assert_eq!(result.images[0].data, [137, 80, 78, 71]);
    }

    #[tokio::test]
    async fn missing_file_is_an_error() {
        let temp_dir = TempDir::new().unwrap();
        let context = ToolContext::new("test", temp_dir.path());
        let result = ReadTool::new()
            .execute(json!({"file_path": "/nonexistent/file.txt"}), &context)
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("not found"), "{}", result.content);
    }

    #[test]
    fn uses_the_shared_description_and_schema() {
        let tool = ReadTool::new();
        assert_eq!(tool.name(), "Read");
        assert_eq!(
            tool.input_schema(),
            nori_tools::ToolKind::Read.spec().input_schema
        );
        assert!(!tool.description().is_empty());
    }
}

//! Edit tool: exact string replacement in a file.
//!
//! The behaviour, description and schema are `nori_tools`' (shared with every other Nori agent
//! path); this adapter only maps the result onto the ACP tool result and its diff.

use async_trait::async_trait;

use super::base::Tool;
use crate::mcp::registry::ToolContext;
use crate::mcp::registry::ToolResult;

/// The Edit tool, run by the session's `nori_tools` workspace
#[derive(Debug)]
pub struct EditTool {
    spec: nori_tools::ToolSpec,
}

impl EditTool {
    /// Create a new Edit tool instance
    pub fn new() -> Self {
        Self {
            spec: nori_tools::ToolKind::Edit.spec(),
        }
    }
}

impl Default for EditTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "Edit"
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

    async fn read(context: &ToolContext, path: &std::path::Path) {
        let result = ReadTool::new()
            .execute(json!({"file_path": path.to_str().unwrap()}), context)
            .await;
        assert!(!result.is_error, "{}", result.content);
    }

    #[tokio::test]
    async fn edit_reports_the_change_as_a_diff() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        std::fs::write(&file_path, "Hello, World!\nGoodbye, World!\n").unwrap();

        let context = ToolContext::new("test", temp_dir.path());
        read(&context, &file_path).await;
        let result = EditTool::new()
            .execute(
                json!({
                    "file_path": file_path.to_str().unwrap(),
                    "old_string": "Hello",
                    "new_string": "Hi"
                }),
                &context,
            )
            .await;

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            std::fs::read_to_string(&file_path).unwrap(),
            "Hi, World!\nGoodbye, World!\n"
        );
        let diff = result.diff.expect("a reported diff");
        assert_eq!(diff.path, file_path);
        assert_eq!(
            diff.old_text.as_deref(),
            Some("Hello, World!\nGoodbye, World!\n")
        );
        assert_eq!(diff.new_text, "Hi, World!\nGoodbye, World!\n");
    }

    #[tokio::test]
    async fn replace_all_and_ambiguous_matches() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        std::fs::write(&file_path, "foo bar foo baz foo\n").unwrap();

        let context = ToolContext::new("test", temp_dir.path());
        read(&context, &file_path).await;
        let ambiguous = EditTool::new()
            .execute(
                json!({"file_path": file_path.to_str().unwrap(), "old_string": "foo", "new_string": "qux"}),
                &context,
            )
            .await;
        assert!(ambiguous.is_error);
        assert!(ambiguous.diff.is_none());
        assert_eq!(
            std::fs::read_to_string(&file_path).unwrap(),
            "foo bar foo baz foo\n"
        );

        let all = EditTool::new()
            .execute(
                json!({"file_path": file_path.to_str().unwrap(), "old_string": "foo", "new_string": "qux", "replace_all": true}),
                &context,
            )
            .await;
        assert!(!all.is_error, "{}", all.content);
        assert_eq!(
            std::fs::read_to_string(&file_path).unwrap(),
            "qux bar qux baz qux\n"
        );
    }

    #[tokio::test]
    async fn editing_a_file_that_was_never_read_is_refused() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        std::fs::write(&file_path, "Hello").unwrap();

        let context = ToolContext::new("test", temp_dir.path());
        let result = EditTool::new()
            .execute(
                json!({"file_path": file_path.to_str().unwrap(), "old_string": "Hello", "new_string": "Hi"}),
                &context,
            )
            .await;
        assert!(result.is_error);
        assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "Hello");
    }

    #[tokio::test]
    async fn string_not_found_is_an_error() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        std::fs::write(&file_path, "Hello, World!").unwrap();

        let context = ToolContext::new("test", temp_dir.path());
        read(&context, &file_path).await;
        let result = EditTool::new()
            .execute(
                json!({"file_path": file_path.to_str().unwrap(), "old_string": "Goodbye", "new_string": "Hi"}),
                &context,
            )
            .await;
        assert!(result.is_error);
    }
}

//! Write tool implementation
//!
//! Writes content to files on the filesystem.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::time::Instant;

use super::base::Tool;
use crate::mcp::registry::FileDiff;
use crate::mcp::registry::MAX_JOURNALED_BYTES;
use crate::mcp::registry::ToolContext;
use crate::mcp::registry::ToolResult;

/// Write tool for creating/overwriting files
#[derive(Debug, Default)]
pub struct WriteTool;

/// Write tool input parameters
#[derive(Debug, Deserialize)]
struct WriteInput {
    /// Path to the file to write
    file_path: String,
    /// Content to write to the file
    content: String,
}

impl WriteTool {
    /// Create a new Write tool instance
    pub fn new() -> Self {
        Self
    }

    /// Check permission before executing the tool
    ///
    /// Note: Permission checking is now handled at the SDK level.
    /// The SDK's `mcp_message` handler calls `can_use_tool` callback before executing MCP tools.
    /// This method is kept for potential future tool-specific permission logic.
    fn check_permission(
        &self,
        _input: &serde_json::Value,
        _context: &ToolContext,
    ) -> Option<ToolResult> {
        // Permission check is handled by SDK's can_use_tool callback
        None
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "Write"
    }

    fn description(&self) -> &str {
        "Write content to a file. Creates the file if it doesn't exist, or overwrites it if it does. Creates parent directories as needed."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["file_path", "content"],
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The absolute path to the file to write"
                },
                "content": {
                    "type": "string",
                    "description": "The content to write to the file"
                }
            }
        })
    }
    async fn execute(&self, input: serde_json::Value, context: &ToolContext) -> ToolResult {
        // Check permission before executing
        if let Some(result) = self.check_permission(&input, context) {
            return result;
        }

        // Parse input
        let params: WriteInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {e}")),
        };

        // Resolve path relative to working directory if not absolute
        let path = if std::path::Path::new(&params.file_path).is_absolute() {
            std::path::PathBuf::from(&params.file_path)
        } else {
            context.cwd.join(&params.file_path)
        };

        let total_start = Instant::now();

        // Create parent directories if they don't exist
        if let Some(parent) = path.parent()
            && !parent.exists()
        {
            let dir_start = Instant::now();
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return ToolResult::error(format!("Failed to create directory: {e}"));
            }
            tracing::debug!(
                parent_dir = %parent.display(),
                dir_creation_duration_ms = dir_start.elapsed().as_millis(),
                "Parent directories created"
            );
        }

        // Check if file exists (for reporting), and keep what it holds for the edit journal. A
        // file too large or not text is overwritten without a reported diff: the journal could
        // not restore it either.
        let file_existed = path.exists();
        let old_text = if file_existed {
            match tokio::fs::metadata(&path).await {
                Ok(metadata) if metadata.len() <= MAX_JOURNALED_BYTES => {
                    tokio::fs::read_to_string(&path).await.ok()
                }
                Ok(_) | Err(_) => None,
            }
        } else {
            None
        };

        // Write content to file
        let write_start = Instant::now();
        match tokio::fs::write(&path, &params.content).await {
            Ok(()) => {
                let write_duration = write_start.elapsed();
                let total_elapsed = total_start.elapsed();

                let action = if file_existed { "Updated" } else { "Created" };
                let lines = params.content.lines().count();
                let bytes = params.content.len();

                tracing::info!(
                    file_path = %path.display(),
                    action = %action,
                    lines = lines,
                    bytes = bytes,
                    write_duration_ms = write_duration.as_millis(),
                    total_elapsed_ms = total_elapsed.as_millis(),
                    "File write successful"
                );

                let mut result = ToolResult::success(format!(
                    "{} {} ({} lines, {} bytes)",
                    action,
                    path.display(),
                    lines,
                    bytes
                ));
                if !file_existed || old_text.is_some() {
                    result = result.with_diff(FileDiff {
                        path: path.clone(),
                        old_text,
                        new_text: params.content.clone(),
                    });
                }
                result.with_metadata(json!({
                    "path": path.display().to_string(),
                    "created": !file_existed,
                    "lines": lines,
                    "bytes": bytes,
                    "write_duration_ms": write_duration.as_millis(),
                    "total_elapsed_ms": total_elapsed.as_millis()
                }))
            }
            Err(e) => {
                let elapsed = total_start.elapsed();
                tracing::error!(
                    file_path = %path.display(),
                    error = %e,
                    elapsed_ms = elapsed.as_millis(),
                    "File write failed"
                );
                ToolResult::error(format!("Failed to write file: {e}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_write_new_file() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("new_file.txt");

        let tool = WriteTool::new();
        let context = ToolContext::new("test", temp_dir.path());

        let result = tool
            .execute(
                json!({
                    "file_path": file_path.to_str().unwrap(),
                    "content": "Hello, World!"
                }),
                &context,
            )
            .await;

        assert!(!result.is_error);
        assert!(result.content.contains("Created"));

        // Verify file was created
        let content = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "Hello, World!");
    }

    #[tokio::test]
    async fn test_write_overwrite_file() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("existing.txt");

        // Create existing file
        std::fs::write(&file_path, "Original content").unwrap();

        let tool = WriteTool::new();
        let context = ToolContext::new("test", temp_dir.path());

        let result = tool
            .execute(
                json!({
                    "file_path": file_path.to_str().unwrap(),
                    "content": "New content"
                }),
                &context,
            )
            .await;

        assert!(!result.is_error);
        assert!(result.content.contains("Updated"));

        // Verify content was replaced
        let content = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "New content");
    }

    #[tokio::test]
    async fn test_write_creates_directories() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("nested/dir/file.txt");

        let tool = WriteTool::new();
        let context = ToolContext::new("test", temp_dir.path());

        let result = tool
            .execute(
                json!({
                    "file_path": file_path.to_str().unwrap(),
                    "content": "Nested content"
                }),
                &context,
            )
            .await;

        assert!(!result.is_error);
        assert!(file_path.exists());
    }
}

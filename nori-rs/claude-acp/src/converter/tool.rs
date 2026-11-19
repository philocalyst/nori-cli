//! Tool information extraction from tool calls
//!
//! Extracts user-friendly information from tool calls for UI display.

use crate::types::ToolInfo;
use crate::types::ToolKind;
use std::path::Path;
use std::path::PathBuf;

/// ACP tool name prefix for SDK MCP server tools
const ACP_TOOL_PREFIX: &str = "mcp__acp__";

/// Maximum path length for display before truncation
/// Paths longer than this will be truncated to show only the filename
const MAX_DISPLAY_LENGTH: usize = 60;

/// Strip the ACP prefix from a tool name if present
fn strip_acp_prefix(name: &str) -> &str {
    name.strip_prefix(ACP_TOOL_PREFIX).unwrap_or(name)
}

/// Clean a path string for display
///
/// Removes common path artifacts like:
/// - Duplicate slashes (e.g., `src//file.rs` → `src/file.rs`)
/// - Redundant `././` sequences (e.g., `././file` → `./file`)
/// - On Windows: also handles backslashes
///
/// Note: This function does NOT strip leading `./` prefix as that may be
/// intentionally added by `truncate_path()` for display purposes.
///
/// # Arguments
///
/// * `path` - The path string to clean
///
/// # Returns
///
/// A cleaned path string
fn clean_path(path: &str) -> String {
    // First, handle redundant ././ sequences
    let without_redundant_dot_slash = path.replace("././", "./");

    // Single-pass algorithm to remove duplicate slashes
    // This is more efficient than using a while loop with replace()
    let mut result = String::with_capacity(without_redundant_dot_slash.len());
    let mut prev_was_slash = false;

    #[cfg(unix)]
    {
        for ch in without_redundant_dot_slash.chars() {
            if ch == '/' {
                if !prev_was_slash {
                    result.push(ch);
                    prev_was_slash = true;
                }
                // Skip duplicate slashes
            } else {
                result.push(ch);
                prev_was_slash = false;
            }
        }
    }
    #[cfg(windows)]
    {
        for ch in without_redundant_dot_slash.chars() {
            if ch == '/' || ch == '\\' {
                if !prev_was_slash {
                    result.push('/');
                    prev_was_slash = true;
                }
                // Skip duplicate slashes (both forward and backslash)
            } else {
                result.push(ch);
                prev_was_slash = false;
            }
        }
    }

    result
}

/// Extract tool information from a tool name and input
///
/// This creates a `ToolInfo` struct with a human-readable title,
/// tool kind classification, and relevant locations (e.g., file paths).
///
/// # Arguments
///
/// * `name` - The tool name (e.g., "Read", "Bash", "Edit", "mcp__acp__Bash")
/// * `input` - The tool input parameters as JSON
/// * `cwd` - Optional current working directory for computing relative paths
///
/// # Returns
///
/// A `ToolInfo` with populated fields for UI display
pub fn extract_tool_info(name: &str, input: &serde_json::Value, cwd: Option<&PathBuf>) -> ToolInfo {
    // Convert Option<&PathBuf> to Option<&Path> for easier use
    let cwd_path = cwd.map(std::path::PathBuf::as_path);
    // Strip mcp__acp__ prefix for ACP tools to use the same display logic
    let effective_name = strip_acp_prefix(name);

    match effective_name {
        "Read" => {
            let path = input
                .get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or("file");

            // Get offset and limit for line range display
            // TypeScript uses 1-indexed display: "Read path (1 - 50)"
            // Reference: vendors/claude-code-acp/src/tools.ts:207-227
            let offset = input.get("offset").and_then(serde_json::Value::as_u64);
            let limit = input.get("limit").and_then(serde_json::Value::as_u64);

            let title = if let (Some(start), Some(count)) = (offset, limit) {
                if count == 0 {
                    // Edge case: limit=0 means "from this line onward"
                    // Use 1-indexed display
                    let display_start = start.saturating_add(1);
                    format!(
                        "Read {} (from line {})",
                        truncate_path(path, cwd_path),
                        display_start
                    )
                } else {
                    // Show line range: "Read file.rs (1 - 50)"
                    // TypeScript uses 1-indexed format: (start+1 - start+count)
                    // Use saturating arithmetic to prevent overflow
                    let display_start = start.saturating_add(1);
                    let display_end = start.saturating_add(count);
                    format!(
                        "Read {} ({} - {})",
                        truncate_path(path, cwd_path),
                        display_start,
                        display_end
                    )
                }
            } else if let Some(start) = offset {
                // Show start line: "Read file.rs (from line 101)"
                // Use 1-indexed display
                let display_start = start.saturating_add(1);
                format!(
                    "Read {} (from line {})",
                    truncate_path(path, cwd_path),
                    display_start
                )
            } else {
                // Just show path: "Read file.rs"
                format!("Read {}", truncate_path(path, cwd_path))
            };

            ToolInfo::new(title, ToolKind::Read).with_location(path)
        }

        "Edit" => {
            let path = input
                .get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or("file");
            let title = format!("Edit {}", truncate_path(path, cwd_path));
            ToolInfo::new(title, ToolKind::Edit).with_location(path)
        }

        "Write" => {
            let path = input
                .get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or("file");
            let title = format!("Write {}", truncate_path(path, cwd_path));
            ToolInfo::new(title, ToolKind::Edit).with_location(path)
        }

        "Bash" => {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");

            // Reference: vendors/claude-code-acp/src/tools.ts:97-111
            // TypeScript always uses the command as title (wrapped in backticks)
            // The description goes into content[], not title
            let title = if cmd.is_empty() {
                "Terminal".to_string()
            } else {
                // Escape backticks in command to avoid breaking markdown
                let escaped_cmd = cmd.replace('`', "\\`");
                format!("`{}`", truncate_string(&escaped_cmd, 50))
            };

            ToolInfo::new(title, ToolKind::Execute)
        }

        "Grep" => {
            let pattern = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let title = format!("Search: {}", truncate_string(pattern, 40));
            ToolInfo::new(title, ToolKind::Search)
        }

        "Glob" => {
            let pattern = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            // For Glob patterns, strip ./ prefix and clean up slashes
            // Glob patterns don't need ./ prefix for display
            let without_dot_slash = pattern.strip_prefix("./").unwrap_or(pattern);
            let clean_pattern = clean_path(without_dot_slash);
            // Wrap in backticks to prevent markdown rendering of ** as bold
            let title = format!("Find: `{}`", truncate_string(&clean_pattern, 40));
            ToolInfo::new(title, ToolKind::Search)
        }

        "LS" => {
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            // Reference: vendors/claude-code-acp/src/tools.ts:241
            // TypeScript uses: "List the 'path' directory's contents"
            let title = format!(
                "List the '{}' directory's contents",
                truncate_path(path, cwd_path)
            );
            ToolInfo::new(title, ToolKind::Search)
        }

        "BashOutput" => {
            // Reference: vendors/claude-code-acp/src/tools.ts:344
            ToolInfo::new("Tail Logs", ToolKind::Execute)
        }

        "KillShell" => {
            // Reference: vendors/claude-code-acp/src/tools.ts:347
            ToolInfo::new("Kill Process", ToolKind::Execute)
        }

        "WebFetch" => {
            let url = input.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let title = format!("Fetch {}", truncate_string(url, 50));
            ToolInfo::new(title, ToolKind::Fetch)
        }

        "WebSearch" => {
            let query = input.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let title = format!("Search: {}", truncate_string(query, 40));
            ToolInfo::new(title, ToolKind::Fetch)
        }

        "Task" => {
            let desc = input
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("Task");
            ToolInfo::new(desc, ToolKind::Think)
        }

        "TodoWrite" => ToolInfo::new("Update task list", ToolKind::Think),

        "EnterPlanMode" | "ExitPlanMode" => {
            ToolInfo::new(effective_name.to_string(), ToolKind::SwitchMode)
        }

        "AskUserQuestion" => ToolInfo::new("Ask question", ToolKind::Other),

        "SlashCommand" => {
            let command = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            let title = if command.is_empty() {
                "Slash command".to_string()
            } else {
                format!("/{command}")
            };
            ToolInfo::new(title, ToolKind::Other)
        }

        "Skill" => {
            let skill = input.get("skill").and_then(|v| v.as_str()).unwrap_or("");
            let title = if skill.is_empty() {
                "Skill".to_string()
            } else {
                format!("Skill: {skill}")
            };
            ToolInfo::new(title, ToolKind::Other)
        }

        "NotebookRead" | "NotebookEdit" => {
            let path = input
                .get("notebook_path")
                .and_then(|v| v.as_str())
                .unwrap_or("notebook");
            let title = format!("{} {}", effective_name, truncate_path(path, cwd_path));
            let kind = if effective_name == "NotebookRead" {
                ToolKind::Read
            } else {
                ToolKind::Edit
            };
            ToolInfo::new(title, kind).with_location(path)
        }

        // Claude Code loading deferred tools: a search, as far as the user is concerned
        "ToolSearch" => ToolInfo::new("ToolSearch".to_string(), ToolKind::Search),

        // Nori's own tools, named as Nori names them
        name if crate::mcp::host_tool_name(name).is_some() => ToolInfo::new(
            crate::mcp::host_tool_name(name).unwrap_or(name).to_string(),
            ToolKind::Other,
        ),

        // MCP tools (format: mcp__server__tool)
        // Note: mcp__acp__* tools are already handled above via strip_acp_prefix
        name if name.starts_with("mcp__") && !name.starts_with(ACP_TOOL_PREFIX) => {
            let parts: Vec<&str> = name.split("__").collect();
            let tool_name = parts.get(2).unwrap_or(&name);
            ToolInfo::new(format!("MCP: {tool_name}"), ToolKind::Other)
        }

        // Default case
        _ => ToolInfo::new(effective_name.to_string(), ToolKind::Other),
    }
}

/// Truncate a file path for display
///
/// # Arguments
///
/// * `path` - The file path to truncate
/// * `cwd` - Optional current working directory for computing relative paths
///
/// # Returns
///
/// A truncated or relative path for display
fn truncate_path(path: &str, cwd: Option<&Path>) -> String {
    let path_obj = std::path::Path::new(path);

    // Try to compute relative path if cwd is provided
    let display_path = if let Some(cwd_path) = cwd {
        if path_obj.is_absolute() {
            // Try to make the path relative to cwd
            match path_obj.strip_prefix(cwd_path) {
                Ok(rel) if !rel.as_os_str().is_empty() => {
                    // Use Path API to count components (cross-platform)
                    let component_count = rel.iter().count();
                    let rel_str = rel.to_string_lossy();
                    // Add ./ prefix for files directly in cwd (single component)
                    if component_count == 1 {
                        format!("./{rel_str}")
                    } else {
                        rel_str.to_string()
                    }
                }
                _ => path.to_string(), // Keep original if strip_prefix fails
            }
        } else {
            // Path is already relative, keep it as-is
            path.to_string()
        }
    } else {
        // No cwd provided, keep original path
        path.to_string()
    };

    // Use clean_path() to normalize slashes (handles multiple duplicates)
    // This handles cases like "/src//*.rs" -> "/src/*.rs" and "a////b" -> "a/b"
    let normalized = clean_path(&display_path);

    // Truncate if still too long
    if normalized.len() > MAX_DISPLAY_LENGTH {
        std::path::Path::new(&normalized)
            .file_name()
            .and_then(|n| n.to_str())
            .map(String::from)
            .unwrap_or_else(|| truncate_string(&normalized, MAX_DISPLAY_LENGTH))
    } else {
        normalized
    }
}

/// Truncate a string to a maximum length
fn truncate_string(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len.saturating_sub(3)])
    }
}

#[cfg(test)]
mod tests;

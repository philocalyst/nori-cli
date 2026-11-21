//! Listing the sessions Claude Code has saved on disk.

use nori_acp_kit::schema::ListSessionsRequest;
use nori_acp_kit::schema::ListSessionsResponse;
use nori_acp_kit::schema::SessionInfo;

use crate::types::AgentConfig;
use crate::types::AgentError;
use crate::types::Result;

/// Handle session/list request
///
/// Lists the sessions saved as JSONL files in the login's `projects/` directory (an account's
/// `CLAUDE_CONFIG_DIR`, else `~/.claude`), newest first, a page at a time, optionally only those
/// of one working directory. The directory is read on a blocking thread: a login with years of
/// sessions must not hold up the connection.
/// Reference: TS `unstable_listSessions` in acp-agent.ts
pub(super) async fn list_sessions(
    request: ListSessionsRequest,
    config: &AgentConfig,
) -> Result<ListSessionsResponse> {
    let config_dir = crate::utils::claude_config_dir(&config.environment)
        .ok_or_else(|| AgentError::Internal("No home directory found".to_string()))?;
    tokio::task::spawn_blocking(move || list_sessions_in(request, &config_dir.join("projects")))
        .await
        .map_err(AgentError::internal)?
}

fn list_sessions_in(
    request: ListSessionsRequest,
    claude_dir: &std::path::Path,
) -> Result<ListSessionsResponse> {
    const PAGE_SIZE: usize = 50;

    tracing::info!(
        claude_dir = ?claude_dir,
        cwd_filter = ?request.cwd,
        cursor = ?request.cursor,
        "Listing sessions"
    );

    // If the projects directory doesn't exist, return empty
    if !claude_dir.exists() {
        return Ok(ListSessionsResponse::new(vec![]));
    }

    let cwd_filter_str = request
        .cwd
        .as_ref()
        .map(|p| p.to_string_lossy().to_string());
    let encoded_cwd_filter = request.cwd.as_ref().map(|cwd| encode_project_path(cwd));

    let mut all_sessions: Vec<SessionInfo> = Vec::new();

    // Read project directories
    let project_dirs = match std::fs::read_dir(claude_dir) {
        Ok(dirs) => dirs,
        Err(e) => {
            tracing::error!(error = %e, "Failed to read projects directory");
            return Ok(ListSessionsResponse::new(vec![]));
        }
    };

    for entry in project_dirs.flatten() {
        let project_dir = entry.path();
        if !project_dir.is_dir() {
            continue;
        }

        let encoded_path = entry.file_name().to_string_lossy().to_string();

        // Coarse pre-filter by encoded path
        if let Some(ref filter) = encoded_cwd_filter
            && &encoded_path != filter
        {
            continue;
        }

        // Read JSONL files in this project directory
        let Ok(files) = std::fs::read_dir(&project_dir) else {
            continue;
        };

        for file_entry in files.flatten() {
            let file_path = file_entry.path();
            let file_name = file_entry.file_name().to_string_lossy().to_string();

            // Skip non-JSONL files and agent-* files (internal metadata)
            if !std::path::Path::new(&file_name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
                || file_name.starts_with("agent-")
            {
                continue;
            }

            let session_id = file_name.trim_end_matches(".jsonl").to_string();

            match parse_session_file(&file_path, &session_id, cwd_filter_str.as_deref()) {
                Ok(Some(info)) => all_sessions.push(info),
                Ok(None) => {} // Filtered out
                Err(e) => {
                    tracing::warn!(
                        file = ?file_path,
                        error = %e,
                        "Failed to parse session file"
                    );
                }
            }
        }
    }

    // Sort by updatedAt descending (most recent first)
    all_sessions.sort_by(|a, b| {
        let time_a = a.updated_at.as_deref().unwrap_or("");
        let time_b = b.updated_at.as_deref().unwrap_or("");
        time_b.cmp(time_a)
    });

    // Handle pagination with cursor
    let start_index = if let Some(ref cursor) = request.cursor {
        parse_cursor(cursor).unwrap_or(0)
    } else {
        0
    };

    let total = all_sessions.len();
    let page = all_sessions
        .into_iter()
        .skip(start_index)
        .take(PAGE_SIZE)
        .collect::<Vec<_>>();

    let has_more = start_index + PAGE_SIZE < total;

    let mut response = ListSessionsResponse::new(page);

    // Note: ListSessionsResponse has next_cursor field behind the feature flag
    // We check if there are more results and set the cursor
    if has_more {
        let next_offset = start_index + PAGE_SIZE;
        let cursor_json = serde_json::json!({"offset": next_offset});
        let cursor_str = base64_encode(&cursor_json.to_string());
        response.next_cursor = Some(cursor_str);
    }

    Ok(response)
}

/// Parse a session JSONL file to extract session info
///
/// Reads the file and extracts:
/// - cwd from any entry
/// - title from the first user message
/// - updatedAt from file modification time
fn parse_session_file(
    file_path: &std::path::Path,
    session_id: &str,
    cwd_filter: Option<&str>,
) -> std::result::Result<Option<SessionInfo>, std::io::Error> {
    use std::io::BufRead;

    let file = std::fs::File::open(file_path)?;
    let reader = std::io::BufReader::new(file);

    let mut session_cwd: Option<String> = None;
    let mut title: Option<String> = None;
    let mut parsed_any = false;

    for line in reader.lines() {
        let line = match line {
            Ok(l) if !l.is_empty() => l,
            _ => continue,
        };

        let entry: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        parsed_any = true;

        // Skip sidechain entries
        if entry.get("isSidechain") == Some(&serde_json::Value::Bool(true)) {
            continue;
        }

        // Extract cwd
        if session_cwd.is_none()
            && let Some(cwd) = entry.get("cwd").and_then(|v| v.as_str())
        {
            session_cwd = Some(cwd.to_string());
        }

        // Extract title from first user message
        if title.is_none()
            && entry.get("type").and_then(|v| v.as_str()) == Some("user")
            && let Some(content) = entry.get("message").and_then(|m| m.get("content"))
        {
            title = extract_title_from_content(content);
        }

        // Stop if we have both
        if title.is_some() && session_cwd.is_some() {
            break;
        }
    }

    if !parsed_any {
        return Ok(None);
    }

    // SessionInfo.cwd is required
    let Some(cwd) = session_cwd else {
        return Ok(None);
    };

    // Verify cwd matches filter if provided
    if let Some(filter) = cwd_filter
        && cwd != filter
    {
        return Ok(None);
    }

    // Get file modification time
    let updated_at = std::fs::metadata(file_path)
        .ok()
        .and_then(|m| m.modified().ok())
        .map(|t| {
            let datetime: chrono::DateTime<chrono::Utc> = t.into();
            datetime.to_rfc3339()
        });

    let mut info = SessionInfo::new(session_id.to_string(), &cwd);
    info.title = title;
    info.updated_at = updated_at;

    Ok(Some(info))
}

/// Extract title from message content
fn extract_title_from_content(content: &serde_json::Value) -> Option<String> {
    let text = if let Some(s) = content.as_str() {
        Some(s.to_string())
    } else if let Some(arr) = content.as_array() {
        arr.first().and_then(|first| {
            if let Some(s) = first.as_str() {
                Some(s.to_string())
            } else {
                first.get("text").and_then(|t| t.as_str()).map(String::from)
            }
        })
    } else {
        None
    };

    text.map(|t| sanitize_title(&t))
}

/// Sanitize a title string for display
fn sanitize_title(text: &str) -> String {
    // Truncate to reasonable length and trim whitespace
    let truncated: String = text.chars().take(200).collect();
    // Remove newlines and excessive whitespace
    truncated.lines().next().unwrap_or("").trim().to_string()
}

/// Encode a project path for use as directory name
///
/// Replaces path separators with hyphens.
/// Reference: TS `encodeProjectPath` in utils.ts
fn encode_project_path(cwd: &std::path::Path) -> String {
    let path_str = cwd.to_string_lossy();
    // Unix paths: replace / with -
    path_str.replace('/', "-")
}

/// Parse a pagination cursor
fn parse_cursor(cursor: &str) -> Option<usize> {
    let decoded = base64_decode(cursor)?;
    let parsed: serde_json::Value = serde_json::from_str(&decoded).ok()?;
    parsed
        .get("offset")?
        .as_u64()
        .and_then(|v| usize::try_from(v).ok())
}

/// Base64 encode a string
fn base64_encode(s: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
}

/// Base64 decode a string
fn base64_decode(s: &str) -> Option<String> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(s).ok()?;
    String::from_utf8(bytes).ok()
}

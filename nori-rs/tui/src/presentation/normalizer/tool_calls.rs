//! Reading a tool call: which kind it is, what it invoked (a command, a read, a search, file
//! changes) across the shapes different agents send, and what it produced.

use std::path::PathBuf;

use nori_protocol::acp::v1 as acp;

use super::super::*;

pub(super) fn is_generic_tool_call(tool_call: &acp::ToolCall) -> bool {
    // A bare call whose title is just the kind of tool ("Read", "Tool") is a placeholder that
    // the agent refines with an update, and is not shown until it is. A bare call that says
    // what it does ("Compact conversation") is shown where it happened: held back, it would
    // appear after the text that followed it.
    const KIND_WORDS: [&str; 16] = [
        "", "tool", "read", "edit", "write", "execute", "bash", "search", "grep", "glob", "list",
        "fetch", "think", "other", "task", "terminal",
    ];
    let title = tool_call.title.trim().to_ascii_lowercase();
    nori_invocation(tool_call).is_none()
        && tool_call.raw_input.is_none()
        && tool_call.locations.is_empty()
        && tool_call.content.is_empty()
        && !tool_call.title.contains('/')
        && KIND_WORDS.contains(&title.as_str())
}

/// Some ACP agents (e.g. Codex) send `kind: "edit"` for all file mutations.
/// The actual operation type is in `rawInput.changes.{path}.type`:
///   - `"add"` → file creation
///   - `"delete"` → file deletion
///   - `"update"` with non-null `move_path` → file rename/move
///   - `"update"` with null/absent `move_path` → normal edit (no refinement)
pub(super) fn refine_edit_kind(kind: ToolKind, raw_input: &Option<serde_json::Value>) -> ToolKind {
    if kind != ToolKind::Edit {
        return kind;
    }
    let Some(input) = raw_input else { return kind };
    let Some(changes) = input.get("changes").and_then(|c| c.as_object()) else {
        return kind;
    };
    for (_path, change) in changes {
        if let Some(change_type) = change.get("type").and_then(|t| t.as_str()) {
            match change_type {
                "add" => return ToolKind::Create,
                "delete" => return ToolKind::Delete,
                _ => {
                    if change
                        .get("move_path")
                        .and_then(|v| v.as_str())
                        .is_some_and(|s| !s.is_empty())
                    {
                        return ToolKind::Move;
                    }
                }
            }
        }
    }
    kind
}

pub(super) fn tool_snapshot_from_tool_call(
    tool_call: &acp::ToolCall,
    phase: ToolPhase,
) -> ToolSnapshot {
    let artifacts = artifacts_from_tool_call(tool_call);
    let invocation =
        nori_invocation(tool_call).or_else(|| invocation_from_tool_call(tool_call, &artifacts));

    ToolSnapshot {
        call_id: tool_call.tool_call_id.to_string(),
        title: sanitize_title(&tool_call.title),
        kind: refine_edit_kind(ToolKind::from_acp(tool_call.kind), &tool_call.raw_input),
        phase,
        locations: tool_call
            .locations
            .iter()
            .map(|location| ToolLocation {
                path: location.path.clone(),
                line: location.line,
            })
            .collect(),
        invocation,
        artifacts,
        raw_input: tool_call.raw_input.clone(),
        raw_output: tool_call.raw_output.clone(),
        owner_request_id: None,
    }
}

/// A call to one of Nori's own tools: named in its `_meta` by an agent built into Nori, or
/// recognized from its title when it came through the `nori-client` MCP server.
pub(super) fn nori_invocation(tool_call: &acp::ToolCall) -> Option<Invocation> {
    let input = tool_call.raw_input.as_ref();
    if let Some(tool) = nori_host_tools::host_tool_of(tool_call.meta.as_ref()) {
        return DelegationCall::from_tool(tool, input)
            .map(Invocation::Delegation)
            .or_else(|| NativeCall::from_tool(tool, input).map(Invocation::Native));
    }
    DelegationCall::recognize(&tool_call.title, input)
        .map(Invocation::Delegation)
        .or_else(|| NativeCall::recognize(&tool_call.title, input).map(Invocation::Native))
}

pub(super) fn invocation_from_tool_call(
    tool_call: &acp::ToolCall,
    artifacts: &[Artifact],
) -> Option<Invocation> {
    let diff_changes: Vec<FileChange> = artifacts
        .iter()
        .filter_map(|artifact| match artifact {
            Artifact::Diff(change) => Some(change.clone()),
            Artifact::Text { .. } => None,
        })
        .collect();

    if !diff_changes.is_empty() {
        return Some(Invocation::FileChanges {
            changes: diff_changes,
        });
    }

    if let Some(invocation) = structured_invocation_from_tool_call(tool_call) {
        return Some(invocation);
    }

    if let Some(json) = &tool_call.raw_input {
        return Some(Invocation::RawJson(json.clone()));
    }

    location_fallback_invocation(tool_call)
}

pub(super) fn location_fallback_invocation(tool_call: &acp::ToolCall) -> Option<Invocation> {
    let location = tool_call.locations.first()?;
    let path = location.path.clone();

    match tool_call.kind {
        acp::ToolKind::Read => Some(Invocation::Read { path }),
        acp::ToolKind::Search => Some(Invocation::Search {
            query: None,
            path: Some(path),
        }),
        // Edit/Delete/Move require more context (old_text/new_text) than a bare
        // location provides. They fall through to the TUI's location-path fallback.
        _ => None,
    }
}

pub(super) fn sanitize_title(title: &str) -> String {
    const CWD_MARKER: &str = " [current working directory ";

    let Some(cwd_start) = title.find(CWD_MARKER) else {
        return title.to_string();
    };

    let before_cwd = &title[..cwd_start];
    let after_marker = cwd_start + CWD_MARKER.len();

    let remainder = if let Some(bracket_end) = title[after_marker..].find(']') {
        let after_bracket = after_marker + bracket_end + 1;
        title[after_bracket..].trim()
    } else {
        return before_cwd.trim().to_string();
    };

    if remainder.is_empty() {
        return before_cwd.trim().to_string();
    }

    // Strip trailing (description text) from the remainder — Gemini appends these
    // after the cwd bracket. Apply stripping to the remainder only, not the
    // reconstituted string, to avoid removing parentheses in the command itself.
    let remainder = if remainder.starts_with('(') && remainder.ends_with(')') {
        ""
    } else if let Some(paren_start) = remainder.rfind(" (")
        && remainder.ends_with(')')
    {
        remainder[..paren_start].trim()
    } else {
        remainder
    };

    if remainder.is_empty() {
        before_cwd.trim().to_string()
    } else {
        format!("{} {remainder}", before_cwd.trim())
    }
}

pub(super) fn artifacts_from_tool_call(tool_call: &acp::ToolCall) -> Vec<Artifact> {
    let mut artifacts = tool_call
        .content
        .iter()
        .filter_map(|content| match content {
            acp::ToolCallContent::Diff(diff) => Some(Artifact::Diff(FileChange {
                path: diff.path.clone(),
                old_text: diff.old_text.clone(),
                new_text: diff.new_text.clone(),
            })),
            acp::ToolCallContent::Content(content) => match &content.content {
                acp::ContentBlock::Text(text) if !text.text.is_empty() => Some(Artifact::Text {
                    text: text.text.clone(),
                    format: TextFormat::Plain,
                }),
                acp::ContentBlock::Resource(resource) => match &resource.resource {
                    acp::EmbeddedResourceResource::TextResourceContents(contents)
                        if !contents.text.is_empty() =>
                    {
                        Some(Artifact::Text {
                            text: contents.text.clone(),
                            format: TextFormat::of_resource(
                                contents.mime_type.as_deref(),
                                &contents.uri,
                            ),
                        })
                    }
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();

    if !artifacts
        .iter()
        .any(|artifact| matches!(artifact, Artifact::Text { .. }))
        && let Some(text) = raw_output_text(tool_call.raw_output.as_ref())
    {
        artifacts.push(Artifact::Text {
            text,
            format: TextFormat::Plain,
        });
    }

    artifacts
}

pub(super) fn structured_invocation_from_tool_call(
    tool_call: &acp::ToolCall,
) -> Option<Invocation> {
    let raw_input = tool_call.raw_input.as_ref()?;
    match tool_call.kind {
        acp::ToolKind::Edit => file_operations_from_edit_input(raw_input)
            .map(|operations| Invocation::FileOperations { operations }),
        acp::ToolKind::Delete => file_operation_from_delete_input(raw_input).map(|operation| {
            Invocation::FileOperations {
                operations: vec![operation],
            }
        }),
        acp::ToolKind::Move => {
            file_operation_from_move_input(raw_input).map(|operation| Invocation::FileOperations {
                operations: vec![operation],
            })
        }
        acp::ToolKind::Execute => {
            extract_command(raw_input).map(|command| Invocation::Command { command })
        }
        acp::ToolKind::Read => {
            let path = extract_path(raw_input, &["path", "file_path", "file"]).or_else(|| {
                parsed_command_path_for(raw_input, |parsed_command| {
                    matches!(parsed_command_type_value(parsed_command), Some("read"))
                })
            })?;
            Some(Invocation::Read { path })
        }
        acp::ToolKind::Search => {
            let mut path = raw_input
                .get("path")
                .or_else(|| raw_input.get("directory"))
                .and_then(serde_json::Value::as_str)
                .map(PathBuf::from);
            let mut query = raw_input
                .get("pattern")
                .or_else(|| raw_input.get("query"))
                .or_else(|| raw_input.get("glob"))
                .and_then(serde_json::Value::as_str)
                .map(|query| match query.strip_prefix("select:") {
                    // An agent loading tools by name (`select:mcp__server__tool,...`): the names.
                    Some(names) => names
                        .split(',')
                        .map(|name| name.trim().rsplit("__").next().unwrap_or(name))
                        .collect::<Vec<_>>()
                        .join(", "),
                    None => query.to_string(),
                });

            if path.is_none()
                && (parsed_command_is_listing(raw_input) || parsed_command_is_search(raw_input))
            {
                path = parsed_command_path_for(raw_input, |parsed_command| {
                    matches!(
                        parsed_command_type_value(parsed_command),
                        Some("list_files")
                    ) || parsed_command_type_value(parsed_command).is_some_and(|type_| {
                        type_.contains("search")
                            || type_.contains("grep")
                            || type_.contains("rg")
                            || type_.contains("glob")
                    })
                });
            }

            if query.is_none() && parsed_command_is_search(raw_input) {
                query = parsed_command_query_for_search(raw_input);
            }

            if parsed_command_is_listing(raw_input)
                || (query.is_none() && title_looks_like_listing(&tool_call.title))
            {
                Some(Invocation::ListFiles { path })
            } else {
                Some(Invocation::Search { query, path })
            }
        }
        acp::ToolKind::Fetch | acp::ToolKind::Think | acp::ToolKind::Other => {
            Some(Invocation::Tool {
                tool_name: tool_call.title.clone(),
                input: Some(raw_input.clone()),
            })
        }
        _ => None,
    }
}

pub(super) fn file_operations_from_edit_input(
    raw_input: &serde_json::Value,
) -> Option<Vec<FileOperation>> {
    let path = extract_path(raw_input, &["file_path", "path", "file"])?;

    if let Some(new_text) = raw_input.get("content").and_then(serde_json::Value::as_str) {
        return Some(vec![FileOperation::Create {
            path,
            new_text: new_text.to_string(),
        }]);
    }

    let old_text = raw_input
        .get("old_string")
        .and_then(serde_json::Value::as_str)?;
    let new_text = raw_input
        .get("new_string")
        .and_then(serde_json::Value::as_str)?;
    Some(vec![FileOperation::Update {
        path,
        old_text: old_text.to_string(),
        new_text: new_text.to_string(),
    }])
}

pub(super) fn file_operation_from_delete_input(
    raw_input: &serde_json::Value,
) -> Option<FileOperation> {
    let path = extract_path(raw_input, &["file_path", "path", "file"])?;
    let old_text = raw_input
        .get("content")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    Some(FileOperation::Delete { path, old_text })
}

pub(super) fn file_operation_from_move_input(
    raw_input: &serde_json::Value,
) -> Option<FileOperation> {
    let from_path = extract_path(raw_input, &["from", "from_path", "path", "file"])?;
    let to_path = extract_path(raw_input, &["to", "to_path", "destination", "new_path"])?;
    let old_text = raw_input
        .get("old_string")
        .or_else(|| raw_input.get("content"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let new_text = raw_input
        .get("new_string")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .or_else(|| old_text.clone());

    Some(FileOperation::Move {
        from_path,
        to_path,
        old_text,
        new_text,
    })
}

pub(super) fn extract_path(raw_input: &serde_json::Value, keys: &[&str]) -> Option<PathBuf> {
    keys.iter()
        .find_map(|key| raw_input.get(*key))
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
}

pub(super) fn extract_command(raw_input: &serde_json::Value) -> Option<String> {
    let command = raw_input.get("command").or_else(|| raw_input.get("cmd"));

    command
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            command
                .and_then(serde_json::Value::as_array)
                .and_then(|command| command_from_array(command))
        })
        .or_else(|| parsed_command_query(raw_input))
}

pub(super) fn command_from_array(command: &[serde_json::Value]) -> Option<String> {
    let command = command
        .iter()
        .map(serde_json::Value::as_str)
        .collect::<Option<Vec<_>>>()?;

    match command.as_slice() {
        [shell, flag, script] if is_shell_wrapper(shell, flag) => Some((*script).to_string()),
        _ => Some(command.join(" ")),
    }
}

pub(super) fn is_shell_wrapper(shell: &str, flag: &str) -> bool {
    let shell_name = shell.rsplit('/').next().unwrap_or(shell);
    matches!(flag, "-c" | "-lc")
        && matches!(
            shell_name,
            "bash" | "sh" | "zsh" | "fish" | "pwsh" | "powershell"
        )
}

pub(super) fn parsed_commands(
    raw_input: &serde_json::Value,
) -> impl Iterator<Item = &serde_json::Value> {
    raw_input
        .get("parsed_cmd")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
}

pub(super) fn find_parsed_command<'a, F>(
    raw_input: &'a serde_json::Value,
    predicate: F,
) -> Option<&'a serde_json::Value>
where
    F: FnMut(&&'a serde_json::Value) -> bool,
{
    parsed_commands(raw_input).find(predicate)
}

pub(super) fn parsed_command_type_value(parsed_command: &serde_json::Value) -> Option<&str> {
    parsed_command.get("type")?.as_str()
}

pub(super) fn parsed_command_path_value(parsed_command: &serde_json::Value) -> Option<PathBuf> {
    parsed_command.get("path")?.as_str().map(PathBuf::from)
}

pub(super) fn parsed_command_query_value(parsed_command: &serde_json::Value) -> Option<String> {
    ["pattern", "query", "cmd"]
        .iter()
        .find_map(|key| parsed_command.get(*key)?.as_str())
        .map(str::to_string)
}

pub(super) fn parsed_command_query(raw_input: &serde_json::Value) -> Option<String> {
    find_parsed_command(raw_input, |_| true).and_then(parsed_command_query_value)
}

pub(super) fn parsed_command_is_listing(raw_input: &serde_json::Value) -> bool {
    find_parsed_command(raw_input, |parsed_command| {
        matches!(
            parsed_command_type_value(parsed_command),
            Some("list_files")
        )
    })
    .is_some()
}

pub(super) fn parsed_command_is_search(raw_input: &serde_json::Value) -> bool {
    find_parsed_command(raw_input, |parsed_command| {
        parsed_command_type_value(parsed_command).is_some_and(|type_| {
            type_.contains("search")
                || type_.contains("grep")
                || type_.contains("rg")
                || type_.contains("glob")
        })
    })
    .is_some()
}

pub(super) fn parsed_command_path_for(
    raw_input: &serde_json::Value,
    predicate: impl FnMut(&&serde_json::Value) -> bool,
) -> Option<PathBuf> {
    find_parsed_command(raw_input, predicate).and_then(parsed_command_path_value)
}

pub(super) fn parsed_command_query_for_search(raw_input: &serde_json::Value) -> Option<String> {
    find_parsed_command(raw_input, |parsed_command| {
        parsed_command_type_value(parsed_command).is_some_and(|type_| {
            type_.contains("search")
                || type_.contains("grep")
                || type_.contains("rg")
                || type_.contains("glob")
        })
    })
    .and_then(parsed_command_query_value)
}

pub(super) fn raw_output_text(raw_output: Option<&serde_json::Value>) -> Option<String> {
    let raw_output = raw_output?;

    if let Some(stdout) = raw_output.get("stdout").and_then(serde_json::Value::as_str)
        && !stdout.is_empty()
    {
        return Some(stdout.to_string());
    }

    if let Some(formatted) = raw_output
        .get("formatted_output")
        .and_then(serde_json::Value::as_str)
        && !formatted.is_empty()
    {
        return Some(formatted.to_string());
    }

    if let Some(aggregated) = raw_output
        .get("aggregated_output")
        .and_then(serde_json::Value::as_str)
        && !aggregated.is_empty()
    {
        return Some(aggregated.to_string());
    }

    if let Some(lines) = raw_output.get("lines").and_then(serde_json::Value::as_i64) {
        return Some(format!("Read {lines} lines"));
    }

    if let Some(count) = raw_output.get("count").and_then(serde_json::Value::as_i64) {
        return Some(format!("{count} matches"));
    }

    None
}

pub(super) fn title_looks_like_listing(title: &str) -> bool {
    let title = title.to_lowercase();
    title.contains("list") || title.contains("glob") || title.contains("ls")
}

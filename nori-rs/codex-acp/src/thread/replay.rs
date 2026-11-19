//! Replaying a saved conversation to the client when a session is loaded: events carry the
//! messages, response items carry the tool calls.

use std::path::PathBuf;

use crate::thread::actor::ThreadActor;
use crate::thread::backends::Auth;
use crate::thread::exec::ParseCommandToolCall;
use crate::thread::exec::parse_command_tool_call;
use crate::thread::media::image_generation_content;
use crate::thread::media::image_generation_tool_status;
use crate::thread::media::web_search_action_to_title_and_id;
use crate::thread::prompt::format_thread_goal_update;
use codex_apply_patch::parse_patch;
use codex_history::RolloutItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentMessageEvent;
use codex_protocol::protocol::AgentReasoningEvent;
use codex_protocol::protocol::AgentReasoningRawContentEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::UserMessageEvent;
use codex_shell_command::parse_command::parse_command;
use nori_acp_kit::acp::Error;
use nori_acp_kit::schema::Diff;
use nori_acp_kit::schema::ToolCall;
use nori_acp_kit::schema::ToolCallContent;
use nori_acp_kit::schema::ToolCallLocation;
use nori_acp_kit::schema::ToolCallStatus;
use nori_acp_kit::schema::ToolKind;
use uuid::Uuid;

impl<A: Auth> ThreadActor<A> {
    /// Replay conversation history to the client via session/update notifications.
    /// This is called when loading a session to stream all prior messages.
    ///
    /// We process both `EventMsg` and `ResponseItem`:
    /// - `EventMsg` for user/agent messages and reasoning (like the TUI does)
    /// - `ResponseItem` for tool calls only (not persisted as EventMsg)
    pub(super) fn handle_replay_history(&mut self, history: Vec<RolloutItem>) -> Result<(), Error> {
        for item in history {
            match item {
                RolloutItem::EventMsg(event_msg) => {
                    self.replay_event_msg(&event_msg);
                }
                RolloutItem::ResponseItem(response_item) => {
                    self.replay_response_item(&response_item);
                }
                // Skip SessionMeta, TurnContext, Compacted
                _ => {}
            }
        }
        Ok(())
    }

    /// Convert and send an EventMsg as ACP notification(s) during replay.
    /// Handles messages and reasoning - mirrors the live event handling in PromptState.
    pub(super) fn replay_event_msg(&self, msg: &EventMsg) {
        match msg {
            EventMsg::UserMessage(UserMessageEvent { message, .. }) => {
                self.client.send_user_message(message.clone());
            }
            EventMsg::AgentMessage(AgentMessageEvent { message, .. }) => {
                self.client.send_agent_text(message.clone());
            }
            EventMsg::AgentReasoning(AgentReasoningEvent { text }) => {
                self.client.send_agent_thought(text.clone());
            }
            EventMsg::AgentReasoningRawContent(AgentReasoningRawContentEvent { text }) => {
                self.client.send_agent_thought(text.clone());
            }
            EventMsg::ThreadGoalUpdated(event) => {
                self.client
                    .send_agent_text(format_thread_goal_update(event));
            }
            // Skip other event types during replay - they either:
            // - Are transient (deltas, turn lifecycle)
            // - Don't have direct ACP equivalents
            // - Are handled via ResponseItem instead
            _ => {}
        }
    }

    /// Parse apply_patch call input to extract patch content for display.
    /// Returns (title, locations, content) if successful.
    /// For CustomToolCall, the input is the patch string directly.
    pub(super) fn parse_apply_patch_call(
        &self,
        input: &str,
    ) -> Option<(String, Vec<ToolCallLocation>, Vec<ToolCallContent>)> {
        // Try to parse the patch using codex-apply-patch parser
        let parsed = parse_patch(input).ok()?;

        let mut locations = Vec::new();
        let mut file_names = Vec::new();
        let mut content = Vec::new();

        for hunk in &parsed.hunks {
            match hunk {
                codex_apply_patch::Hunk::AddFile { path, contents } => {
                    let full_path = self.config.cwd.as_path().join(path);
                    file_names.push(path.display().to_string());
                    locations.push(ToolCallLocation::new(full_path.clone()));
                    // New file: no old_text, new_text is the contents
                    content.push(ToolCallContent::Diff(Diff::new(
                        full_path,
                        contents.clone(),
                    )));
                }
                codex_apply_patch::Hunk::DeleteFile { path } => {
                    let full_path = self.config.cwd.as_path().join(path);
                    file_names.push(path.display().to_string());
                    locations.push(ToolCallLocation::new(full_path.clone()));
                    // Delete file: old_text would be original content, new_text is empty
                    content.push(ToolCallContent::Diff(
                        Diff::new(full_path, "").old_text("[file deleted]"),
                    ));
                }
                codex_apply_patch::Hunk::UpdateFile {
                    path,
                    move_path,
                    chunks,
                } => {
                    let full_path = self.config.cwd.as_path().join(path);
                    let dest_path = move_path
                        .as_ref()
                        .map(|p| self.config.cwd.as_path().join(p))
                        .unwrap_or_else(|| full_path.clone());
                    file_names.push(path.display().to_string());
                    locations.push(ToolCallLocation::new(dest_path.clone()));

                    // Build old and new text from chunks
                    let old_lines: Vec<String> = chunks
                        .iter()
                        .flat_map(|c| c.old_lines.iter().cloned())
                        .collect();
                    let new_lines: Vec<String> = chunks
                        .iter()
                        .flat_map(|c| c.new_lines.iter().cloned())
                        .collect();

                    content.push(ToolCallContent::Diff(
                        Diff::new(dest_path, new_lines.join("\n")).old_text(old_lines.join("\n")),
                    ));
                }
            }
        }

        let title = if file_names.is_empty() {
            "Apply patch".to_string()
        } else {
            format!("Edit {}", file_names.join(", "))
        };

        Some((title, locations, content))
    }

    /// Parse shell function call arguments to extract command info for rich display.
    /// Returns (title, kind, locations) if successful.
    ///
    /// Handles both:
    /// - `shell` / `container.exec`: `command` is `Vec<String>`
    /// - `shell_command`: `command` is a `String` (shell script)
    pub(super) fn parse_shell_function_call(
        &self,
        name: &str,
        arguments: &str,
    ) -> Option<(String, ToolKind, Vec<ToolCallLocation>)> {
        // Extract command and workdir based on tool type
        let (command_vec, workdir): (Vec<String>, Option<String>) = if name == "shell_command" {
            // shell_command: command is a string (shell script)
            #[derive(serde::Deserialize)]
            struct ShellCommandArgs {
                command: String,
                #[serde(default)]
                workdir: Option<String>,
            }
            let args: ShellCommandArgs = serde_json::from_str(arguments).ok()?;
            // Wrap in bash -lc for parsing
            (
                vec!["bash".to_string(), "-lc".to_string(), args.command],
                args.workdir,
            )
        } else {
            // shell / container.exec: command is Vec<String>
            #[derive(serde::Deserialize)]
            struct ShellArgs {
                command: Vec<String>,
                #[serde(default)]
                workdir: Option<String>,
            }
            let args: ShellArgs = serde_json::from_str(arguments).ok()?;
            (args.command, args.workdir)
        };

        let cwd = workdir
            .map(PathBuf::from)
            .unwrap_or_else(|| self.config.cwd.clone().into());

        let parsed_cmd = parse_command(&command_vec);
        let ParseCommandToolCall {
            title,
            file_extension: _,
            terminal_output: _,
            locations,
            kind,
        } = parse_command_tool_call(parsed_cmd, &cwd);

        Some((title, kind, locations))
    }

    /// Convert and send a single ResponseItem as ACP notification(s) during replay.
    /// Only handles tool calls - messages/reasoning are handled via EventMsg.
    pub(super) fn replay_response_item(&self, item: &ResponseItem) {
        match item {
            // Skip Message and Reasoning - these are handled via EventMsg
            ResponseItem::Message { .. } | ResponseItem::Reasoning { .. } => {}
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } => {
                // Check if this is a shell command - parse it like we do for LocalShellCall
                if matches!(name.as_str(), "shell" | "container.exec" | "shell_command")
                    && let Some((title, kind, locations)) =
                        self.parse_shell_function_call(name, arguments)
                {
                    self.client.send_tool_call(
                        ToolCall::new(call_id.clone(), title)
                            .kind(kind)
                            .status(ToolCallStatus::Completed)
                            .locations(locations)
                            .raw_input(serde_json::from_str::<serde_json::Value>(arguments).ok()),
                    );
                    return;
                }

                // Fall through to generic function call handling
                self.client.send_completed_tool_call(
                    call_id.clone(),
                    name.clone(),
                    ToolKind::Other,
                    serde_json::from_str(arguments).ok(),
                );
            }
            ResponseItem::FunctionCallOutput {
                call_id: Some(call_id),
                output,
                ..
            } => {
                self.client
                    .send_tool_call_completed(call_id.clone(), serde_json::to_value(output).ok());
            }
            ResponseItem::LocalShellCall {
                call_id: Some(call_id),
                action,
                status,
                ..
            } => {
                let codex_protocol::models::LocalShellAction::Exec(exec) = action;
                let cwd = exec
                    .working_directory
                    .as_ref()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| self.config.cwd.clone().into());

                // Parse the command to get rich info like the live event handler does
                let parsed_cmd = parse_command(&exec.command);
                let ParseCommandToolCall {
                    title,
                    file_extension: _,
                    terminal_output: _,
                    locations,
                    kind,
                } = parse_command_tool_call(parsed_cmd, &cwd);

                let tool_status = match status {
                    codex_protocol::models::LocalShellStatus::Completed => {
                        ToolCallStatus::Completed
                    }
                    codex_protocol::models::LocalShellStatus::InProgress
                    | codex_protocol::models::LocalShellStatus::Incomplete => {
                        ToolCallStatus::Failed
                    }
                };
                self.client.send_tool_call(
                    ToolCall::new(call_id.clone(), title)
                        .kind(kind)
                        .status(tool_status)
                        .locations(locations),
                );
            }
            ResponseItem::CustomToolCall {
                name,
                input,
                call_id,
                ..
            } => {
                // Check if this is an apply_patch call - show the patch content
                if name == "apply_patch"
                    && let Some((title, locations, content)) = self.parse_apply_patch_call(input)
                {
                    self.client.send_tool_call(
                        ToolCall::new(call_id.clone(), title)
                            .kind(ToolKind::Edit)
                            .status(ToolCallStatus::Completed)
                            .locations(locations)
                            .content(content)
                            .raw_input(serde_json::from_str::<serde_json::Value>(input).ok()),
                    );
                    return;
                }

                // Fall through to generic custom tool call handling
                self.client.send_completed_tool_call(
                    call_id.clone(),
                    name.clone(),
                    ToolKind::Other,
                    serde_json::from_str(input).ok(),
                );
            }
            ResponseItem::CustomToolCallOutput {
                name: _,
                call_id,
                output,
                ..
            } => {
                self.client
                    .send_tool_call_completed(call_id.clone(), Some(serde_json::json!(output)));
            }
            ResponseItem::WebSearchCall { id, action, .. } => {
                let (title, call_id) = if let Some(action) = action {
                    web_search_action_to_title_and_id(&id.as_ref().map(ToString::to_string), action)
                } else {
                    ("Web Search".into(), generate_fallback_id("web_search"))
                };
                self.client.send_tool_call(
                    ToolCall::new(call_id, title)
                        .kind(ToolKind::Search)
                        .status(ToolCallStatus::Completed),
                );
            }
            ResponseItem::ImageGenerationCall {
                id,
                status,
                revised_prompt,
                result,
                ..
            } => {
                self.client.send_tool_call(
                    ToolCall::new(
                        id.as_ref()
                            .map(ToString::to_string)
                            .unwrap_or_else(|| generate_fallback_id("image_generation")),
                        "Image generation",
                    )
                    .kind(ToolKind::Other)
                    .status(image_generation_tool_status(status))
                    .content(image_generation_content(
                        revised_prompt.clone(),
                        result.clone(),
                        None,
                    ))
                    .raw_output(serde_json::json!({
                        "status": status,
                        "revised_prompt": revised_prompt,
                        "result": result,
                    })),
                );
            }
            // Skip GhostSnapshot, Compaction, Other, LocalShellCall without call_id
            _ => {}
        }
    }
}

/// Generate a fallback ID using UUID (used when id is missing)
pub(super) fn generate_fallback_id(prefix: &str) -> String {
    format!("{}_{}", prefix, Uuid::new_v4())
}

//! Running Bash through the client's terminal, with streamed output.

use super::*;

impl AcpMcpServer {
    /// Execute Bash tool with streaming output via meta field
    ///
    /// This implementation bypasses the Terminal API (which causes dispatch loop deadlock)
    /// and instead executes commands directly, sending terminal-like updates via the
    /// _meta field in ToolCallUpdate notifications.
    ///
    /// Zed supports these meta fields:
    /// - terminal_info: { terminal_id, cwd } - sent at start
    /// - terminal_output: { terminal_id, data } - sent for each output chunk
    /// - terminal_exit: { terminal_id, exit_code } - sent when command completes
    #[instrument(
        name = "acp_bash_tool",
        skip(self, arguments, context),
        fields(
            tool_use_id = ?tool_use_id,
        )
    )]
    pub(super) async fn execute_bash_tool(
        &self,
        arguments: Value,
        tool_use_id: Option<&str>,
        context: &ToolContext,
    ) -> Result<ToolResult, String> {
        let bash_start = Instant::now();

        // OnceLock provides lock-free access after initialization
        let session_id = self.session_id.get();
        let connection_cx = self.connection_cx.get();

        // Extract command parameters
        let command = arguments
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or("Missing command")?
            .to_string();
        let description = arguments
            .get("description")
            .and_then(|v| v.as_str())
            .map(String::from);
        let run_in_background = arguments
            .get("run_in_background")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let timeout_ms = arguments
            .get("timeout")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(120_000)
            .min(600_000); // Max 10 minutes

        // Generate unique terminal ID for tracking
        let terminal_id = uuid::Uuid::new_v4().to_string();

        // Title is the actual command (no truncation, let UI handle display)
        let title = command.clone();

        tracing::info!(
            command = %command,
            description = ?description,
            terminal_id = %terminal_id,
            run_in_background = run_in_background,
            timeout_ms = timeout_ms,
            cwd = ?context.cwd,
            "Executing Bash command with streaming output"
        );

        // Send terminal_info notification at start (if we have connection)
        // IMPORTANT: Use ToolCall (not ToolCallUpdate) notification because Zed
        // only creates terminals when terminal_info is in a ToolCall notification.
        if let (Some(cx), Some(session_id), Some(tool_use_id)) =
            (connection_cx, session_id, tool_use_id)
        {
            // Build meta with terminal_info and optional description for future use
            let mut meta_json = serde_json::json!({
                "terminal_info": {
                    "terminal_id": &terminal_id,
                    "cwd": context.cwd.display().to_string()
                }
            });
            // Add description to meta if available (for future use by clients)
            if let Some(ref desc) = description {
                meta_json["description"] = serde_json::json!(desc);
            }
            let meta = Self::value_to_meta(meta_json);
            if let Err(e) = Self::send_tool_call_with_meta(
                cx,
                session_id,
                tool_use_id,
                Some(&title),
                ToolCallStatus::InProgress,
                Some(&terminal_id), // Pass terminal_id for content association
                meta,
            ) {
                tracing::debug!("Failed to send terminal_info: {}", e);
            }
        }

        // Drop locks before executing command
        let cx_clone = connection_cx.cloned();
        let session_id_clone = session_id.cloned();
        let tool_use_id_clone = tool_use_id.map(String::from);

        // Execute command with streaming
        let result = self
            .execute_command_with_streaming(
                &command,
                &terminal_id,
                context,
                timeout_ms,
                run_in_background,
                cx_clone.as_ref(),
                session_id_clone.as_deref(),
                tool_use_id_clone.as_deref(),
            )
            .await;

        // Send terminal_exit notification
        if let (Some(cx), Some(session_id), Some(tool_use_id)) = (
            cx_clone.as_ref(),
            session_id_clone.as_ref(),
            tool_use_id_clone.as_ref(),
        ) {
            let exit_code = match &result {
                Ok(r) if !r.is_error => 0,
                _ => 1,
            };
            let meta = Self::value_to_meta(serde_json::json!({
                "terminal_exit": {
                    "terminal_id": &terminal_id,
                    "exit_code": exit_code
                }
            }));
            if let Err(e) = Self::send_tool_call_update_with_meta(
                cx,
                session_id,
                tool_use_id,
                Some(ToolCallStatus::Completed),
                None,
                None, // No content for terminal_exit
                meta,
            ) {
                tracing::debug!("Failed to send terminal_exit: {}", e);
            }
        }

        let bash_elapsed = bash_start.elapsed();
        tracing::info!(
            command = %command,
            terminal_id = %terminal_id,
            total_elapsed_ms = bash_elapsed.as_millis(),
            is_error = result.as_ref().map(|r| r.is_error).unwrap_or(true),
            "Bash command completed"
        );

        result
    }

    /// Execute command with streaming output via meta field
    ///
    /// This function executes the command directly using tokio::process::Command
    /// and sends output chunks via ToolCallUpdate notifications with terminal_output meta.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn execute_command_with_streaming(
        &self,
        command: &str,
        terminal_id: &str,
        context: &ToolContext,
        timeout_ms: u64,
        run_in_background: bool,
        cx: Option<&Cx>,
        session_id: Option<&str>,
        tool_use_id: Option<&str>,
    ) -> Result<ToolResult, String> {
        use tokio::io::AsyncBufReadExt;
        use tokio::io::BufReader;
        use tokio::process::Command;

        // Spawn the command
        let mut child = Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(&context.cwd)
            .env("CLAUDECODE", "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to spawn command: {e}"))?;

        // Handle background execution
        if run_in_background {
            let shell_id = format!("term-{terminal_id}");
            tracing::info!(shell_id = %shell_id, "Command started in background");
            return Ok(ToolResult::success(format!(
                "Command started in background.\n\nShell ID: {shell_id}\n\nUse BashOutput to check status."
            )));
        }

        // Collect output
        let mut output = String::new();
        let mut stderr_output = String::new();

        // Take stdout and stderr
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        // Read stdout in a task
        let stdout_task = if let Some(stdout) = stdout {
            let cx = cx.cloned();
            let session_id = session_id.map(String::from);
            let tool_use_id = tool_use_id.map(String::from);
            let terminal_id = terminal_id.to_string();

            Some(tokio::spawn(async move {
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();
                let mut collected = String::new();

                while let Ok(Some(line)) = lines.next_line().await {
                    collected.push_str(&line);
                    collected.push('\n');

                    // Send terminal_output notification
                    if let (Some(cx), Some(session_id), Some(tool_use_id)) =
                        (cx.as_ref(), session_id.as_ref(), tool_use_id.as_ref())
                    {
                        let meta = Self::value_to_meta(serde_json::json!({
                            "terminal_output": {
                                "terminal_id": &terminal_id,
                                "data": format!("{}\n", line)
                            }
                        }));
                        drop(Self::send_tool_call_update_with_meta(
                            cx,
                            session_id,
                            tool_use_id,
                            None, // No status change for terminal_output
                            None,
                            None, // No content for terminal_output
                            meta,
                        ));
                    }
                }
                collected
            }))
        } else {
            None
        };

        // Read stderr in a task
        let stderr_task = stderr.map(|stderr| {
            tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                let mut collected = String::new();

                while let Ok(Some(line)) = lines.next_line().await {
                    collected.push_str(&line);
                    collected.push('\n');
                }
                collected
            })
        });

        // Wait for command with timeout
        let timeout_duration = std::time::Duration::from_millis(timeout_ms);
        let wait_result = tokio::time::timeout(timeout_duration, child.wait()).await;

        // Collect outputs
        if let Some(task) = stdout_task
            && let Ok(out) = task.await
        {
            output = out;
        }
        if let Some(task) = stderr_task
            && let Ok(err) = task.await
        {
            stderr_output = err;
        }

        // Combine output
        let combined_output = if stderr_output.is_empty() {
            output
        } else if output.is_empty() {
            stderr_output
        } else {
            format!("{output}\n--- stderr ---\n{stderr_output}")
        };

        // Process result
        match wait_result {
            Ok(Ok(status)) => {
                let exit_code = status.code().unwrap_or(-1);
                tracing::info!(exit_code = exit_code, command = %command, "Command completed");

                if status.success() {
                    Ok(ToolResult::success(combined_output))
                } else {
                    Ok(ToolResult::error(format!(
                        "Command failed with exit code {exit_code}\n{combined_output}"
                    )))
                }
            }
            Ok(Err(e)) => {
                tracing::error!(error = %e, "Failed to wait for command");
                Ok(ToolResult::error(format!(
                    "Failed to wait for command: {e}\n{combined_output}"
                )))
            }
            Err(_) => {
                tracing::warn!(timeout_ms = timeout_ms, "Command timed out");
                // Try to kill the process
                drop(child.kill().await);
                Ok(ToolResult::error(format!(
                    "Command timed out after {timeout_ms}ms\n{combined_output}"
                )))
            }
        }
    }
}

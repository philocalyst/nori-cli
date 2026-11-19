//! The MCP JSON-RPC surface the SDK drives: `initialize`, `tools/list`, `tools/call`.

use super::*;

/// Placeholder handler for SDK tool definitions
pub(super) struct PlaceholderHandler;

impl ToolHandler for PlaceholderHandler {
    fn handle(
        &self,
        args: Value,
    ) -> BoxFuture<'static, claude_code_agent_sdk::errors::Result<SdkToolResult>> {
        tracing::warn!("PlaceholderHandler called with args: {:?}", args);
        async move {
            // This should never be called - execution goes through AcpMcpServer::handle_message
            tracing::error!("PlaceholderHandler was called! SDK is not using handle_message!");
            Ok(SdkToolResult {
                content: vec![claude_code_agent_sdk::McpToolResultContent::Text {
                    text: "Tool execution error: placeholder handler called".to_string(),
                }],
                is_error: true,
            })
        }
        .boxed()
    }
}

#[async_trait]
impl SdkMcpServer for AcpMcpServer {
    #[instrument(
        name = "acp_handle_message",
        skip(self, message),
        fields(
            method = tracing::field::Empty,
            message_size = message.to_string().len(),
        )
    )]
    async fn handle_message(&self, message: Value) -> claude_code_agent_sdk::errors::Result<Value> {
        let start_time = Instant::now();

        let method = message["method"].as_str().ok_or_else(|| {
            claude_code_agent_sdk::errors::ClaudeError::Transport("Missing method".to_string())
        })?;

        // Record method to span
        tracing::Span::current().record("method", method);

        tracing::debug!(
            method = %method,
            message_id = ?message.get("id"),
            "AcpMcpServer handling message"
        );

        let result = match method {
            "initialize" => {
                tracing::info!(
                    server_name = %self.name,
                    server_version = %self.version,
                    "ACP MCP server initializing"
                );
                Ok(serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {}
                    },
                    "serverInfo": {
                        "name": self.name,
                        "version": self.version
                    }
                }))
            }
            "tools/list" => {
                // The tools that stand in for the CLI's own file and terminal tools stay loaded:
                // behind Claude Code's tool search, the model would first have to look for its
                // Read and Edit.
                let replacements = crate::session::acp_replacement_tools();
                let tools: Vec<_> = self
                    .tools
                    .values()
                    .map(|t| {
                        let mut tool = serde_json::json!({
                            "name": t.name,
                            "description": t.description,
                            "inputSchema": t.input_schema
                        });
                        if replacements.contains(&t.name.as_str()) {
                            tool["_meta"] = serde_json::json!({"anthropic/alwaysLoad": true});
                        }
                        tool
                    })
                    .collect();

                let tool_names: Vec<&str> =
                    self.tools.keys().map(std::string::String::as_str).collect();
                tracing::info!(
                    tool_count = tools.len(),
                    tools = ?tool_names,
                    "Returning tool list"
                );
                Ok(serde_json::json!({
                    "tools": tools
                }))
            }
            "tools/call" => {
                let params = &message["params"];
                let tool_name = params["name"].as_str().ok_or_else(|| {
                    claude_code_agent_sdk::errors::ClaudeError::Transport(
                        "Missing tool name".to_string(),
                    )
                })?;
                let arguments = params["arguments"].clone();

                // Get tool_use_id from _meta if available
                let tool_use_id = params
                    .get("_meta")
                    .and_then(|m| m.get("claudecode/toolUseId"))
                    .and_then(|v| v.as_str());

                // Log full _meta for debugging
                let meta_preview = params.get("_meta").map(|m| {
                    if m.as_object().map(serde_json::Map::len).unwrap_or(0) > 1 {
                        format!("{m:?}")
                    } else {
                        "{claudecode/toolUseId}".to_string()
                    }
                });

                tracing::info!(
                    tool_name = %tool_name,
                    tool_use_id = ?tool_use_id,
                    has_meta = params.get("_meta").is_some(),
                    meta_preview = ?meta_preview,
                    args_size = arguments.to_string().len(),
                    "Received tools/call request from Claude CLI"
                );

                let tool_start = Instant::now();

                let result = self
                    .execute_tool(tool_name, arguments, tool_use_id)
                    .await
                    .map_err(|e| {
                        tracing::error!(
                            tool_name = %tool_name,
                            error = %e,
                            "Tool execution failed"
                        );
                        claude_code_agent_sdk::errors::ClaudeError::Transport(e)
                    })?;

                let tool_elapsed = tool_start.elapsed();

                tracing::info!(
                    tool_name = %tool_name,
                    elapsed_ms = tool_elapsed.as_millis(),
                    is_error = result.is_error,
                    content_len = result.content.len(),
                    "Tool execution completed"
                );

                Ok(serde_json::json!({
                    "content": [{
                        "type": "text",
                        "text": result.content
                    }],
                    "is_error": result.is_error
                }))
            }
            // MCP notifications - these don't expect a response but we return empty success
            "notifications/cancelled" => {
                // Handle cancellation notification
                // The request_id is in params.requestId
                let request_id = message["params"]["requestId"].as_str();
                tracing::info!(
                    request_id = ?request_id,
                    "Received MCP cancellation notification"
                );

                // Call the cancel callback to interrupt Claude CLI
                // Use try_lock to avoid potential deadlock if callback is locked
                match self.cancel_callback.try_lock() {
                    Ok(callback) => {
                        if let Some(ref cb) = *callback {
                            tracing::info!("Invoking cancel callback to interrupt Claude CLI");
                            cb();
                        } else {
                            tracing::warn!(
                                "No cancel callback registered, cancellation may not take effect"
                            );
                        }
                    }
                    Err(_) => {
                        tracing::warn!(
                            "Cancel callback is busy, cannot invoke cancellation safely"
                        );
                    }
                }

                Ok(serde_json::json!({}))
            }
            "notifications/initialized" => {
                tracing::debug!("Received initialized notification from Claude CLI");
                Ok(serde_json::json!({}))
            }
            "notifications/progress" => {
                let progress_token = message["params"]["progressToken"].as_str();
                let progress = message["params"]["progress"].as_f64();
                tracing::trace!(
                    progress_token = ?progress_token,
                    progress = ?progress,
                    "Received progress notification"
                );
                Ok(serde_json::json!({}))
            }
            _ => {
                // Check if it's a notification (starts with "notifications/")
                if method.starts_with("notifications/") {
                    tracing::debug!(
                        method = %method,
                        "Received unknown notification, ignoring"
                    );
                    Ok(serde_json::json!({}))
                } else {
                    tracing::warn!(
                        method = %method,
                        "Received unknown method"
                    );
                    Err(claude_code_agent_sdk::errors::ClaudeError::Transport(
                        format!("Unknown method: {method}"),
                    ))
                }
            }
        };

        let elapsed = start_time.elapsed();
        tracing::debug!(
            method = %method,
            elapsed_ms = elapsed.as_millis(),
            is_ok = result.is_ok(),
            "Message handling completed"
        );

        result
    }

    fn list_tools(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .map(|t| ToolDefinition {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: t.input_schema.clone(),
                annotations: t.annotations.clone(),
            })
            .collect()
    }
}

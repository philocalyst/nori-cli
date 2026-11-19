//! The answers the PreToolUse hook gives the CLI, and the tool-result notification that goes with a
//! denial.

use std::sync::Arc;
use std::sync::OnceLock;

use claude_code_agent_sdk::HookJsonOutput;
use claude_code_agent_sdk::HookSpecificOutput;
use claude_code_agent_sdk::PreToolUseHookSpecificOutput;
use claude_code_agent_sdk::SyncHookJsonOutput;
use nori_acp_kit::Cx;
use nori_acp_kit::schema::SessionId;
use nori_acp_kit::schema::SessionNotification;
use nori_acp_kit::schema::SessionUpdate;
use nori_acp_kit::schema::ToolCallContent;
use nori_acp_kit::schema::ToolCallId;
use nori_acp_kit::schema::ToolCallStatus;
use nori_acp_kit::schema::ToolCallUpdate;
use nori_acp_kit::schema::ToolCallUpdateFields;

fn decision(kind: &str, reason: Option<String>) -> HookJsonOutput {
    HookJsonOutput::Sync(SyncHookJsonOutput {
        continue_: Some(true),
        hook_specific_output: Some(HookSpecificOutput::PreToolUse(
            PreToolUseHookSpecificOutput {
                permission_decision: Some(kind.to_string()),
                permission_decision_reason: reason,
                updated_input: None,
                additional_context: None,
            },
        )),
        ..Default::default()
    })
}

/// Let the tool run.
pub(super) fn allow(reason: impl Into<String>) -> HookJsonOutput {
    decision("allow", Some(reason.into()))
}

/// Let the tool run, for the reason of the rule that allowed it.
pub(super) fn allow_opt(reason: Option<String>) -> HookJsonOutput {
    decision("allow", reason)
}

/// Leave the decision to the `can_use_tool` callback.
pub(super) fn defer(reason: impl Into<String>) -> HookJsonOutput {
    decision("defer", Some(reason.into()))
}

/// Block the tool.
fn deny(reason: String) -> HookJsonOutput {
    decision("deny", Some(reason))
}

/// Send a tool result notification when a tool is denied by permission check
///
/// This ensures that clients (like Zed) receive a corresponding tool_result
/// notification even when a tool is blocked before execution.
///
/// # Arguments
///
/// * `connection_cx_lock` - The connection context for sending notifications
/// * `session_id` - The session ID
/// * `tool_use_id` - The tool use ID to correlate with the tool_use notification
/// * `tool_name` - The name of the tool that was denied
/// * `reason` - The reason for the denial
///
/// # Note
///
/// This function sends the notification **synchronously** to ensure:
/// 1. The notification is sent before the hook response
/// 2. The notification can be properly flushed with the flush mechanism
/// 3. Any send errors are detected immediately
fn send_denied_tool_result(
    connection_cx_lock: &Arc<OnceLock<Cx>>,
    session_id: &str,
    tool_use_id: &str,
    tool_name: &str,
    reason: &str,
) {
    let Some(connection_cx) = connection_cx_lock.get() else {
        tracing::warn!(
            tool_name = %tool_name,
            tool_use_id = %tool_use_id,
            "Connection context not available, cannot send denied tool result"
        );
        return;
    };

    let session_id = SessionId::new(session_id.to_string());
    let tool_call_id = ToolCallId::new(tool_use_id.to_string());

    // Build error content
    let error_content = format!("Tool execution denied: {reason}");
    let content: Vec<ToolCallContent> = vec![format!("```\n{error_content}\n```").into()];

    // Build raw_output JSON
    let raw_output = serde_json::json!({
        "content": error_content,
        "is_error": true
    });

    // Create tool result notification with Failed status
    let update_fields = ToolCallUpdateFields::new()
        .status(ToolCallStatus::Failed)
        .content(content)
        .raw_output(raw_output);

    let update = ToolCallUpdate::new(tool_call_id, update_fields);
    let notification = SessionNotification::new(session_id, SessionUpdate::ToolCallUpdate(update));

    // Send the notification synchronously
    // Note: send_notification uses unbounded_send which is non-blocking
    // The actual network IO is handled by the outgoing actor
    if let Err(e) = connection_cx.send_notification(notification) {
        tracing::warn!(
            tool_name = %tool_name,
            tool_use_id = %tool_use_id,
            error = %e,
            "Failed to send denied tool result notification"
        );
    } else {
        tracing::debug!(
            tool_name = %tool_name,
            tool_use_id = %tool_use_id,
            "Sent denied tool result notification"
        );
    }
}

/// Create a deny hook response with tool_result notification
///
/// This helper function ensures that when a tool is denied:
/// 1. A tool_result notification is sent to the client (so Zed doesn't show "not found")
/// 2. The hook returns the proper deny response
///
/// # Arguments
///
/// * `connection_cx_lock` - The connection context for sending notifications
/// * `session_id` - The session ID
/// * `tool_use_id` - Optional tool use ID
/// * `tool_name` - The name of the tool that was denied
/// * `reason` - The reason for the denial
///
/// # Returns
///
/// A HookJsonOutput with deny decision
pub(super) fn deny_with_notification(
    connection_cx_lock: &Arc<OnceLock<Cx>>,
    session_id: &str,
    tool_use_id: Option<&String>,
    tool_name: &str,
    reason: String,
) -> HookJsonOutput {
    // Send tool_result notification to client so Zed doesn't show "Tool call not found"
    // Note: send_notification is non-blocking (uses unbounded_send)
    if let Some(tuid) = tool_use_id {
        send_denied_tool_result(connection_cx_lock, session_id, tuid, tool_name, &reason);
    }

    deny(reason)
}

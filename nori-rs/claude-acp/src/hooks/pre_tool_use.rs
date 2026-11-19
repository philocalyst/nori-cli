//! PreToolUse hook implementation
//!
//! Checks permissions using SettingsManager before tool execution.
//! For "Ask" decisions, sends permission request directly (has correct tool_use_id).

use nori_acp_kit::Cx;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Instant;

use claude_code_agent_sdk::HookCallback;
use claude_code_agent_sdk::HookContext;
use claude_code_agent_sdk::HookInput;
use claude_code_agent_sdk::HookJsonOutput;
use claude_code_agent_sdk::SyncHookJsonOutput;
use dashmap::DashMap;
use futures::future::BoxFuture;
use tokio::sync::RwLock;
use tracing::Instrument;

use super::output;
use crate::command_safety::command_might_be_dangerous;
use crate::command_safety::is_known_safe_command;
use crate::converter::EditSnapshots;
use crate::session::PermissionHandler;
use crate::session::PermissionMode;
use crate::settings::PermissionChecker;
use crate::utils::is_plans_directory_path;

/// Creates a PreToolUse hook that checks permissions using settings rules and permission mode.
///
/// This hook runs before the SDK's built-in permission rules, allowing us to enforce
/// our own permission settings for ACP-prefixed tools.
///
/// # Permission Handling
///
/// - **Allow**: Returns with `permission_decision: "allow"` - tool executes immediately
/// - **Deny**: Returns with `permission_decision: "deny"` - tool execution is blocked
/// - **Ask**: Returns with `permission_decision: "ask"` - SDK triggers permission request flow
///
/// # Permission Mode Integration
///
/// The hook respects the session's permission mode:
/// - **BypassPermissions/AcceptEdits**: Allows all tools without checking rules
///   (AcceptEdits behaves like BypassPermissions for root compatibility)
/// - **Plan**: Blocks write operations (Edit, Write, Bash, NotebookEdit)
/// - **Default**: Auto-allows read-only operations (Read, Grep, Glob, LS, NotebookRead),
///   checks settings rules for other tools
/// - **DontAsk**: Checks settings rules and mode-based auto-approval
///
/// # Architecture
///
/// The hook and `can_use_tool` callback work together:
/// 1. **Hook** (this file): Makes quick decisions based on static rules (allow/deny/ask)
/// 2. **`can_use_tool` callback** (`can_use_tool.rs`): Checks cached permission results
///
/// For "Ask" decisions, the hook sends permission request directly (using the correct tool_use_id),
/// then caches the result. The `can_use_tool` callback checks this cache and returns immediately.
///
/// # Arguments
///
/// * `connection_cx_lock` - Connection for sending permission requests
/// * `session_id` - Session ID for permission requests
/// * `permission_checker` - Optional permission checker for settings-based rules
/// * `permission` - Shared permission handler (contains mode that can be updated at runtime)
/// * `tool_use_id_cache` - Cache for storing tool_use_id (for can_use_tool callback)
///
/// # Returns
///
/// A hook callback that can be used with ClaudeAgentOptions
pub fn create_pre_tool_use_hook(
    connection_cx_lock: Arc<OnceLock<Cx>>,
    session_id: String,
    permission_checker: Option<Arc<RwLock<PermissionChecker>>>,
    permission: Arc<RwLock<PermissionHandler>>,
    tool_use_id_cache: Arc<DashMap<String, String>>,
    edit_snapshots: Arc<EditSnapshots>,
) -> HookCallback {
    Arc::new(
        move |input: HookInput, tool_use_id: Option<String>, _context: HookContext| {
            // Clone the connection_cx_lock for sending denied tool result notifications
            let connection_cx_lock = Arc::clone(&connection_cx_lock);
            let permission_checker = permission_checker.clone();
            let permission = permission.clone();
            let session_id = session_id.clone();
            let tool_use_id_cache = Arc::clone(&tool_use_id_cache);
            let edit_snapshots = Arc::clone(&edit_snapshots);

            // Extract tool name early for span naming
            let (tool_name, is_pre_tool) = match &input {
                HookInput::PreToolUse(pre_tool) => (pre_tool.tool_name.clone(), true),
                _ => (String::new(), false),
            };

            // Create a span for this hook execution
            let span = if is_pre_tool {
                tracing::info_span!(
                    "pre_tool_use_hook",
                    tool_name = %tool_name,
                    tool_use_id = ?tool_use_id,
                    permission_decision = tracing::field::Empty,
                    permission_rule = tracing::field::Empty,
                    check_duration_us = tracing::field::Empty,
                )
            } else {
                tracing::debug_span!(
                    "pre_tool_use_hook_skip",
                    event_type = ?std::mem::discriminant(&input)
                )
            };

            Box::pin(
                async move {
                    let start_time = Instant::now();

                    // Only handle PreToolUse events
                    let (tool_name, tool_input) = if let HookInput::PreToolUse(pre_tool) = &input {
                        (pre_tool.tool_name.clone(), pre_tool.tool_input.clone())
                    } else {
                        tracing::debug!("Ignoring non-PreToolUse event");
                        return HookJsonOutput::Sync(SyncHookJsonOutput {
                            continue_: Some(true),
                            ..Default::default()
                        });
                    };

                    tracing::debug!(
                        tool_name = %tool_name,
                        tool_use_id = ?tool_use_id,
                        "PreToolUse hook triggered"
                    );

                    // Whatever the decision, a file this tool may edit is read first, so its
                    // result can be reported as a diff.
                    if let Some(id) = &tool_use_id {
                        edit_snapshots.record(id, &tool_name, &tool_input).await;
                    }

                    // IMPORTANT: ExitPlanMode is handled specially by canUseTool callback
                    // We skip all permission checks here to avoid double permission prompts
                    // The canUseTool callback will handle the permission dialog
                    let stripped_tool_name = tool_name.strip_prefix("mcp__acp__").unwrap_or(&tool_name);
                    if stripped_tool_name == "ExitPlanMode" {
                        tracing::info!(
                            tool_name = %tool_name,
                            tool_use_id = ?tool_use_id,
                            "ExitPlanMode detected in pre_tool_use - skipping permission checks, delegating to canUseTool callback"
                        );
                        return output::defer("ExitPlanMode permission handled by canUseTool callback".to_string());
                    }

                    // Nori's own tools are the host's: Nori decides what they may do, so they
                    // never prompt, in any mode.
                    if crate::mcp::host_tool_name(&tool_name).is_some() {
                        return output::allow("Nori's own tool".to_string(),);
                    }

                    // Get current permission mode
                    let (mode, plans_dir) = {
                        let permission = permission.read().await;
                        (permission.mode(), permission.plans_dir().to_path_buf())
                    };

                    // BypassPermissions and AcceptEdits modes allow everything
                    // (AcceptEdits behaves like BypassPermissions for root compatibility)
                    if matches!(
                        mode,
                        PermissionMode::BypassPermissions | PermissionMode::AcceptEdits
                    ) {
                        let elapsed = start_time.elapsed();
                        let mode_str = match mode {
                            PermissionMode::BypassPermissions => "BypassPermissions",
                            PermissionMode::AcceptEdits => "AcceptEdits",
                            _ => unreachable!(),
                        };
                        tracing::info!(
                            tool_name = %tool_name,
                            tool_use_id = ?tool_use_id,
                            mode = %mode_str,
                            elapsed_us = elapsed.as_micros(),
                            "Tool allowed by permission mode (auto-approve all)"
                        );

                        return output::allow(format!(
                                        "Allowed by {mode_str} mode (auto-approve all tools)"
                                    ));
                    }

                    // Default mode: auto-allow read-only operations
                    // This allows tools like Read, Grep, Glob, LS, NotebookRead to execute without permission prompt
                    if mode == PermissionMode::Default {
                        let is_read_only = matches!(
                            stripped_tool_name,
                            "Read" | "Grep" | "Glob" | "LS" | "NotebookRead"
                        );
                        if is_read_only {
                            let elapsed = start_time.elapsed();
                            tracing::debug!(
                                tool_name = %tool_name,
                                tool_use_id = ?tool_use_id,
                                mode = "default",
                                elapsed_us = elapsed.as_micros(),
                                "Tool auto-allowed in Default mode (read-only operation)"
                            );
                            return output::allow("Auto-allowed in Default mode (read-only operation)"
                                                .to_string(),);
                        }

                        // Check Bash commands for known safe commands (auto-allow)
                        if stripped_tool_name == "Bash"
                            && let Some(cmd) = tool_input.get("command").and_then(|v| v.as_str()) {
                                // Check if this is a known safe command
                                if is_known_safe_command(cmd) {
                                    let elapsed = start_time.elapsed();
                                    tracing::info!(
                                        tool_name = %tool_name,
                                        command = %cmd,
                                        tool_use_id = ?tool_use_id,
                                        mode = "default",
                                        elapsed_us = elapsed.as_micros(),
                                        "Bash command auto-allowed (known safe command)"
                                    );
                                    return output::allow(format!(
                                                    "Auto-allowed: known safe command ({})",
                                                    cmd.split_whitespace().next().unwrap_or("")
                                                ));
                                }

                                // Check if this is a dangerous command (log warning for user awareness)
                                if command_might_be_dangerous(cmd) {
                                    tracing::warn!(
                                        tool_name = %tool_name,
                                        command = %cmd,
                                        tool_use_id = ?tool_use_id,
                                        "Bash command flagged as potentially dangerous"
                                    );
                                    // Continue to normal permission flow - user will be asked
                                }
                            }
                    }

                    // Plan mode: Block write operations EXCEPT for plan files
                    if mode == PermissionMode::Plan {
                        let is_write_operation = matches!(
                            stripped_tool_name,
                            "Edit" | "Write" | "Bash" | "NotebookEdit"
                        );

                        if is_write_operation {
                            // For file operations, check if writing to plans directory
                            let is_plan_file = if matches!(stripped_tool_name, "Edit" | "Write" | "NotebookEdit") {
                                tool_input
                                    .get("file_path")
                                    .or_else(|| tool_input.get("path"))
                                    .and_then(|v| v.as_str())
                                    .map(|path| is_plans_directory_path(path, &plans_dir))
                                    .unwrap_or(false)
                            } else {
                                // Bash is never allowed in Plan mode
                                false
                            };

                            if !is_plan_file {
                                let reason = format!(
                                    "Tool {stripped_tool_name} is not allowed in Plan mode (only read operations and writing to ~/.claude/plans/ are allowed)"
                                );
                                tracing::warn!(
                                    tool_name = %tool_name,
                                    tool_use_id = ?tool_use_id,
                                    mode = "plan",
                                    elapsed_us = start_time.elapsed().as_micros(),
                                    "Tool blocked by Plan mode"
                                );
                                return output::deny_with_notification(
                                    &connection_cx_lock,
                                    &session_id,
                                    tool_use_id.as_ref(),
                                    &tool_name,
                                    reason,
                                );
                            }

                            // Allow plan file writes
                            tracing::info!(
                                tool_name = %tool_name,
                                file_path = ?tool_input.get("file_path"),
                                "Plan mode: allowing write to plans directory"
                            );
                        }

                        // Auto-allow read operations in Plan mode
                        let is_read_only = matches!(
                            stripped_tool_name,
                            "Read" | "Grep" | "Glob" | "LS" | "NotebookRead"
                        );
                        if is_read_only {
                            return output::allow("Allowed in Plan mode (read-only operation)".to_string());
                        }
                    }

                    // Check permission (if checker is available, otherwise default to Ask)
                    let permission_check = if let Some(checker) = &permission_checker {
                        let checker = checker.read().await;
                        checker.check_permission(&tool_name, &tool_input)
                    } else {
                        // No permission checker - default to Ask
                        crate::settings::PermissionCheckResult {
                            decision: crate::settings::PermissionDecision::Ask,
                            rule: None,
                        }
                    };
                    let elapsed = start_time.elapsed();

                    // Record permission decision to span (batched for performance)
                    let span = tracing::Span::current();
                    span.record(
                        "permission_decision",
                        format!("{:?}", permission_check.decision),
                    );
                    span.record("check_duration_us", elapsed.as_micros());
                    if let Some(ref rule) = permission_check.rule {
                        span.record("permission_rule", rule.as_str());
                    }

                    tracing::info!(
                        tool_name = %tool_name,
                        tool_use_id = ?tool_use_id,
                        decision = ?permission_check.decision,
                        rule = ?permission_check.rule,
                        elapsed_us = elapsed.as_micros(),
                        "Permission check completed"
                    );

                    // 根据权限决策返回相应的 Hook 输出
                    // SDK 已修改为在 mcp_message 处理中调用 can_use_tool 回调，
                    // 因此 Ask 决策会由 SDK 层处理，Hook 只需要返回 continue_: true
                    match permission_check.decision {
                        crate::settings::PermissionDecision::Allow => {
                            tracing::debug!(
                                tool_name = %tool_name,
                                rule = ?permission_check.rule,
                                "Tool execution allowed by rule"
                            );
                            output::allow_opt(permission_check.rule)
                        }
                        crate::settings::PermissionDecision::Deny => {
                            tracing::info!(
                                tool_name = %tool_name,
                                rule = ?permission_check.rule,
                                "Tool execution denied by rule"
                            );
                            let reason = permission_check.rule.unwrap_or_else(|| {
                                // Use stripped tool name, or fall back to original, or a default
                                let display_name = if !stripped_tool_name.is_empty() {
                                    stripped_tool_name
                                } else if !tool_name.is_empty() {
                                    tool_name.as_str()
                                } else {
                                    "the requested tool" // Fallback if both are empty
                                };
                                format!("Tool {display_name} denied by permission settings")
                            });
                            output::deny_with_notification(
                                &connection_cx_lock,
                                &session_id,
                                tool_use_id.as_ref(),
                                &tool_name,
                                reason,
                            )
                        }
                        crate::settings::PermissionDecision::Ask => {
                            // Following TypeScript version's design:
                            // For "ask" decisions, we just return { continue: true } to let the
                            // normal permission flow continue. The actual permission request
                            // will be sent by the can_use_tool callback, NOT here.
                            //
                            // This ensures proper message ordering:
                            // 1. SDK processes tool_use -> sends session/update ToolCall
                            // 2. SDK calls can_use_tool callback
                            // 3. can_use_tool sends requestPermission() and waits for user response
                            //
                            // If we sent requestPermission() here (before can_use_tool), there
                            // could be race conditions with session/update notifications.

                            // Cache tool_use_id for can_use_tool callback to use
                            // The CLI doesn't always pass tool_use_id in mcp_message requests,
                            // so we cache it here where we have it.
                            if let Some(ref tuid) = tool_use_id {
                                let key = crate::session::stable_cache_key(&tool_input);
                                tracing::debug!(
                                    tool_name = %tool_name,
                                    tool_use_id = %tuid,
                                    "Caching tool_use_id for can_use_tool callback"
                                );
                                tool_use_id_cache.insert(key, tuid.clone());
                            }

                            tracing::debug!(
                                tool_name = %tool_name,
                                "Ask decision - delegating to can_use_tool callback"
                            );
                            HookJsonOutput::Sync(SyncHookJsonOutput {
                                continue_: Some(true),
                                hook_specific_output: None,
                                ..Default::default()
                            })
                        }
                    }
                }
                .instrument(span),
            ) as BoxFuture<'static, HookJsonOutput>
        },
    )
}

#[cfg(test)]
mod tests;

//! Permission handling for tool execution
//!
//! This module provides permission checking using a strategy pattern,
//! where each permission mode has its own strategy implementation.

use serde::Deserialize;
use serde::Serialize;
use std::fmt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::permissions::strategies::AcceptEditsModeStrategy;
use crate::permissions::strategies::BypassPermissionsModeStrategy;
use crate::permissions::strategies::DefaultModeStrategy;
use crate::permissions::strategies::DontAskModeStrategy;
use crate::permissions::strategies::PermissionModeStrategy;
use crate::permissions::strategies::PlanModeStrategy;
use crate::settings::PermissionChecker;
use crate::settings::PermissionDecision;
use claude_code_agent_sdk::PermissionMode as SdkPermissionMode;

/// Permission mode for tool execution
///
/// Controls how tool calls are approved during a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// Default mode - prompt for dangerous operations
    Default,
    /// Auto-approve file edits
    AcceptEdits,
    /// Planning mode - read-only operations
    Plan,
    /// Don't ask mode - deny if not pre-approved
    DontAsk,
    /// Bypass all permission checks (default mode for development)
    #[default]
    BypassPermissions,
}

impl PermissionMode {
    /// Parse from string (ACP setMode request)
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            // Nori never asks permission: the modes that ask, kept by older sessions and
            // settings, are Build.
            "default" | "acceptEdits" | "dontAsk" | "bypassPermissions" => {
                Some(Self::BypassPermissions)
            }
            "plan" => Some(Self::Plan),
            _ => None,
        }
    }

    /// Convert to string for SDK
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::DontAsk => "dontAsk",
            Self::BypassPermissions => "bypassPermissions",
        }
    }

    /// Convert to SDK PermissionMode
    ///
    /// Note: SDK doesn't support DontAsk mode yet, so we map it to Default
    pub fn to_sdk_mode(self) -> SdkPermissionMode {
        match self {
            PermissionMode::Default => SdkPermissionMode::Default,
            PermissionMode::AcceptEdits => SdkPermissionMode::AcceptEdits,
            PermissionMode::Plan => SdkPermissionMode::Plan,
            PermissionMode::DontAsk => {
                // SDK doesn't support DontAsk yet, treat as Default
                SdkPermissionMode::Default
            }
            PermissionMode::BypassPermissions => SdkPermissionMode::BypassPermissions,
        }
    }
}

/// Permission check result from the handler
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolPermissionResult {
    /// Tool execution is allowed (auto-approved or by rule)
    Allowed,
    /// Tool execution is blocked (by rule or mode)
    Blocked { reason: String },
    /// User should be asked for permission
    NeedsPermission,
}

/// Permission handler for tool execution
///
/// Uses a strategy pattern where each permission mode has its own strategy.
pub struct PermissionHandler {
    mode: PermissionMode,
    /// Strategy for current permission mode
    strategy: Arc<dyn PermissionModeStrategy>,
    /// Shared permission checker from settings (shared with hook)
    checker: Option<Arc<RwLock<PermissionChecker>>>,
    /// The login's plans directory (`<config dir>/plans`): the one place plan mode may write.
    plans_dir: PathBuf,
}

impl fmt::Debug for PermissionHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PermissionHandler")
            .field("mode", &self.mode)
            .field("strategy", &"<strategy>")
            .field("checker", &self.checker)
            .field("plans_dir", &self.plans_dir)
            .finish()
    }
}

impl Default for PermissionHandler {
    fn default() -> Self {
        Self {
            mode: PermissionMode::Default,
            strategy: Arc::new(DefaultModeStrategy),
            checker: None,
            plans_dir: crate::utils::claude_config_dir(&Default::default())
                .map(|dir| dir.join("plans"))
                .unwrap_or_default(),
        }
    }
}

impl PermissionHandler {
    /// Create with settings-based checker
    ///
    /// Uses Default mode (standard behavior with permission prompts).
    pub fn with_checker(checker: Arc<RwLock<PermissionChecker>>, plans_dir: PathBuf) -> Self {
        Self {
            mode: PermissionMode::Default,
            strategy: Arc::new(DefaultModeStrategy),
            checker: Some(checker),
            plans_dir,
        }
    }

    /// The login's plans directory.
    pub fn plans_dir(&self) -> &Path {
        &self.plans_dir
    }

    /// Create strategy for a given mode
    fn create_strategy(mode: PermissionMode, plans_dir: &Path) -> Arc<dyn PermissionModeStrategy> {
        match mode {
            PermissionMode::Default => Arc::new(DefaultModeStrategy),
            PermissionMode::AcceptEdits => Arc::new(AcceptEditsModeStrategy),
            PermissionMode::Plan => Arc::new(PlanModeStrategy {
                plans_dir: plans_dir.to_path_buf(),
            }),
            PermissionMode::DontAsk => Arc::new(DontAskModeStrategy),
            PermissionMode::BypassPermissions => Arc::new(BypassPermissionsModeStrategy),
        }
    }

    /// Get current permission mode
    pub fn mode(&self) -> PermissionMode {
        self.mode
    }

    /// Set permission mode
    pub fn set_mode(&mut self, mode: PermissionMode) {
        self.mode = mode;
        self.strategy = Self::create_strategy(mode, &self.plans_dir);
    }

    /// Check permission for a tool with full context
    ///
    /// Combines strategy-based checking with settings rules.
    /// Returns the permission result.
    pub async fn check_permission(
        &self,
        tool_name: &str,
        tool_input: &serde_json::Value,
    ) -> ToolPermissionResult {
        // Check settings rules first (if available)
        if let Some(ref checker) = self.checker {
            let checker_read = checker.read().await;
            let result = checker_read.check_permission(tool_name, tool_input);
            match result.decision {
                PermissionDecision::Deny => {
                    return ToolPermissionResult::Blocked {
                        reason: result
                            .rule
                            .map(|r| format!("Denied by rule: {r}"))
                            .unwrap_or_else(|| "Denied by settings".to_string()),
                    };
                }
                PermissionDecision::Allow => {
                    return ToolPermissionResult::Allowed;
                }
                PermissionDecision::Ask => {
                    // Fall through to strategy-based check
                }
            }
        }

        // Tools that talk to the user or plan the work act on nothing, so every mode allows them
        // (DontAsk included: there is nobody to ask about them, and nothing to protect).
        if matches!(
            tool_name,
            "AskUserQuestion" | "Agent" | "Task" | "TodoWrite" | "SlashCommand"
        ) {
            return ToolPermissionResult::Allowed;
        }

        // Use strategy for mode-specific logic
        let strategy_result = self.strategy.check_permission(tool_name, tool_input);

        // Special handling for DontAsk mode: convert NeedsPermission to Blocked
        if self.mode == PermissionMode::DontAsk
            && strategy_result == ToolPermissionResult::NeedsPermission
        {
            return ToolPermissionResult::Blocked {
                reason: "Tool not pre-approved by settings rules in DontAsk mode".to_string(),
            };
        }

        strategy_result
    }

    /// Add a fine-grained allow rule based on tool call details
    /// This is used for "Always Allow" with specific parameters
    pub async fn add_allow_rule_for_tool_call(
        &self,
        tool_name: &str,
        tool_input: &serde_json::Value,
    ) {
        if let Some(ref checker) = self.checker {
            let mut checker_write = checker.write().await;
            checker_write.add_allow_rule_for_tool_call(tool_name, tool_input);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DontAsk denies what no rule allows, but never the tools that plan the work or talk to the
    /// person: they act on nothing.
    #[tokio::test]
    async fn dont_ask_still_lets_the_agent_plan_and_ask() {
        let mut handler = PermissionHandler::default();
        handler.set_mode(PermissionMode::DontAsk);
        for tool in ["TodoWrite", "Task", "AskUserQuestion"] {
            assert_eq!(
                handler.check_permission(tool, &serde_json::json!({})).await,
                ToolPermissionResult::Allowed,
                "{tool}"
            );
        }
        assert!(matches!(
            handler
                .check_permission("Bash", &serde_json::json!({"command": "rm -rf /"}))
                .await,
            ToolPermissionResult::Blocked { .. }
        ));
    }

    #[test]
    fn test_permission_mode_parse() {
        // Every mode that would ask is Build; only Plan stays itself.
        assert_eq!(
            [
                "default",
                "acceptEdits",
                "dontAsk",
                "bypassPermissions",
                "plan",
                "invalid"
            ]
            .map(PermissionMode::parse),
            [
                Some(PermissionMode::BypassPermissions),
                Some(PermissionMode::BypassPermissions),
                Some(PermissionMode::BypassPermissions),
                Some(PermissionMode::BypassPermissions),
                Some(PermissionMode::Plan),
                None
            ]
        );
    }

    #[test]
    fn test_permission_mode_str() {
        assert_eq!(PermissionMode::Default.as_str(), "default");
        assert_eq!(PermissionMode::AcceptEdits.as_str(), "acceptEdits");
        assert_eq!(PermissionMode::Plan.as_str(), "plan");
        assert_eq!(
            PermissionMode::BypassPermissions.as_str(),
            "bypassPermissions"
        );
    }
}

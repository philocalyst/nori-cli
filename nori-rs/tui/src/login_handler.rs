//! Login handler for /login slash command.
//!
//! This module handles authentication flows for ACP agents:
//! - OAuth browser flow (Codex)
//! - External CLI passthrough (Gemini, Claude Code, OpenCode)

use codex_login::ShutdownHandle;
use nori_harness::AgentKind;
use nori_harness::list_available_agents;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Method used for authentication
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginMethod {
    /// OAuth browser flow - starts local server, opens browser
    OAuthBrowser,
    /// External CLI passthrough - spawns agent CLI with PTY
    ExternalCli {
        /// The command to run (e.g., "gemini")
        command: String,
        /// Arguments to pass (e.g., ["login"])
        args: Vec<String>,
    },
}

/// State of the login flow
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginFlowState {
    /// No login flow active
    Idle,
    /// OAuth flow in progress - waiting for browser
    AwaitingBrowserAuth,
    /// External CLI login in progress - spawned PTY process
    AwaitingExternalCli {
        /// The agent name for display purposes
        agent_name: String,
    },
    /// Login successful
    Success,
    /// Login cancelled by user
    Cancelled,
}

/// Result of checking agent support for login
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentLoginSupport {
    /// Agent supports in-app login
    Supported {
        agent: AgentKind,
        is_installed: bool,
        login_method: LoginMethod,
    },
    /// Unknown agent
    Unknown { agent_name: String },
}

/// Handler for the /login command flow.
///
/// Manages the OAuth authentication state and provides a shutdown handle
/// for cancelling the login server.
pub struct LoginHandler {
    /// Current state of the login flow
    state: LoginFlowState,
    /// Shutdown handle for cancelling OAuth flow
    shutdown_handle: Option<ShutdownHandle>,
    /// Task handle for cancelling external CLI PTY process
    pty_task_handle: Option<JoinHandle<()>>,
    /// Input channel for the active external CLI PTY process
    pty_input_tx: Option<mpsc::Sender<Vec<u8>>>,
}

impl LoginHandler {
    /// Create a new login handler
    pub fn new() -> Self {
        Self {
            state: LoginFlowState::Idle,
            shutdown_handle: None,
            pty_task_handle: None,
            pty_input_tx: None,
        }
    }

    /// Check if an agent supports in-app login
    pub fn check_agent_support(agent_name: &str) -> AgentLoginSupport {
        let normalized = agent_name.to_lowercase();

        // Try to find the agent in the registry
        let agents = list_available_agents();
        let agent_info = agents
            .into_iter()
            .find(|a| a.agent_name.to_lowercase() == normalized);

        match agent_info {
            Some(info) => {
                match info.agent {
                    // Codex supports in-app login via OAuth browser flow
                    AgentKind::Codex => AgentLoginSupport::Supported {
                        agent: AgentKind::Codex,
                        is_installed: info.is_installed,
                        login_method: LoginMethod::OAuthBrowser,
                    },
                    // Gemini supports in-app login via external CLI passthrough
                    AgentKind::Gemini => AgentLoginSupport::Supported {
                        agent: AgentKind::Gemini,
                        is_installed: info.is_installed,
                        login_method: LoginMethod::ExternalCli {
                            command: "gemini".to_string(),
                            args: vec!["login".to_string()],
                        },
                    },
                    AgentKind::ClaudeCode => AgentLoginSupport::Supported {
                        agent: AgentKind::ClaudeCode,
                        is_installed: info.is_installed,
                        login_method: LoginMethod::ExternalCli {
                            command: "claude".to_string(),
                            args: vec![
                                "auth".to_string(),
                                "login".to_string(),
                                "--claudeai".to_string(),
                            ],
                        },
                    },
                    AgentKind::OpenCode => AgentLoginSupport::Supported {
                        agent: AgentKind::OpenCode,
                        is_installed: info.is_installed,
                        login_method: LoginMethod::ExternalCli {
                            command: "opencode".to_string(),
                            args: vec!["auth".to_string(), "login".to_string()],
                        },
                    },
                    AgentKind::Antigravity => AgentLoginSupport::Supported {
                        agent: AgentKind::Antigravity,
                        is_installed: info.is_installed,
                        // The official CLI signs in through its ACP entry point; it may live
                        // only in Nori's side-by-side install directory, not on PATH.
                        login_method: LoginMethod::ExternalCli {
                            command: nori_acp_host::antigravity_cli::discover().map_or_else(
                                |_| "agy".to_string(),
                                |install| install.path.to_string_lossy().into_owned(),
                            ),
                            args: vec!["acp".to_string(), "--login".to_string()],
                        },
                    },
                }
            }
            None => AgentLoginSupport::Unknown {
                agent_name: agent_name.to_string(),
            },
        }
    }

    /// Start the OAuth flow
    pub fn start_oauth(&mut self) {
        self.state = LoginFlowState::AwaitingBrowserAuth;
    }

    /// Start the external CLI login flow
    pub fn start_external_cli(&mut self, agent_name: String) {
        self.state = LoginFlowState::AwaitingExternalCli { agent_name };
    }

    /// Set the shutdown handle for cancellation
    pub fn set_shutdown_handle(&mut self, handle: ShutdownHandle) {
        self.shutdown_handle = Some(handle);
    }

    /// Set the PTY task handle for cancellation
    pub fn set_pty_task_handle(&mut self, handle: JoinHandle<()>) {
        self.pty_task_handle = Some(handle);
    }

    /// Set the input channel for the external CLI PTY process.
    pub fn set_pty_input_sender(&mut self, sender: mpsc::Sender<Vec<u8>>) {
        self.pty_input_tx = Some(sender);
    }

    /// Whether this handler currently owns an external CLI login process.
    pub fn has_external_cli_task(&self) -> bool {
        self.pty_task_handle.is_some()
    }

    /// Forward terminal input to the external CLI when a login process is active.
    pub fn send_pty_input(&self, bytes: Vec<u8>) -> bool {
        let Some(sender) = &self.pty_input_tx else {
            return false;
        };
        match sender.try_send(bytes) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!("external CLI login input queue is full; dropping input");
                true
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// OAuth login completed successfully
    pub fn oauth_complete(&mut self) {
        self.state = LoginFlowState::Success;
    }

    /// Cancel the login flow
    pub fn cancel(&mut self) {
        // Shutdown OAuth server if running
        if let Some(handle) = self.shutdown_handle.take() {
            handle.shutdown();
        }
        // Abort PTY task if running (this will drop the session and kill the process)
        if let Some(handle) = self.pty_task_handle.take() {
            handle.abort();
        }
        self.pty_input_tx = None;
        self.state = LoginFlowState::Cancelled;
    }
}

impl Default for LoginHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_agent_support_returns_supported_for_codex_with_oauth() {
        let support = LoginHandler::check_agent_support("codex");

        match support {
            AgentLoginSupport::Supported {
                agent,
                login_method,
                ..
            } => {
                assert_eq!(agent, AgentKind::Codex);
                assert_eq!(login_method, LoginMethod::OAuthBrowser);
            }
            _ => panic!("Expected Supported variant for codex"),
        }
    }

    #[test]
    fn check_agent_support_returns_supported_for_claude_cli_login() {
        let support = LoginHandler::check_agent_support("claude-code");

        let AgentLoginSupport::Supported {
            agent,
            login_method,
            ..
        } = support
        else {
            panic!("Expected in-app CLI login support for claude-code");
        };

        assert_eq!(agent, AgentKind::ClaudeCode);
        assert_eq!(
            login_method,
            LoginMethod::ExternalCli {
                command: "claude".to_string(),
                args: vec![
                    "auth".to_string(),
                    "login".to_string(),
                    "--claudeai".to_string()
                ],
            }
        );
    }

    #[test]
    fn check_agent_support_returns_supported_for_gemini_with_external_cli() {
        // Gemini supports in-app login via external CLI passthrough
        let support = LoginHandler::check_agent_support("gemini");

        match support {
            AgentLoginSupport::Supported {
                agent,
                login_method,
                ..
            } => {
                assert_eq!(agent, AgentKind::Gemini);
                match login_method {
                    LoginMethod::ExternalCli { command, args } => {
                        assert_eq!(command, "gemini");
                        assert_eq!(args, vec!["login".to_string()]);
                    }
                    _ => panic!("Expected ExternalCli login method for gemini"),
                }
            }
            _ => panic!("Expected Supported variant for gemini"),
        }
    }

    #[test]
    fn start_external_cli_transitions_to_awaiting_external_cli() {
        let mut handler = LoginHandler::new();

        handler.start_external_cli("Gemini".to_string());

        match handler.state {
            LoginFlowState::AwaitingExternalCli { agent_name } => {
                assert_eq!(agent_name, "Gemini");
            }
            _ => panic!("Expected AwaitingExternalCli state"),
        }
    }

    #[test]
    fn check_agent_support_returns_unknown_for_invalid_agent() {
        let support = LoginHandler::check_agent_support("unknown-agent");

        match support {
            AgentLoginSupport::Unknown { agent_name } => {
                assert_eq!(agent_name, "unknown-agent");
            }
            _ => panic!("Expected Unknown variant for unknown-agent"),
        }
    }

    #[test]
    fn start_oauth_transitions_to_awaiting_browser_auth() {
        let mut handler = LoginHandler::new();

        handler.start_oauth();

        assert_eq!(handler.state, LoginFlowState::AwaitingBrowserAuth);
    }

    #[test]
    fn oauth_complete_transitions_to_success() {
        let mut handler = LoginHandler::new();
        handler.state = LoginFlowState::AwaitingBrowserAuth;

        handler.oauth_complete();

        assert_eq!(handler.state, LoginFlowState::Success);
    }

    #[test]
    fn cancel_transitions_to_cancelled_state() {
        let mut handler = LoginHandler::new();
        handler.state = LoginFlowState::AwaitingBrowserAuth;

        handler.cancel();

        assert_eq!(handler.state, LoginFlowState::Cancelled);
    }
}

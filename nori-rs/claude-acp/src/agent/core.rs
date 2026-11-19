//! The agent's shared state.

use std::sync::Arc;

use crate::session::SessionManager;
use crate::types::AgentConfig;

/// The Claude Code agent: its configuration and the sessions it holds. One per connection.
#[derive(Debug)]
pub struct ClaudeAcpAgent {
    pub(super) config: AgentConfig,
    pub(super) sessions: Arc<SessionManager>,
    /// Whether this connection has been told who the account is.
    pub(super) identity_announced: std::sync::atomic::AtomicBool,
}

impl ClaudeAcpAgent {
    pub fn new(config: AgentConfig) -> Self {
        Self {
            config,
            sessions: Arc::new(SessionManager::new()),
            identity_announced: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

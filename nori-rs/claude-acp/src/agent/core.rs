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
    /// Whether the client is Nori's own harness, the only client that knows Nori's extensions
    /// (`_auth/status_update`). Another ACP client is never sent them.
    pub(super) nori_host: std::sync::atomic::AtomicBool,
}

impl ClaudeAcpAgent {
    pub fn new(config: AgentConfig) -> Self {
        Self {
            config,
            sessions: Arc::new(SessionManager::new()),
            identity_announced: std::sync::atomic::AtomicBool::new(false),
            nori_host: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

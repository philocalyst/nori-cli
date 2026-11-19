//! Session manager for tracking active sessions
//!
//! Uses DashMap for concurrent access with entry API to avoid deadlocks.

use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashMap;
use tracing::instrument;

use crate::mcp::SessionTools;
use crate::types::AgentConfig;
use crate::types::AgentError;
use crate::types::NewSessionMeta;
use crate::types::Result;

use super::session::Session;

/// Manager for active sessions
///
/// Provides thread-safe session storage and lookup using DashMap.
/// Uses entry API for atomic operations to prevent deadlocks.
#[derive(Debug, Default)]
pub struct SessionManager {
    /// Active sessions keyed by session_id
    sessions: DashMap<String, Arc<Session>>,
}

impl SessionManager {
    /// Create a new session manager
    pub fn new() -> Self {
        Self {
            sessions: DashMap::new(),
        }
    }

    /// Create a new session and store it
    ///
    /// # Arguments
    ///
    /// * `session_id` - Unique identifier for the session
    /// * `cwd` - Working directory for the session
    /// * `config` - Agent configuration
    /// * `meta` - Optional session metadata
    ///
    /// # Returns
    ///
    /// Arc reference to the created session
    pub fn create_session(
        &self,
        session_id: String,
        cwd: PathBuf,
        config: &AgentConfig,
        meta: Option<&NewSessionMeta>,
        tools: &SessionTools,
    ) -> Result<Arc<Session>> {
        // Use entry API to atomically check and insert
        let entry = self.sessions.entry(session_id.clone());

        match entry {
            dashmap::Entry::Occupied(_) => {
                // Session already exists
                Err(AgentError::internal(format!(
                    "Session already exists: {session_id}"
                )))
            }
            dashmap::Entry::Vacant(vacant) => {
                // Session::new() now directly returns Arc<Session>
                let arc_session = Session::new(session_id, cwd, config, meta, tools)?;
                vacant.insert(Arc::clone(&arc_session));
                Ok(arc_session)
            }
        }
    }

    /// Take in a session made elsewhere, such as one started ahead of time.
    pub fn adopt(&self, session: Arc<Session>) -> Result<()> {
        match self.sessions.entry(session.session_id.clone()) {
            dashmap::Entry::Occupied(_) => Err(AgentError::internal(format!(
                "Session already exists: {}",
                session.session_id
            ))),
            dashmap::Entry::Vacant(vacant) => {
                vacant.insert(session);
                Ok(())
            }
        }
    }

    /// Get an existing session
    pub fn get_session(&self, session_id: &str) -> Option<Arc<Session>> {
        self.sessions.get(session_id).map(|r| Arc::clone(&r))
    }

    /// Get an existing session or return SessionNotFound error
    pub fn get_session_or_error(&self, session_id: &str) -> Result<Arc<Session>> {
        self.get_session(session_id)
            .ok_or_else(|| AgentError::SessionNotFound(session_id.to_string()))
    }

    /// Remove a session
    pub fn remove_session(&self, session_id: &str) -> Option<Arc<Session>> {
        self.sessions.remove(session_id).map(|(_, v)| v)
    }

    /// Remove a session and cleanup its resources
    ///
    /// This properly cleans up all child processes (MCP servers, bash processes)
    /// to prevent zombie processes.
    #[instrument(
        name = "manager_remove_and_cleanup",
        skip(self),
        fields(session_id = %session_id)
    )]
    pub async fn remove_and_cleanup(&self, session_id: &str) -> Result<()> {
        if let Some(session) = self.remove_session(session_id) {
            session.cleanup().await?;
        }
        Ok(())
    }

    /// Get all session IDs
    pub fn session_ids(&self) -> Vec<String> {
        self.sessions.iter().map(|r| r.key().clone()).collect()
    }

    /// Clear all sessions with cleanup
    ///
    /// This properly cleans up all child processes for all sessions
    /// to prevent zombie processes.
    #[instrument(name = "manager_clear", skip(self))]
    pub async fn clear(&self) {
        let session_ids = self.session_ids();
        for session_id in session_ids {
            drop(self.remove_and_cleanup(&session_id).await);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> AgentConfig {
        AgentConfig {
            cli_path: None,
            environment: Default::default(),
            base_url: None,
            api_key: None,
            model: None,
            small_fast_model: None,
            max_thinking_tokens: None,
        }
    }

    #[test]
    fn test_manager_get_session() {
        let manager = SessionManager::new();
        let config = test_config();

        manager
            .create_session(
                "session-1".to_string(),
                PathBuf::from("/tmp"),
                &config,
                None,
                &SessionTools::default(),
            )
            .unwrap();

        let session = manager.get_session("session-1");
        assert!(session.is_some());
        assert_eq!(session.unwrap().session_id, "session-1");

        let missing = manager.get_session("nonexistent");
        assert!(missing.is_none());
    }

    #[test]
    fn test_manager_get_session_or_error() {
        let manager = SessionManager::new();
        let config = test_config();

        manager
            .create_session(
                "session-1".to_string(),
                PathBuf::from("/tmp"),
                &config,
                None,
                &SessionTools::default(),
            )
            .unwrap();

        let result = manager.get_session_or_error("session-1");
        assert!(result.is_ok());

        let error = manager.get_session_or_error("nonexistent");
        assert!(matches!(error, Err(AgentError::SessionNotFound(_))));
    }

    #[test]
    fn test_manager_session_ids() {
        let manager = SessionManager::new();
        let config = test_config();

        manager
            .create_session(
                "session-1".to_string(),
                PathBuf::from("/tmp"),
                &config,
                None,
                &SessionTools::default(),
            )
            .unwrap();
        manager
            .create_session(
                "session-2".to_string(),
                PathBuf::from("/tmp"),
                &config,
                None,
                &SessionTools::default(),
            )
            .unwrap();

        let ids = manager.session_ids();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"session-1".to_string()));
        assert!(ids.contains(&"session-2".to_string()));
    }

    /// Test remove_and_cleanup() properly cleans up sessions
    ///
    /// Verifies that remove_and_cleanup() removes the session
    /// and calls its cleanup method.
    /// Test remove_and_cleanup() with non-existent session
    ///
    /// Verifies that remove_and_cleanup() handles non-existent
    /// sessions gracefully without error.
    #[tokio::test]
    async fn test_manager_remove_and_cleanup_nonexistent() {
        let manager = SessionManager::new();

        // Removing non-existent session should succeed (idempotent)
        let result = manager.remove_and_cleanup("nonexistent-session").await;
        assert!(result.is_ok(), "Removing non-existent session should be OK");
    }
}

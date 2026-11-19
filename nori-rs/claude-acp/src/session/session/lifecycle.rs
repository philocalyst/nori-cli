//! Connecting a session to its Claude CLI, disconnecting, and cleaning up what it started.

use std::sync::atomic::Ordering;
use std::time::Instant;

use tracing::instrument;

use super::*;
use crate::session::BackgroundTerminal;

impl Session {
    pub async fn connect(&self) -> Result<()> {
        if self.connected.load(Ordering::SeqCst) {
            tracing::debug!(
                session_id = %self.session_id,
                "Already connected to Claude CLI"
            );
            return Ok(());
        }

        let start_time = Instant::now();
        tracing::info!(
            session_id = %self.session_id,
            cwd = ?self.cwd,
            "Connecting to Claude CLI..."
        );

        let mut client = self.client.write().await;
        // Another caller may have connected while this one waited for the lock.
        if self.connected.load(Ordering::SeqCst) {
            return Ok(());
        }
        client.connect().await.map_err(|e| {
            let agent_error = crate::types::from_sdk(e);
            tracing::error!(session_id = %self.session_id, error = %agent_error, "Failed to connect to Claude CLI");
            agent_error
        })?;

        self.connected.store(true, Ordering::SeqCst);

        let elapsed = start_time.elapsed();
        tracing::info!(
            session_id = %self.session_id,
            elapsed_ms = elapsed.as_millis(),
            "Successfully connected to Claude CLI"
        );

        Ok(())
    }

    /// Disconnect from Claude CLI
    #[instrument(
        name = "session_disconnect",
        skip(self),
        fields(session_id = %self.session_id)
    )]
    pub async fn disconnect(&self) -> Result<()> {
        if !self.connected.load(Ordering::SeqCst) {
            tracing::debug!(
                session_id = %self.session_id,
                "Already disconnected from Claude CLI"
            );
            return Ok(());
        }

        let start_time = Instant::now();
        tracing::info!(
            session_id = %self.session_id,
            "Disconnecting from Claude CLI..."
        );

        let mut client = self.client.write().await;
        client.disconnect().await.map_err(|e| {
            let agent_error = crate::types::from_sdk(e);
            tracing::error!(session_id = %self.session_id, error = %agent_error, "Failed to disconnect from Claude CLI");
            agent_error
        })?;

        self.connected.store(false, Ordering::SeqCst);

        let elapsed = start_time.elapsed();
        tracing::info!(
            session_id = %self.session_id,
            elapsed_ms = elapsed.as_millis(),
            "Disconnected from Claude CLI"
        );

        Ok(())
    }
    /// Cleanup all child processes for this session
    ///
    /// This method ensures all external MCP servers and background bash processes
    /// are properly terminated and reaped to prevent zombie processes.
    #[instrument(
        name = "session_cleanup",
        skip(self),
        fields(session_id = %self.session_id)
    )]
    pub async fn cleanup(&self) -> Result<()> {
        let start_time = Instant::now();

        tracing::info!(
            session_id = %self.session_id,
            "Starting session cleanup"
        );

        // Kill all background bash processes
        let shell_ids = self.background_processes.shell_ids();
        let shell_count = shell_ids.len();
        let mut shell_success = 0;
        let mut shell_errors = 0;

        for shell_id in &shell_ids {
            if let Some((_, BackgroundTerminal::Running { mut child, .. })) =
                self.background_processes.remove(shell_id)
            {
                match child.kill().await {
                    Ok(()) => {
                        // Wait for the process to exit (prevents zombie)
                        drop(child.wait().await);
                        shell_success += 1;
                    }
                    Err(e) => {
                        shell_errors += 1;
                        tracing::warn!(
                            session_id = %self.session_id,
                            shell_id = %shell_id,
                            error = %e,
                            "Failed to kill background process during cleanup"
                        );
                    }
                }
            }
        }

        tracing::info!(
            session_id = %self.session_id,
            shell_count = shell_count,
            shell_success = shell_success,
            shell_errors = shell_errors,
            "Background processes cleanup completed"
        );

        // 3. Stop the CLI. Closing its input lets it shut down the servers it started; whatever
        // outlives it (a tool's command, a dev server) is stopped after. The tree is read first,
        // because once the CLI is gone its orphans can no longer be traced back to it.
        let session_id = self.session_id.clone();
        let tree = tokio::task::spawn_blocking(move || {
            crate::utils::descendants::session_tree(&session_id)
        })
        .await
        .unwrap_or_default();
        if tokio::time::timeout(CLI_EXIT_GRACE, self.disconnect())
            .await
            .is_err()
        {
            tracing::debug!(session_id = %self.session_id, "The Claude CLI did not exit in time");
        }
        crate::utils::descendants::terminate(&tree, std::time::Duration::from_millis(300)).await;

        let elapsed = start_time.elapsed();
        tracing::info!(
            session_id = %self.session_id,
            elapsed_ms = elapsed.as_millis(),
            "Session cleanup completed"
        );

        Ok(())
    }
}

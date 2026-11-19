//! Background process management for terminal commands
//!
//! Manages background terminal processes that are started with `run_in_background=true`.
//! Supports retrieving incremental output and killing running processes.

use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use dashmap::DashMap;
use tokio::process::Child;
use tokio::sync::Mutex;

use crate::session::wrapped_child::WrappedChild;

/// Child process handle that can be either wrapped or unwrapped
///
/// This enum allows us to support both:
/// - Legacy tokio::process::Child (no process group)
/// - WrappedChild with process group support (process-wrap)
///
/// Note: Clone implementation creates a new handle that shares the same child
/// but does NOT clone stdout/stderr (those are taken by the first user).
#[derive(Debug)]
pub enum ChildHandle {
    /// Unwrapped child (legacy, no process group support)
    Unwrapped {
        /// The child process
        child: Arc<Mutex<Child>>,
    },
    /// Wrapped child with process group support (via process-wrap)
    Wrapped {
        /// The wrapped child
        child: Arc<Mutex<WrappedChild>>,
    },
}

impl Clone for ChildHandle {
    fn clone(&self) -> Self {
        match self {
            Self::Unwrapped { child } => Self::Unwrapped {
                child: Arc::clone(child),
            },
            Self::Wrapped { child } => Self::Wrapped {
                child: Arc::clone(child),
            },
        }
    }
}

impl ChildHandle {
    /// Kill the process (and process group if wrapped)
    pub async fn kill(&mut self) -> io::Result<()> {
        match self {
            Self::Unwrapped { child } => {
                let mut guard = child.lock().await;
                guard.kill().await
            }
            Self::Wrapped { child } => {
                let mut guard = child.lock().await;
                guard.kill().await
            }
        }
    }

    /// Wait for the process to exit
    pub async fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        match self {
            Self::Unwrapped { child } => {
                let mut guard = child.lock().await;
                guard.wait().await
            }
            Self::Wrapped { child } => {
                let mut guard = child.lock().await;
                guard.wait().await
            }
        }
    }
}

/// Terminal exit status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalExitStatus {
    /// Process exited normally with exit code
    Exited(i32),
    /// Process was killed by user
    Killed,
    /// Process was aborted
    Aborted,
}

impl TerminalExitStatus {
    /// Get status string for API response
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exited(_) => "exited",
            Self::Killed => "killed",
            Self::Aborted => "aborted",
        }
    }
}

/// Background terminal state
#[derive(Debug)]
pub enum BackgroundTerminal {
    /// Terminal is still running
    Running {
        /// The child process handle (wrapped or unwrapped)
        child: ChildHandle,
        /// Accumulated output buffer
        output_buffer: Arc<Mutex<String>>,
        /// Last read offset for incremental output
        /// Using AtomicUsize for lock-free atomic operations
        last_read_offset: Arc<AtomicUsize>,
    },
    /// Terminal has finished
    Finished {
        /// Exit status
        status: TerminalExitStatus,
        /// Final output
        final_output: String,
    },
}

impl BackgroundTerminal {
    /// Create a new running terminal with a child handle
    pub fn new_running(child: ChildHandle) -> Self {
        Self::Running {
            child,
            output_buffer: Arc::new(Mutex::new(String::new())),
            last_read_offset: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Get the status string
    pub fn status_str(&self) -> &'static str {
        match self {
            Self::Running { .. } => "running",
            Self::Finished { status, .. } => status.as_str(),
        }
    }

    /// Get incremental output since last read
    pub async fn get_incremental_output(&self) -> String {
        match self {
            Self::Running {
                output_buffer,
                last_read_offset,
                ..
            } => {
                // Use atomic load to get current offset (lock-free)
                let current_offset = last_read_offset.load(Ordering::Acquire);

                let buffer = output_buffer.lock().await;
                let new_output = buffer[current_offset..].to_string();
                let new_len = buffer.len();
                drop(buffer);

                // Update offset using atomic store (lock-free)
                last_read_offset.store(new_len, Ordering::Release);

                new_output
            }
            Self::Finished { final_output, .. } => final_output.clone(),
        }
    }

    /// Transition to finished state
    pub async fn finish(self, status: TerminalExitStatus) -> Self {
        match self {
            Self::Running { output_buffer, .. } => {
                let final_output = output_buffer.lock().await.clone();
                Self::Finished {
                    status,
                    final_output,
                }
            }
            finished @ Self::Finished { .. } => finished,
        }
    }
}

/// Manager for background terminal processes
#[derive(Debug, Default)]
pub struct BackgroundProcessManager {
    /// Map of shell ID to background terminal
    terminals: DashMap<String, BackgroundTerminal>,
}

impl BackgroundProcessManager {
    /// Create a new background process manager
    pub fn new() -> Self {
        Self {
            terminals: DashMap::new(),
        }
    }

    /// Register a new background terminal
    pub fn register(&self, shell_id: String, terminal: BackgroundTerminal) {
        self.terminals.insert(shell_id, terminal);
    }

    /// Get terminal by ID (returns reference for reading)
    pub fn get(
        &self,
        shell_id: &str,
    ) -> Option<dashmap::mapref::one::Ref<'_, String, BackgroundTerminal>> {
        self.terminals.get(shell_id)
    }

    /// Remove terminal by ID
    pub fn remove(&self, shell_id: &str) -> Option<(String, BackgroundTerminal)> {
        self.terminals.remove(shell_id)
    }

    /// Update a terminal to finished state
    pub async fn finish_terminal(&self, shell_id: &str, status: TerminalExitStatus) {
        if let Some((id, terminal)) = self.terminals.remove(shell_id) {
            let finished = terminal.finish(status).await;
            self.terminals.insert(id, finished);
        }
    }

    /// Get all shell IDs
    pub fn shell_ids(&self) -> Vec<String> {
        self.terminals.iter().map(|r| r.key().clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_terminal_exit_status() {
        assert_eq!(TerminalExitStatus::Exited(0).as_str(), "exited");
        assert_eq!(TerminalExitStatus::Killed.as_str(), "killed");
        assert_eq!(TerminalExitStatus::Aborted.as_str(), "aborted");
    }
}

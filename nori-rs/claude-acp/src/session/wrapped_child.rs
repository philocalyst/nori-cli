//! Wrapped child process with process group support
//!
//! Provides a unified interface over process-wrap's ChildWrapper
//! while maintaining compatibility with existing Arc<Mutex<>> pattern.

use process_wrap::tokio::ChildWrapper;
use std::io;
use std::pin::Pin;

/// Wrapper around Box<dyn ChildWrapper> that provides
/// a stable interface compatible with Arc<Mutex<>>
///
/// This wraps the process-wrap ChildWrapper trait to work
/// with our existing Arc<Mutex<>> storage pattern in BackgroundTerminal.
#[derive(Debug)]
pub struct WrappedChild {
    inner: Box<dyn ChildWrapper>,
}

impl WrappedChild {
    /// Create a new wrapped child from a process-wrap ChildWrapper
    pub fn new(inner: Box<dyn ChildWrapper>) -> Self {
        Self { inner }
    }

    /// Kill the process group and wait for exit
    ///
    /// This will terminate the entire process group, not just the parent process.
    pub async fn kill(&mut self) -> io::Result<()> {
        Pin::from(self.inner.kill()).await
    }

    /// Wait for the process to exit
    pub async fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        self.inner.wait().await
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_wrapped_child_creation() {
        // This is a placeholder test
        // Real tests would require spawning an actual process
        // which is better done as integration tests
    }
}

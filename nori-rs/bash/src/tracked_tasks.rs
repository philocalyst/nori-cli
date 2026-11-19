//! A task spawner that can stop what the shell started in the background.
//!
//! `cmd &`, `( ... ) &` and `{ ...; } &` run as tasks inside this process. The default spawner
//! gives no way to stop them, so a command that timed out or was cancelled would leave its
//! background work running after the tool reported it stopped. This spawner remembers every task
//! so the session can abort them; dropping a task's future kills the external process it owns.

use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use brush_core::error::ErrorKind;
use brush_core::extensions::DefaultErrorFormatter;
use brush_core::extensions::DefaultProcessLauncher;
use brush_core::extensions::DefaultTaskSpawner;
use brush_core::extensions::ShellExtensionsImpl;
use brush_core::extensions::TaskSpawner;
use brush_core::tasks::TaskHandle;
use tokio::task::AbortHandle;

/// The extensions of every session's shell.
pub(crate) type SessionExtensions =
    ShellExtensionsImpl<DefaultErrorFormatter, DefaultProcessLauncher, TrackedSpawner>;

/// The shell type of a session.
pub(crate) type SessionShell = brush_core::Shell<SessionExtensions>;

/// Spawns on tokio and keeps an abort handle for every task.
#[derive(Clone, Default)]
pub(crate) struct TrackedSpawner {
    tasks: Arc<Mutex<Vec<AbortHandle>>>,
}

impl TrackedSpawner {
    /// Stop every task still running.
    pub(crate) fn abort_all(&self) {
        let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
        for task in tasks.drain(..) {
            task.abort();
        }
    }
}

impl TaskSpawner for TrackedSpawner {
    fn spawn<F>(&self, future: F) -> TaskHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let join = tokio::spawn(future);
        {
            let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
            tasks.retain(|task| !task.is_finished());
            tasks.push(join.abort_handle());
        }
        TaskHandle::from_future(async move {
            join.await
                .map_err(|error| ErrorKind::TaskError(Box::new(error)).into())
        })
    }

    fn spawn_blocking<F>(
        &self,
        future: F,
    ) -> impl Future<Output = TaskHandle<F::Output>> + Send + use<F>
    where
        F: Future + Send + 'static,
        F::Output: Send + Sync + 'static,
    {
        DefaultTaskSpawner.spawn_blocking(future)
    }
}

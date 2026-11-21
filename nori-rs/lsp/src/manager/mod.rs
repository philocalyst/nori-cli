//! [`LspManager`]: the one handle the rest of Nori holds. Servers start lazily the first time a
//! file of their type is touched, documents are kept in sync, diagnostics land in a shared store,
//! and everything shuts down when the last clone is dropped.
//!
//! Every public method is async, bounded by the timings in [`Timing`] and cancel-safe: dropping
//! the future abandons the wait and nothing else.

mod actor;
mod requests;

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::mpsc::unbounded_channel;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::time::Instant;

use self::actor::Actor;
use self::actor::Command;
use self::actor::DocStore;
use self::actor::SlotView;
use self::actor::SlotWatch;
use self::actor::Want;
pub use self::requests::Answer;
use crate::config::LspConfig;
use crate::config::Timing;
use crate::diagnostics::DiagnosticStore;
use crate::diagnostics::DiagnosticsReport;
use crate::diagnostics::FileDiagnostics;
use crate::diagnostics::ProjectBaseline;
use crate::diagnostics::ReportStatus;
use crate::diagnostics::Severity;
use crate::error::LspError;
use crate::error::LspResult;
use crate::launcher::ProcessLauncher;
use crate::launcher::ServerLauncher;
use crate::registry::Registry;
use crate::registry::find_root;
use crate::registry::language_for_path;
use crate::server::DocSync;
use crate::server::ServerHandle;
use crate::types::ServerStatus;

/// What was known about the rest of the project when a change was announced.
#[derive(Clone)]
struct ChangeMark {
    publish_seq: i64,
    baseline: ProjectBaseline,
}

struct Inner {
    commands: UnboundedSender<Command>,
    store: DiagnosticStore,
    docs: DocStore,
    marks: Mutex<std::collections::HashMap<PathBuf, ChangeMark>>,
    registry: Registry,
    launcher: Arc<dyn ServerLauncher>,
    timing: Timing,
    workspace_root: PathBuf,
    enabled: bool,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // The actor shuts every server down and ends.
        let _ = self.commands.send(Command::Shutdown { reply: None });
    }
}

/// A language server resolved for one file.
pub(crate) struct Resolved {
    pub path: PathBuf,
    pub language: &'static str,
    pub handles: Vec<Arc<ServerHandle>>,
}

/// Cheap to clone; all clones share the servers.
#[derive(Clone)]
pub struct LspManager {
    inner: Arc<Inner>,
}

impl LspManager {
    /// The process's manager for `workspace_root`, with the default configuration and servers
    /// found on `PATH`: every part of Nori asking for the same root (the agents' `lsp_*` tools,
    /// the prompt editor) shares its language servers instead of starting its own. Must be
    /// called inside a tokio runtime the first time a root is asked for.
    pub fn shared(workspace_root: impl Into<PathBuf>) -> Self {
        static SHARED: std::sync::OnceLock<
            std::sync::Mutex<std::collections::HashMap<PathBuf, LspManager>>,
        > = std::sync::OnceLock::new();
        let root = workspace_root.into();
        SHARED
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(root.clone())
            .or_insert_with(|| LspManager::new(root, LspConfig::default()))
            .clone()
    }

    /// Starts the manager for a workspace, running servers as child processes found on `PATH`.
    /// Must be called inside a tokio runtime; it spawns the actor and starts no server.
    pub fn new(workspace_root: impl Into<PathBuf>, config: LspConfig) -> Self {
        Self::with_launcher(workspace_root, config, Arc::new(ProcessLauncher))
    }

    /// Like [`LspManager::new`], with servers started by `launcher`: a remote machine's shell,
    /// or an in-process fake in tests.
    pub fn with_launcher(
        workspace_root: impl Into<PathBuf>,
        config: LspConfig,
        launcher: Arc<dyn ServerLauncher>,
    ) -> Self {
        let workspace_root = workspace_root.into();
        // One realpath at startup so every path below agrees with what servers report.
        let workspace_root = std::fs::canonicalize(&workspace_root).unwrap_or(workspace_root);
        let (commands, mailbox) = unbounded_channel();
        let store = DiagnosticStore::default();
        let docs = DocStore::default();
        let actor = Actor::new(
            mailbox,
            commands.clone(),
            Arc::clone(&launcher),
            config.timing.clone(),
            config.max_restarts,
            store.clone(),
            Arc::clone(&docs),
        );
        tokio::spawn(actor.run());
        Self {
            inner: Arc::new(Inner {
                commands,
                store,
                docs,
                marks: Mutex::new(std::collections::HashMap::new()),
                registry: Registry::with_overrides(config.servers),
                launcher,
                timing: config.timing,
                workspace_root,
                enabled: config.enabled,
            }),
        }
    }

    pub fn workspace_root(&self) -> &Path {
        &self.inner.workspace_root
    }

    pub fn registry(&self) -> &Registry {
        &self.inner.registry
    }

    /// A counter that grows whenever any server publishes diagnostics. The UI subscribes and
    /// re-reads [`LspManager::diagnostics`] on change.
    pub fn subscribe_diagnostics(&self) -> watch::Receiver<i64> {
        self.inner.store.subscribe()
    }

    /// The last diagnostics published for a file, errors first. Never waits.
    pub fn diagnostics(&self, path: &Path) -> Vec<crate::diagnostics::Diagnostic> {
        self.inner.store.file(&self.absolute(path))
    }

    /// Every file that currently has errors. Never waits.
    pub fn project_errors(&self) -> Vec<FileDiagnostics> {
        self.inner.store.files_with(Severity::Error)
    }

    fn absolute(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.inner.workspace_root.join(path)
        }
    }

    /// Servers and their state, for the status line and `lsp_diagnostics`.
    pub async fn status(&self) -> Vec<ServerStatus> {
        let (reply, answer) = oneshot::channel();
        if self.inner.commands.send(Command::Status { reply }).is_err() {
            return Vec::new();
        }
        answer.await.unwrap_or_default()
    }

    /// Restarts the named server (every server when `None`) with a fresh restart budget and
    /// returns how many were restarted.
    pub async fn restart(&self, name: Option<&str>) -> i32 {
        let (reply, answer) = oneshot::channel();
        let name = name.map(ToOwned::to_owned);
        if self
            .inner
            .commands
            .send(Command::Restart { name, reply })
            .is_err()
        {
            return 0;
        }
        answer.await.unwrap_or(0)
    }

    /// Stops every server and waits for them (bounded by [`Timing::shutdown`]). The manager
    /// answers every later call with [`LspError::ShutDown`].
    pub async fn shutdown(&self) {
        let (reply, answer) = oneshot::channel();
        if self
            .inner
            .commands
            .send(Command::Shutdown { reply: Some(reply) })
            .is_ok()
        {
            let _ = answer.await;
        }
    }

    /// Finds the server for `path` and waits (at most [`Timing::ready_wait`]) for it to be ready,
    /// starting it if this is the first file of its kind.
    pub(crate) async fn resolve(&self, path: &Path) -> LspResult<Resolved> {
        if !self.inner.enabled {
            return Err(LspError::NoServer {
                path: path.to_path_buf(),
                hint: " (language servers are turned off)".to_owned(),
            });
        }
        let path = self.absolute(path);
        let registry = self.inner.registry.clone();
        let launcher = Arc::clone(&self.inner.launcher);
        let workspace = self.inner.workspace_root.clone();
        let detected =
            tokio::task::spawn_blocking(move || detect(&registry, &*launcher, &workspace, &path))
                .await
                .map_err(|_| LspError::ShutDown)??;

        let (reply, answer) = oneshot::channel();
        self.inner
            .commands
            .send(Command::Ensure {
                wants: detected.wants,
                reply,
            })
            .map_err(|_| LspError::ShutDown)?;
        let watches = answer.await.map_err(|_| LspError::ShutDown)??;
        let mut handles = Vec::new();
        for watch in watches {
            handles.push(wait_ready(watch, self.inner.timing.ready_wait).await?);
        }
        Ok(Resolved {
            path: detected.path,
            language: detected.language,
            handles,
        })
    }

    /// Reads `path` from disk, makes sure its servers are ready and have the current text.
    pub(crate) async fn prepare(&self, path: &Path) -> LspResult<requests::Prepared> {
        let resolved = self.resolve(path).await?;
        let text = tokio::fs::read_to_string(&resolved.path)
            .await
            .map_err(|error| LspError::Io {
                path: resolved.path.clone(),
                message: error.to_string(),
            })?;
        let text: Arc<str> = Arc::from(text);
        self.remember(&resolved.path, resolved.language, &text);
        for handle in &resolved.handles {
            handle.sync_document(&resolved.path, resolved.language, &text);
        }
        Ok(requests::Prepared {
            path: resolved.path,
            text,
            handles: resolved.handles,
        })
    }

    fn remember(&self, path: &Path, language: &str, text: &Arc<str>) {
        self.inner
            .docs
            .lock()
            .insert(path.to_path_buf(), (language.to_owned(), Arc::clone(text)));
    }

    /// Announces that `path` was written with `new_text`: opens or updates the document on its
    /// servers and tells them it was saved. Call this after every edit, then
    /// [`LspManager::diagnostics_after_edit`]. An `Err` of [`LspError::NoServer`] only means
    /// there is nothing to tell.
    pub async fn notify_file_changed(&self, path: &Path, new_text: String) -> LspResult<()> {
        let text: Arc<str> = Arc::from(new_text);
        let resolved = self.resolve(path).await?;
        self.inner.marks.lock().insert(
            resolved.path.clone(),
            ChangeMark {
                publish_seq: self.inner.store.last_publish(&resolved.path),
                baseline: self.inner.store.baseline_except(&resolved.path),
            },
        );
        self.remember(&resolved.path, resolved.language, &text);
        for handle in &resolved.handles {
            handle.sync_document(&resolved.path, resolved.language, &text);
            handle.save_document(&resolved.path, &text);
        }
        Ok(())
    }

    /// Tells the servers a file is no longer being worked on.
    pub async fn close_file(&self, path: &Path) {
        let path = self.absolute(path);
        self.inner.docs.lock().remove(&path);
        self.inner.marks.lock().remove(&path);
        if let Ok(resolved) = self.resolve(&path).await {
            for handle in &resolved.handles {
                handle.close_document(&resolved.path);
            }
        }
    }

    /// The diagnostics a model should see after an edit: waits for the first publish following
    /// [`LspManager::notify_file_changed`] (up to [`Timing::first_diagnostics`]) and then for the
    /// server to go quiet ([`Timing::settle`]). Reports the file's diagnostics and, separately,
    /// errors that appeared in other files.
    pub async fn diagnostics_after_edit(&self, path: &Path) -> DiagnosticsReport {
        let resolved = match self.resolve(path).await {
            Ok(resolved) => resolved,
            Err(error) => return self.failed_report(path, &error),
        };
        let mark = self.inner.marks.lock().get(&resolved.path).cloned();
        let mark = mark.unwrap_or_else(|| ChangeMark {
            publish_seq: self.inner.store.last_publish(&resolved.path),
            baseline: self.inner.store.baseline_except(&resolved.path),
        });
        self.wait_and_report(&resolved, &mark).await
    }

    /// Diagnostics of a file as the servers see it on disk now: syncs the file, waits if the
    /// servers had not seen this text, and reports. For `lsp_diagnostics`.
    pub async fn refresh_diagnostics(&self, path: &Path) -> DiagnosticsReport {
        let resolved = match self.resolve(path).await {
            Ok(resolved) => resolved,
            Err(error) => return self.failed_report(path, &error),
        };
        let text = match tokio::fs::read_to_string(&resolved.path).await {
            Ok(text) => Arc::<str>::from(text),
            Err(_) => {
                return self.failed_report(
                    path,
                    &LspError::Io {
                        path: resolved.path.clone(),
                        message: "the file cannot be read".to_owned(),
                    },
                );
            }
        };
        self.remember(&resolved.path, resolved.language, &text);
        let mark = ChangeMark {
            publish_seq: self.inner.store.last_publish(&resolved.path),
            baseline: self.inner.store.baseline_except(&resolved.path),
        };
        let mut changed = false;
        for handle in &resolved.handles {
            let sync = handle.sync_document(&resolved.path, resolved.language, &text);
            if sync != DocSync::Unchanged {
                handle.save_document(&resolved.path, &text);
                changed = true;
            }
        }
        if changed {
            return self.wait_and_report(&resolved, &mark).await;
        }
        self.report(&resolved, &mark, ReportStatus::Fresh)
    }

    async fn wait_and_report(&self, resolved: &Resolved, mark: &ChangeMark) -> DiagnosticsReport {
        let timing = &self.inner.timing;
        let fresh = self
            .inner
            .store
            .wait_for_publish(
                &resolved.path,
                mark.publish_seq,
                timing.first_diagnostics,
                timing.settle,
                timing.settle_cap,
            )
            .await;
        let status = if fresh {
            ReportStatus::Fresh
        } else {
            ReportStatus::Stale
        };
        self.report(resolved, mark, status)
    }

    fn report(
        &self,
        resolved: &Resolved,
        mark: &ChangeMark,
        status: ReportStatus,
    ) -> DiagnosticsReport {
        DiagnosticsReport {
            path: resolved.path.clone(),
            file: self.inner.store.file(&resolved.path),
            project: self
                .inner
                .store
                .new_errors_since(&mark.baseline, &resolved.path),
            servers: resolved
                .handles
                .iter()
                .map(|handle| handle.name().to_owned())
                .collect(),
            status,
        }
    }

    fn failed_report(&self, path: &Path, error: &LspError) -> DiagnosticsReport {
        let path = self.absolute(path);
        let status = match error {
            LspError::NoServer { .. } | LspError::ShutDown => ReportStatus::NoServer,
            LspError::ServerStarting { .. } => ReportStatus::Stale,
            LspError::ServerUnavailable { reason, .. } => ReportStatus::Failed(reason.clone()),
            LspError::Unsupported { .. }
            | LspError::Timeout { .. }
            | LspError::Request { .. }
            | LspError::InvalidPosition { .. }
            | LspError::Io { .. } => ReportStatus::Failed(error.to_string()),
        };
        let mut report = DiagnosticsReport::without_server(path.clone(), status);
        if report.status == ReportStatus::Stale {
            report.file = self.inner.store.file(&path);
        }
        report
    }
}

struct Detected {
    path: PathBuf,
    language: &'static str,
    wants: Vec<Want>,
}

/// Picks the server for a file: the first configured one whose binary exists and whose project
/// root can be found. Blocking filesystem work.
fn detect(
    registry: &Registry,
    launcher: &dyn ServerLauncher,
    workspace: &Path,
    path: &Path,
) -> LspResult<Detected> {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let Some(language) = language_for_path(&path) else {
        return Err(LspError::NoServer {
            path,
            hint: " (Nori knows no language server for this file type)".to_owned(),
        });
    };
    let mut missing_root = false;
    for server in registry.for_language(language) {
        let Some(command) = launcher.resolve(server) else {
            continue;
        };
        let Some(root) = find_root(&path, server, workspace) else {
            missing_root = true;
            continue;
        };
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        return Ok(Detected {
            path,
            language,
            wants: vec![Want {
                server: server.clone(),
                command,
                root,
            }],
        });
    }
    let hint = if missing_root {
        " (no project file found above it; the language server needs one)".to_owned()
    } else {
        registry.install_hint(language)
    };
    Err(LspError::NoServer { path, hint })
}

async fn wait_ready(watch: SlotWatch, wait: Duration) -> LspResult<Arc<ServerHandle>> {
    let SlotWatch { name, mut rx } = watch;
    let deadline = Instant::now() + wait;
    loop {
        let view = rx.borrow_and_update().clone();
        match view {
            SlotView::Ready(handle) => return Ok(handle),
            SlotView::Failed { reason, retry_at } => {
                let retry = match retry_at {
                    Some(at) => {
                        let left = at.saturating_duration_since(Instant::now()).as_secs();
                        format!("; it will be tried again in {left} s")
                    }
                    None => {
                        "; it failed too often and is not restarted (restart it by hand)".to_owned()
                    }
                };
                return Err(LspError::ServerUnavailable {
                    server: name,
                    reason: format!("{reason}{retry}"),
                });
            }
            SlotView::Starting => {}
        }
        match tokio::time::timeout_at(deadline, rx.changed()).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(LspError::ShutDown),
            Err(_) => return Err(LspError::ServerStarting { server: name }),
        }
    }
}

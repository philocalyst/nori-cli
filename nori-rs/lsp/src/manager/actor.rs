//! The manager's actor: the table of servers (one slot per server and root), their lifecycle and
//! restarts. It does no IO and awaits nothing but its mailbox; starting a server happens in a
//! task of its own, and callers wait for a slot to become ready on a `watch` channel.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use helix_lsp::LanguageServerId;
use parking_lot::Mutex;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::config::Timing;
use crate::diagnostics::DiagnosticStore;
use crate::error::LspError;
use crate::launcher::LaunchRequest;
use crate::launcher::ServerLauncher;
use crate::registry::ServerConfig;
use crate::server::ServerHandle;
use crate::server::pump;
use crate::types::ServerState;
use crate::types::ServerStatus;

/// The texts the manager last sent for each file, so a server that becomes ready later can be
/// brought up to date.
pub(crate) type DocStore = Arc<Mutex<HashMap<PathBuf, (String, Arc<str>)>>>;

/// A server that stayed up this long is not counted against its restart budget.
const HEALTHY_AFTER: Duration = Duration::from_secs(60);
/// How many known documents a freshly started server is given.
const STARTUP_DOCUMENTS: usize = 200;

pub(crate) type SlotKey = (String, PathBuf);

#[derive(Clone)]
pub(crate) enum SlotView {
    Starting,
    Ready(Arc<ServerHandle>),
    Failed {
        reason: String,
        /// `None` once the restart budget is spent.
        retry_at: Option<Instant>,
    },
}

#[derive(Clone)]
pub(crate) struct Want {
    pub server: ServerConfig,
    pub command: PathBuf,
    pub root: PathBuf,
}

impl Want {
    fn key(&self) -> SlotKey {
        (self.server.name.clone(), self.root.clone())
    }
}

pub(crate) struct SlotWatch {
    pub name: String,
    pub rx: watch::Receiver<SlotView>,
}

pub(crate) enum Command {
    Ensure {
        wants: Vec<Want>,
        reply: oneshot::Sender<Result<Vec<SlotWatch>, LspError>>,
    },
    Started {
        key: SlotKey,
        id: LanguageServerId,
        result: Result<Arc<ServerHandle>, String>,
    },
    Exited {
        key: SlotKey,
        id: LanguageServerId,
        reason: String,
    },
    Ready {
        reply: oneshot::Sender<Vec<Arc<ServerHandle>>>,
    },
    Status {
        reply: oneshot::Sender<Vec<ServerStatus>>,
    },
    Restart {
        name: Option<String>,
        reply: oneshot::Sender<i32>,
    },
    Shutdown {
        reply: Option<oneshot::Sender<()>>,
    },
}

struct Slot {
    want: Want,
    view: watch::Sender<SlotView>,
    id: Option<LanguageServerId>,
    failures: i32,
    ready_at: Option<Instant>,
}

pub(crate) struct Actor {
    rx: UnboundedReceiver<Command>,
    tx: UnboundedSender<Command>,
    launcher: Arc<dyn ServerLauncher>,
    timing: Timing,
    max_restarts: i32,
    store: DiagnosticStore,
    docs: DocStore,
    slots: HashMap<SlotKey, Slot>,
    next_id: i64,
}

impl Actor {
    pub fn new(
        rx: UnboundedReceiver<Command>,
        tx: UnboundedSender<Command>,
        launcher: Arc<dyn ServerLauncher>,
        timing: Timing,
        max_restarts: i32,
        store: DiagnosticStore,
        docs: DocStore,
    ) -> Self {
        Self {
            rx,
            tx,
            launcher,
            timing,
            max_restarts,
            store,
            docs,
            slots: HashMap::new(),
            next_id: 1,
        }
    }

    pub async fn run(mut self) {
        while let Some(command) = self.rx.recv().await {
            match command {
                Command::Ensure { wants, reply } => {
                    let _ = reply.send(Ok(self.ensure(wants)));
                }
                Command::Started { key, id, result } => self.started(&key, id, result),
                Command::Exited { key, id, reason } => self.exited(&key, id, reason),
                Command::Ready { reply } => {
                    let _ = reply.send(self.ready_handles());
                }
                Command::Status { reply } => {
                    let _ = reply.send(self.status());
                }
                Command::Restart { name, reply } => {
                    let _ = reply.send(self.restart(name.as_deref()));
                }
                Command::Shutdown { reply } => {
                    self.shutdown().await;
                    if let Some(reply) = reply {
                        let _ = reply.send(());
                    }
                    return;
                }
            }
        }
        self.shutdown().await;
    }

    fn ensure(&mut self, wants: Vec<Want>) -> Vec<SlotWatch> {
        let mut watches = Vec::new();
        for want in wants {
            let key = want.key();
            if !self.slots.contains_key(&key) {
                let (view, _) = watch::channel(SlotView::Starting);
                self.slots.insert(
                    key.clone(),
                    Slot {
                        want: want.clone(),
                        view,
                        id: None,
                        failures: 0,
                        ready_at: None,
                    },
                );
                self.spawn_start(&key);
            } else if let Some(slot) = self.slots.get(&key) {
                let due = matches!(
                    &*slot.view.borrow(),
                    SlotView::Failed { retry_at: Some(at), .. } if Instant::now() >= *at
                );
                if due {
                    slot.view.send_replace(SlotView::Starting);
                    self.spawn_start(&key);
                }
            }
            if let Some(slot) = self.slots.get(&key) {
                watches.push(SlotWatch {
                    name: key.0.clone(),
                    rx: slot.view.subscribe(),
                });
            }
        }
        watches
    }

    fn spawn_start(&mut self, key: &SlotKey) {
        let Some(slot) = self.slots.get_mut(key) else {
            return;
        };
        let id = LanguageServerId(self.next_id);
        self.next_id += 1;
        slot.id = Some(id);
        let job = StartJob {
            key: key.clone(),
            id,
            want: slot.want.clone(),
            launcher: Arc::clone(&self.launcher),
            timing: self.timing.clone(),
            store: self.store.clone(),
            docs: Arc::clone(&self.docs),
            tx: self.tx.clone(),
        };
        tokio::spawn(job.run());
    }

    fn started(
        &mut self,
        key: &SlotKey,
        id: LanguageServerId,
        result: Result<Arc<ServerHandle>, String>,
    ) {
        let Some(slot) = self.slots.get_mut(key) else {
            return;
        };
        if slot.id != Some(id) {
            return;
        }
        match result {
            Ok(handle) => {
                slot.ready_at = Some(Instant::now());
                slot.view.send_replace(SlotView::Ready(handle));
            }
            Err(reason) => self.fail(key, reason),
        }
    }

    fn exited(&mut self, key: &SlotKey, id: LanguageServerId, reason: String) {
        let current = self.slots.get(key).and_then(|slot| slot.id);
        if current == Some(id) {
            self.store.clear_server(id.0);
            self.fail(key, reason);
        }
    }

    /// Marks a slot failed and schedules the next attempt, if the budget allows one.
    fn fail(&mut self, key: &SlotKey, reason: String) {
        let Some(slot) = self.slots.get_mut(key) else {
            return;
        };
        if slot.ready_at.is_some_and(|at| at.elapsed() > HEALTHY_AFTER) {
            slot.failures = 0;
        }
        slot.ready_at = None;
        slot.failures += 1;
        let retry_at = if slot.failures > self.max_restarts {
            None
        } else {
            let doublings = u32::try_from(slot.failures - 1).unwrap_or(0).min(16);
            let delay = self
                .timing
                .backoff_base
                .saturating_mul(2u32.saturating_pow(doublings))
                .min(self.timing.backoff_cap);
            Some(Instant::now() + delay)
        };
        tracing::warn!(server = %key.0, root = %key.1.display(), %reason, "language server failed");
        slot.view
            .send_replace(SlotView::Failed { reason, retry_at });
    }

    fn ready_handles(&self) -> Vec<Arc<ServerHandle>> {
        let mut handles: Vec<Arc<ServerHandle>> = self
            .slots
            .values()
            .filter_map(|slot| match &*slot.view.borrow() {
                SlotView::Ready(handle) => Some(Arc::clone(handle)),
                SlotView::Starting | SlotView::Failed { .. } => None,
            })
            .collect();
        handles.sort_by_key(|handle| handle.id());
        handles
    }

    fn status(&self) -> Vec<ServerStatus> {
        let mut out: Vec<ServerStatus> = self
            .slots
            .iter()
            .map(|(key, slot)| {
                let state = match &*slot.view.borrow() {
                    SlotView::Starting => ServerState::Starting,
                    SlotView::Ready(handle) => ServerState::Ready {
                        busy: handle.is_busy(),
                    },
                    SlotView::Failed { reason, retry_at } => ServerState::Failed {
                        reason: reason.clone(),
                        retry_in_seconds: retry_at.map(|at| {
                            let left = at.saturating_duration_since(Instant::now());
                            i64::try_from(left.as_secs()).unwrap_or(i64::MAX)
                        }),
                    },
                };
                ServerStatus {
                    name: key.0.clone(),
                    root: key.1.clone(),
                    state,
                }
            })
            .collect();
        out.sort_by(|a, b| (&a.name, &a.root).cmp(&(&b.name, &b.root)));
        out
    }

    /// Restarts servers by name (all when `None`), with a fresh restart budget.
    fn restart(&mut self, name: Option<&str>) -> i32 {
        let keys: Vec<SlotKey> = self
            .slots
            .keys()
            .filter(|key| name.is_none_or(|name| key.0 == name))
            .cloned()
            .collect();
        for key in &keys {
            if let Some(slot) = self.slots.get_mut(key) {
                if let Some(id) = slot.id {
                    self.store.clear_server(id.0);
                }
                slot.failures = 0;
                slot.ready_at = None;
                // Dropping the old handle kills the old process.
                slot.view.send_replace(SlotView::Starting);
            }
            self.spawn_start(key);
        }
        i32::try_from(keys.len()).unwrap_or(i32::MAX)
    }

    async fn shutdown(&mut self) {
        let handles = self.ready_handles();
        let deadline = self.timing.shutdown;
        let mut stopping = JoinSet::new();
        for handle in handles {
            stopping.spawn(async move { handle.shutdown(deadline).await });
        }
        while stopping.join_next().await.is_some() {}
        for slot in self.slots.values() {
            if let Some(id) = slot.id {
                self.store.clear_server(id.0);
            }
        }
        self.slots.clear();
    }
}

/// Starting one server: launch, handshake, bring it up to date with the documents we know.
struct StartJob {
    key: SlotKey,
    id: LanguageServerId,
    want: Want,
    launcher: Arc<dyn ServerLauncher>,
    timing: Timing,
    store: DiagnosticStore,
    docs: DocStore,
    tx: UnboundedSender<Command>,
}

impl StartJob {
    async fn run(self) {
        let result = self.start().await;
        let _ = self.tx.send(Command::Started {
            key: self.key.clone(),
            id: self.id,
            result,
        });
    }

    async fn start(&self) -> Result<Arc<ServerHandle>, String> {
        let launched = self
            .launcher
            .launch(LaunchRequest {
                server: self.want.server.clone(),
                command: self.want.command.clone(),
                root: self.want.root.clone(),
                id: self.id,
                request_timeout_seconds: self.timing.request.as_secs(),
            })
            .map_err(|error| error.to_string())?;
        let handle = Arc::new(ServerHandle::new(
            self.id,
            self.want.server.name.clone(),
            self.want.root.clone(),
            launched.client,
        ));
        let exits = self.tx.clone();
        let (key, id) = (self.key.clone(), self.id);
        tokio::spawn(pump(
            Arc::downgrade(&handle),
            launched.incoming,
            self.store.clone(),
            move |reason| {
                let _ = exits.send(Command::Exited { key, id, reason });
            },
        ));
        if let Err(error) = handle.client().handshake(self.timing.init).await {
            let tail = handle.stderr_tail();
            let detail = tail
                .iter()
                .rev()
                .find(|line| !line.trim().is_empty())
                .map_or(String::new(), |line| format!(" (stderr: {line})"));
            return Err(format!("initialize failed: {error}{detail}"));
        }
        self.sync_known_documents(&handle);
        Ok(handle)
    }

    fn sync_known_documents(&self, handle: &ServerHandle) {
        let known: Vec<(PathBuf, String, Arc<str>)> = self
            .docs
            .lock()
            .iter()
            .filter(|(path, _)| path.starts_with(&self.want.root))
            .take(STARTUP_DOCUMENTS)
            .map(|(path, (language, text))| (path.clone(), language.clone(), Arc::clone(text)))
            .collect();
        for (path, language, text) in known {
            if self.want.server.languages.contains(&language) {
                handle.sync_document(&path, &language, &text);
            }
        }
    }
}

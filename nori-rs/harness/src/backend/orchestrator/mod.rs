//! Delegation: any agent Nori runs can hand work to another session on any configured agent, so a
//! Claude session can delegate to Codex and the other way round, in trees of any depth.
//!
//! The vocabulary:
//!
//! - An **actor** is an addressable agent session: the root, a child Nori started, or a provider's
//!   own child. The tree-wide [`directory`] is the only registry of them. Actors have a stable
//!   id, aliases, a parent, a lifecycle, a mailbox of tasks, and a history.
//! - A **task** (`task-N`) is one turn of work given to an actor. Sending a message to a live
//!   actor queues a task on it; sending one to an ended actor revives that same actor. A task
//!   never creates an actor by itself.
//! - A **message** is what a task carries: it advances `queued -> delivered -> acknowledged` and
//!   its sender can see where it is.
//!
//! Nothing here blocks on a peer. Results and replies arrive as new turns: a task that finishes
//! while its requester is not waiting on it reaches it in a hidden turn as soon as it is idle
//! (the completion wake, see [`wake`]); a peer's message reaches an idle actor at once and a
//! working one at its next turn boundary. A result read with `task_status` or `task_wait` is
//! delivered by that read, so each reaches the requester once. A child never gets broader
//! permissions than its parent: it starts with the parent's current approval policy.
//!
//! The user sees children as the parent's subagents: a child's updates are relayed into the
//! parent's event stream under the child's session id, and its permission requests are asked in
//! the parent conversation, named after the child.

mod actor_watch;
mod approvals;
mod brief;
mod caller;
mod capabilities;
mod catalog;
mod catalog_api;
mod child;
mod compaction;
mod context;
mod control;
mod delegation;
mod delivery;
mod directory;
mod exchange;
mod fence;
mod judge;
mod lifecycle;
mod managed_journal;
mod models;
mod native_rebind;
mod parent;
mod persistence;
mod polling;
mod probe;
mod program;
mod report;
mod spawn;
mod status;
mod task;
mod text;
mod tools;
mod wake;
mod worktree;

pub(crate) use actor_watch::ActorWatch;
pub use catalog_api::CachedAgentModel;
pub use catalog_api::cached_agent_models;
pub use catalog_api::refresh_agent_catalog;
pub use catalog_api::refresh_agent_catalog_except;
pub(crate) use directory::ActorView;
pub(crate) use directory::DescendantRoute;
pub(crate) use judge::ReviewFailure;
pub(crate) use judge::ReviewRequest;
pub(crate) use native_rebind::NativeRebindSpec;
pub(crate) use program::ActorScope;
pub(crate) use program::ProgramActor;
pub(crate) use program::RecoveryNote;
pub(crate) use report::TaskSnapshot;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use nori_config::AskForApproval;
use nori_protocol::SessionEvent;
use nori_protocol::acp::v1 as acp;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::watch;

use super::BackendEvent;
use crate::connection::DelegatedRequest;
pub(crate) use catalog::advertises;
pub(crate) use catalog::clear_unavailable;
pub(crate) use catalog::mark_unavailable;
pub(crate) use catalog::offered_models;
pub(crate) use catalog::remember as remember_models;
use directory::Directory;
pub(crate) use probe::reprobe;
pub(crate) use text::TASK_INSTRUCTIONS;
pub(crate) use tools::always_load;

/// The longest a tool call blocks waiting on tasks. Agents drop MCP calls that stay silent much
/// longer (Codex after 60 seconds); a longer task is delivered by the completion wake.
const MAX_WAIT: Duration = Duration::from_secs(50);
/// Default wait returns after this polling interval; the task continues and delivers its result.
const DEFAULT_WAIT: Duration = Duration::from_secs(45);

/// What every orchestrator of one delegation tree shares.
#[derive(Debug)]
pub(crate) struct Tree {
    /// Tasks not yet ended, for status reporting. This is not an admission limit.
    running: AtomicI64,
    /// The one registry of addressable sessions.
    pub(super) directory: Directory,
    /// Serializes the directory's journal writes.
    journal: persistence::Journal,
    /// What programs hand their actors' recovery engines and what those engines report back.
    program: program::ProgramState,
    /// The session's compiled gates, which the whole tree is consulted through.
    pub(crate) gates: super::gates::GateCell,
}

/// What a session needs to delegate: the configuration its children start from and where it sits
/// in its tree of delegations.
#[derive(Debug, Clone)]
pub(crate) struct Seed {
    pub(crate) config: Arc<crate::config::NoriConfig>,
    pub(crate) cli_version: String,
    pub(crate) depth: i64,
    pub(crate) tree: Arc<Tree>,
    pub(crate) record_transcript: bool,
    /// The actor this session is, when it is a delegated child.
    pub(crate) actor_view: Option<acp::SessionId>,
    /// The role the child was delegated in (`review`, `research`, ...), which selects its
    /// recovery policy.
    pub(crate) role: Option<String>,
    /// What the program that started this actor restricts it to.
    pub(crate) scope: Option<Arc<ActorScope>>,
}

impl Seed {
    /// A session the user started, at the root of its own tree.
    pub(crate) fn root(config: Arc<crate::config::NoriConfig>, cli_version: String) -> Self {
        Self {
            config,
            cli_version,
            depth: 0,
            tree: Arc::new(Tree {
                running: AtomicI64::new(0),
                directory: Directory::default(),
                journal: persistence::Journal::default(),
                program: program::ProgramState::default(),
                gates: super::gates::GateCell::default(),
            }),
            record_transcript: true,
            actor_view: None,
            role: None,
            scope: None,
        }
    }

    /// The seed of a child of this session, which runs with `config`.
    pub(crate) fn child(&self, config: Arc<crate::config::NoriConfig>) -> Self {
        Self {
            config,
            cli_version: self.cli_version.clone(),
            depth: self.depth.saturating_add(1),
            tree: Arc::clone(&self.tree),
            record_transcript: self.record_transcript,
            actor_view: self.actor_view.clone(),
            // The role and restriction the delegation gave the child (see `child::launch`).
            role: self.role.clone(),
            scope: self.scope.clone(),
        }
    }

    /// Mark a short-lived catalog probe as a session without a conversation log.
    pub(crate) fn without_transcript(mut self) -> Self {
        self.record_transcript = false;
        self
    }

    /// Whether the session was started as a delegated child.
    pub(crate) fn is_child(&self) -> bool {
        self.depth > 0
    }
}

/// A completion wake: the hidden prompt, and what it hands over.
pub(crate) struct WakeTurn {
    pub(crate) text: String,
    pub(crate) delivered: nori_protocol::TaskResultsDelivered,
}

/// What the orchestrator needs the parent's backend to do.
pub(crate) enum Signal {
    /// A child asks for permission; the parent asks the user in its own conversation, and the
    /// answer goes back to the child.
    Approval(Box<DelegatedRequest>),
    /// Results are owed to the parent agent; deliver them when the parent is idle.
    ResultsOwed,
    /// Messages from peers are queued for the root session.
    MailboxWake,
    /// These relayed permission requests belong to a task that has ended; nobody needs them
    /// answered any more.
    Withdraw(Vec<acp::RequestId>),
}

/// The parent session's side of the orchestrator, bound once the session exists.
pub(crate) struct Binding {
    pub(crate) session_id: acp::SessionId,
    pub(crate) approval_policy: watch::Receiver<AskForApproval>,
}

/// The delegations of one session: its side of the tree's directory.
#[derive(Clone)]
pub(crate) struct Orchestrator {
    inner: Arc<Inner>,
}

pub(super) struct Inner {
    seed: Seed,
    program_backend: std::sync::OnceLock<std::sync::Weak<super::AcpBackend>>,
    events: mpsc::Sender<BackendEvent>,
    binding: std::sync::Mutex<Option<Binding>>,
    signals: std::sync::Mutex<Option<mpsc::UnboundedSender<Signal>>>,
    admission: Arc<Mutex<()>>,
    generation: Arc<AtomicI64>,
    shut_down: Arc<AtomicBool>,
    /// Provider-owned sessions share controls but keep their own delegation identity.
    caller_view: Option<acp::SessionId>,
    native_scopes: Mutex<HashMap<acp::SessionId, Orchestrator>>,
    /// What this caller's recent status polls returned.
    polls: std::sync::Mutex<polling::PollTracker>,
    /// The background watchers this actor armed.
    monitors: super::monitor::Monitors,
    /// The background jobs this actor started; their wakes are monitors.
    jobs: super::jobs::Jobs,
    /// The Pkl runtime's modules for this session (jobs, monitors, skills, plugins), as last
    /// loaded; `None` until the first load ends. A child's are empty.
    modules: Arc<tokio::sync::watch::Sender<Option<Arc<nori_pkl::catalog::Catalog>>>>,
}

impl Orchestrator {
    /// The orchestrator of a session, and the signals its backend must act on.
    pub(crate) fn new(
        seed: Seed,
        events: mpsc::Sender<BackendEvent>,
    ) -> (Self, mpsc::UnboundedReceiver<Signal>) {
        let (signals_tx, signals_rx) = mpsc::unbounded_channel();
        let monitors = super::monitor::Monitors::new();
        let home = seed.config.nori_home.clone();
        let inner = Inner {
            seed,
            program_backend: std::sync::OnceLock::new(),
            events,
            binding: std::sync::Mutex::new(None),
            signals: std::sync::Mutex::new(Some(signals_tx)),
            admission: Arc::new(Mutex::new(())),
            generation: Arc::new(AtomicI64::new(0)),
            shut_down: Arc::new(AtomicBool::new(false)),
            caller_view: None,
            native_scopes: Mutex::new(HashMap::new()),
            polls: std::sync::Mutex::new(polling::PollTracker::default()),
            jobs: super::jobs::Jobs::new(monitors.clone(), home),
            monitors,
            modules: Arc::new(tokio::sync::watch::Sender::new(None)),
        };
        (
            Self {
                inner: Arc::new(inner),
            },
            signals_rx,
        )
    }

    pub(crate) async fn rewind_admission(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.inner.admission.lock().await
    }

    pub(crate) fn advance_generation(&self) {
        self.inner.generation.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn admission_generation(&self) -> i64 {
        self.inner.generation.load(Ordering::Acquire)
    }

    /// The monitors this actor armed.
    pub(crate) fn monitors(&self) -> &super::monitor::Monitors {
        &self.inner.monitors
    }

    /// The background jobs this actor started.
    pub(crate) fn jobs(&self) -> &super::jobs::Jobs {
        &self.inner.jobs
    }

    /// Whether this is a delegated child's session, which works to its parent's task.
    pub(crate) fn is_child(&self) -> bool {
        self.inner.seed.is_child()
    }

    /// The session's Pkl modules as last loaded. A caller right after the session started waits
    /// for the first load (they are read off the event loop at start), at most a few seconds.
    pub(crate) async fn modules(&self) -> Arc<nori_pkl::catalog::Catalog> {
        const FIRST_LOAD: std::time::Duration = std::time::Duration::from_secs(10);
        let mut loaded = self.inner.modules.subscribe();
        let first = tokio::time::timeout(FIRST_LOAD, loaded.wait_for(Option::is_some)).await;
        match first {
            Ok(Ok(catalog)) => catalog.clone().unwrap_or_default(),
            Ok(Err(_)) | Err(_) => Arc::default(),
        }
    }

    /// The modules as they load: `None` until the first load, then each catalog published.
    pub(crate) fn module_updates(
        &self,
    ) -> tokio::sync::watch::Receiver<Option<Arc<nori_pkl::catalog::Catalog>>> {
        self.inner.modules.subscribe()
    }

    pub(crate) fn publish_modules(&self, catalog: Arc<nori_pkl::catalog::Catalog>) {
        self.inner.modules.send_replace(Some(catalog));
    }

    /// The configuration this session runs with.
    pub(crate) fn config(&self) -> &Arc<crate::config::NoriConfig> {
        &self.inner.seed.config
    }

    /// What a replacement of this session's agent keeps: its place in its delegation tree.
    pub(crate) fn seed(&self) -> &Seed {
        &self.inner.seed
    }

    /// What the program that started this actor says about its recovery for the turn it is
    /// running, laid over the policy in force.
    pub(crate) fn recovery_override(&self) -> Option<nori_workflow::RecoveryOverride> {
        let view = self.inner.seed.actor_view.as_ref()?;
        self.inner
            .seed
            .tree
            .program
            .recovery_override(&view.to_string())
    }

    /// The role this session was delegated in, if it is a child with one.
    pub(crate) fn role(&self) -> Option<&str> {
        self.inner.seed.role.as_deref()
    }

    /// Whether the user trusts the session's project, so its own scripts may run.
    pub(crate) fn project_trusted(&self) -> bool {
        self.inner.seed.config.active_project.trust_level == Some(nori_config::TrustLevel::Trusted)
    }

    /// Bind the parent session once it exists, and again when `/compact` replaces it.
    pub(crate) fn bind(&self, binding: Binding) {
        let new_session_id = binding.session_id.clone();
        let old_session_id = if let Ok(mut slot) = self.inner.binding.lock() {
            let old = slot.as_ref().map(|existing| existing.session_id.clone());
            *slot = Some(binding);
            old
        } else {
            None
        };
        let directory = &self.inner.seed.tree.directory;
        if self.inner.caller_view.is_some() {
            return;
        }
        if let Some(view) = &self.inner.seed.actor_view {
            // A child's own session is another name of the child: provider children announced
            // on it are found under it.
            let owner = Arc::downgrade(&self.inner);
            directory.with(|registry| {
                if let Some(id) = registry.by_session(view).cloned() {
                    registry.note_session(&id, new_session_id.clone());
                    // Children restored from a journal were owned by the root's orchestrator
                    // while their parent slept; its revival hands them to it.
                    for actor in registry.actors.values_mut() {
                        if actor.parent.as_ref() == Some(&id)
                            && matches!(actor.link, directory::Link::Gone(_))
                        {
                            actor.owner = Some(owner.clone());
                        }
                    }
                }
            });
        } else {
            self.preserve_continuity_on_bind(old_session_id.as_ref(), &new_session_id);
        }
    }

    /// Stop every task the session's agents delegated, on an explicit stop (not on an interrupt
    /// of the root's turn), and hold finished results for the user's
    /// next turn; see [`lifecycle`].
    pub(crate) async fn stop(&self) {
        self.inner.stop_delegations().await;
    }

    /// The user sent a prompt: results held by a stop are owed again, after this turn.
    pub(crate) async fn release_held(&self) {
        let scope = self.inner.scope().await;
        self.inner.seed.tree.directory.with(|registry| {
            for actor in registry.actors.values_mut() {
                for task in &mut actor.tasks {
                    if task.wake == task::Wake::AfterNextTurn
                        && matches!(&task.requester, task::Party::Actor(id) if scope.contains(id))
                    {
                        task.wake = task::Wake::Always;
                    }
                }
            }
        });
    }

    /// End every child and stop signalling the backend.
    pub(crate) async fn shutdown(&self) {
        // First, so nothing starts while the running tasks are cancelled.
        self.inner.shut_down.store(true, Ordering::Release);
        self.stop().await;
        self.inner.journal_snapshot().await;
        // Watchers end with the session; the journal keeps them armed, so a resume finds them dormant.
        self.inner.monitors.close();
        let scope = self.inner.scope().await;
        let children = self.inner.seed.tree.directory.with(|registry| {
            let mut children = Vec::new();
            for actor in registry.actors.values_mut() {
                if actor
                    .parent
                    .as_ref()
                    .is_some_and(|parent| scope.contains(parent))
                    && let directory::Link::Child(child) | directory::Link::Staged(child) =
                        &actor.link
                {
                    children.push(child.clone());
                    actor.link = directory::Link::Gone(nori_protocol::SubagentOutcome::Closed);
                }
            }
            children
        });
        // Together, so quitting takes as long as the slowest child rather than all of them.
        futures::future::join_all(children.iter().map(|child| child.harness.shutdown())).await;
        let scopes: Vec<_> = self
            .inner
            .native_scopes
            .lock()
            .await
            .drain()
            .map(|(_, scope)| scope)
            .collect();
        for scope in scopes {
            if let Ok(mut signals) = scope.inner.signals.lock() {
                signals.take();
            }
        }
        if let Ok(mut signals) = self.inner.signals.lock() {
            signals.take();
        }
        if let Ok(mut binding) = self.inner.binding.lock() {
            binding.take();
        }
    }

    pub(super) fn inner(&self) -> &Arc<Inner> {
        &self.inner
    }
}

impl Inner {
    pub(super) fn directory(&self) -> &Directory {
        &self.seed.tree.directory
    }

    pub(super) fn count_running_task(&self) {
        self.seed.tree.running.fetch_add(1, Ordering::AcqRel);
    }

    pub(super) fn count_ended_task(&self) {
        self.seed.tree.running.fetch_sub(1, Ordering::AcqRel);
    }

    pub(super) async fn emit(&self, event: SessionEvent) {
        if let Some(view) = &self.caller_view
            && let Some(backend) = self
                .program_backend
                .get()
                .and_then(std::sync::Weak::upgrade)
            && let Ok(recorder) = backend.native_children.history(view).await
            && let Err(error) = recorder
                .record_session_event(&crate::transcript::SessionEventEntry::new(event.clone()))
                .await
        {
            tracing::warn!(%error, "failed to journal native caller delegation event");
        }
        let _ = self.events.send(BackendEvent::Public(event)).await;
    }

    pub(super) fn signal(&self, signal: Signal) {
        if let Ok(signals) = self.signals.lock()
            && let Some(signals) = signals.as_ref()
        {
            let _ = signals.send(signal);
        }
    }

    pub(super) fn parent_session(&self) -> Option<acp::SessionId> {
        self.binding
            .lock()
            .ok()?
            .as_ref()
            .map(|binding| binding.session_id.clone())
    }

    /// The parent's approval policy now: a child never gets a broader one.
    pub(super) fn approval_policy(&self) -> AskForApproval {
        self.binding
            .lock()
            .ok()
            .and_then(|binding| {
                binding
                    .as_ref()
                    .map(|binding| *binding.approval_policy.borrow())
            })
            .unwrap_or(self.seed.config.approval_policy)
    }

    pub(super) fn backend(&self) -> Option<Arc<super::AcpBackend>> {
        self.program_backend
            .get()
            .and_then(std::sync::Weak::upgrade)
    }
}

//! The actor directory: the one registry of every addressable agent session in a delegation tree.
//!
//! An **actor** is any agent session Nori can talk to: the root session, a child it started for
//! a delegation (including the children of conversation programs), or a provider's own child
//! session (`native` actors). Each actor has one stable [`ActorId`], aliases, a kind, a parent, a
//! lifecycle ([`Presence`]), and the list of [tasks](super::task::Task) it was given. A task is
//! one turn of work for an actor; it is not a registry entry of its own and never creates an
//! actor.
//!
//! Everything that needs to find an actor resolves through [`Registry::resolve`]: the task tools,
//! `delegate_task` alias reuse, conversation-program `send`, and the user's subagent controls.
//! What is genuinely different between actor kinds lives in [`Link`], the transport handle.
//!
//! The registry is shared by every orchestrator in the tree and guarded by one synchronous mutex;
//! it is never held across an await. Callers read or change it through [`Directory::with`] and do
//! their I/O afterwards.

mod resolve;
mod tasks;
mod view;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::Weak;

use nori_protocol::SubagentOutcome;
use nori_protocol::acp::v1 as acp;
use tokio::sync::watch;

pub(super) use resolve::Caller;
pub(super) use resolve::ResolveError;
pub(super) use resolve::Resolved;
pub(super) use tasks::NewTask;
pub(crate) use view::ActorView;

use super::Inner;
use super::child;
use super::compaction::ActorDigest;
use super::fence::FenceLedger;
use super::task::Party;
use super::task::Task;
use super::task::TaskId;
use super::task::TaskStatus;
use crate::runtime::HarnessHandle;
use crate::transcript::TranscriptRecorder;

/// The identity of an actor for as long as the session lives. A child's id is its session id as
/// the user's view shows it, so it never changes; the root keeps the id it first had when
/// `/compact` or a rewind gives it a new session (the old session ids keep resolving to it).
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct ActorId(acp::SessionId);

impl ActorId {
    pub(super) fn new(session: acp::SessionId) -> Self {
        Self(session)
    }

    pub(super) fn session(&self) -> &acp::SessionId {
        &self.0
    }
}

impl std::fmt::Display for ActorId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Which kind of session an actor is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActorKind {
    /// The session the user is talking to.
    Root,
    /// A full harness session Nori started: a delegated child, a program's child, a fork.
    Managed,
    /// A child the actor's provider runs inside its own session; Nori can queue work for it and
    /// read it, but does not own its turns.
    Native,
}

/// How Nori reaches an actor's session.
#[derive(Clone)]
pub(super) enum Link {
    /// The root session, woken through its orchestrator's signals.
    Root(Weak<Inner>),
    /// The child's agent is being started.
    Starting,
    Child(child::Child),
    /// An adopted session whose view is not yet published; invisible to every resolver.
    Staged(child::Child),
    Native {
        backend: Weak<crate::backend::AcpBackend>,
        /// The provider said the child is working.
        running: bool,
    },
    /// The actor's session ended and cannot take another turn until it is revived.
    Gone(SubagentOutcome),
}

/// An actor's lifecycle as the agents and the user see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Presence {
    Starting,
    Idle,
    /// A turn is in progress. `task` is absent for provider children, whose turns are not
    /// tracked as tasks.
    Working {
        task: Option<TaskId>,
    },
    Ended(SubagentOutcome),
}

impl Presence {
    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Idle => "idle",
            Self::Working { .. } => "working",
            Self::Ended(SubagentOutcome::Failed { .. }) => "failed",
            Self::Ended(SubagentOutcome::Cancelled) => "cancelled",
            Self::Ended(SubagentOutcome::Completed { .. } | SubagentOutcome::Closed) => "ended",
        }
    }
}

/// One addressable session.
pub(super) struct Actor {
    pub(super) id: ActorId,
    /// The session id the user's view and the provider know the actor by right now.
    pub(super) wire: acp::SessionId,
    pub(super) kind: ActorKind,
    /// The actor that delegated this one, and is owed its results.
    pub(super) parent: Option<ActorId>,
    /// The session the actor is announced under when that is not its parent's current session: a
    /// history fork of a provider child.
    pub(super) announce_parent: Option<acp::SessionId>,
    /// Names the actor answers to, first the one it is shown by.
    pub(super) aliases: Vec<String>,
    pub(super) agent: String,
    pub(super) agent_name: String,
    pub(super) model: Option<String>,
    /// Settings the agent was started with, kept so a revived actor starts the same way.
    pub(super) options: Vec<(String, String)>,
    pub(super) role: Option<String>,
    /// What the program that started the actor restricts it to; kept so a revived actor is
    /// restricted the same way.
    pub(super) scope: Option<std::sync::Arc<super::ActorScope>>,
    /// What the actor was first asked, shortened: the user's label for it.
    pub(super) title: String,
    pub(super) worktree: Option<PathBuf>,
    pub(super) link: Link,
    /// The orchestrator of the session that started this actor; it runs the actor's turns.
    pub(super) owner: Option<Weak<Inner>>,
    /// The actor's own conversation journal, kept so it can be restored after its session ends.
    pub(super) journal: Option<std::sync::Arc<TranscriptRecorder>>,
    /// Every task given to the actor, oldest first.
    pub(super) tasks: Vec<Task>,
    /// Leases on the results owed to this actor.
    pub(super) ledger: FenceLedger,
    pub(super) digest: Option<ActorDigest>,
    /// The user's queued prompts as the view last showed them.
    pub(super) queue_shown: Vec<String>,
    /// The connection that owns a provider child, kept after the child ends so it can be revived.
    pub(super) native_backend: Option<Weak<crate::backend::AcpBackend>>,
    /// The root session reports a turn open, whoever owns it.
    pub(super) busy: bool,
    /// Where the actor's own conversation journal lives (project, session), so it can be reopened
    /// after the process that wrote it is gone.
    pub(super) journal_ref: Option<(String, String)>,
}

impl Actor {
    pub(super) fn presence(&self) -> Presence {
        let open = self
            .tasks
            .iter()
            .find(|task| matches!(task.status, TaskStatus::Dispatched | TaskStatus::Running));
        match &self.link {
            Link::Starting | Link::Staged(_) => Presence::Starting,
            Link::Gone(outcome) => Presence::Ended(outcome.clone()),
            // A child whose actor stopped is ended, never running, even if nothing told us yet.
            Link::Child(child) if child.is_closed() => Presence::Ended(SubagentOutcome::Failed {
                reason: Some("its session ended unexpectedly".to_owned()),
            }),
            Link::Root(_) if self.busy => Presence::Working {
                task: open.map(|task| task.id.clone()),
            },
            Link::Root(_) | Link::Child(_) => {
                open.map_or(Presence::Idle, |task| Presence::Working {
                    task: Some(task.id.clone()),
                })
            }
            Link::Native { running, .. } => {
                if *running || open.is_some() {
                    Presence::Working {
                        task: open.map(|task| task.id.clone()),
                    }
                } else {
                    Presence::Idle
                }
            }
        }
    }

    /// The best address for the actor: its alias, or its session id.
    pub(super) fn address(&self) -> String {
        self.aliases
            .first()
            .cloned()
            .unwrap_or_else(|| self.wire.to_string())
    }

    /// The name the user's view shows for the actor.
    pub(super) fn label(&self) -> String {
        match (self.kind, self.aliases.first()) {
            (ActorKind::Root, _) => "root".to_owned(),
            (_, Some(alias)) => alias.clone(),
            (_, None) => self.agent_name.clone(),
        }
    }

    pub(super) fn open_tasks(&self) -> impl Iterator<Item = &Task> {
        self.tasks.iter().filter(|task| !task.status.is_ended())
    }

    pub(super) fn child(&self) -> Option<&child::Child> {
        match &self.link {
            Link::Child(child) | Link::Staged(child) => Some(child),
            Link::Root(_) | Link::Starting | Link::Native { .. } | Link::Gone(_) => None,
        }
    }
}

/// What registering an actor needs to know.
pub(super) struct NewActor {
    pub(super) id: ActorId,
    pub(super) kind: ActorKind,
    pub(super) parent: Option<ActorId>,
    pub(super) announce_parent: Option<acp::SessionId>,
    pub(super) alias: Option<String>,
    pub(super) agent: String,
    pub(super) agent_name: String,
    pub(super) model: Option<String>,
    pub(super) options: Vec<(String, String)>,
    pub(super) role: Option<String>,
    pub(super) scope: Option<std::sync::Arc<super::ActorScope>>,
    pub(super) title: String,
    pub(super) worktree: Option<PathBuf>,
    pub(super) link: Link,
    pub(super) owner: Option<Weak<Inner>>,
}

/// How the user's controls reach a session more than one hop down, through the child connection
/// that relays it.
#[derive(Clone)]
pub(crate) struct DescendantRoute {
    pub(crate) handle: HarnessHandle,
    pub(crate) session_id: acp::SessionId,
}

/// The mutable contents of the directory.
#[derive(Default)]
pub(super) struct Registry {
    pub(super) actors: HashMap<ActorId, Actor>,
    /// Registration order, which the listings keep.
    pub(super) order: Vec<ActorId>,
    /// Every session id an actor has had: its view id, and the id of the session behind it.
    pub(super) by_session: HashMap<acp::SessionId, ActorId>,
    pub(super) aliases: HashMap<String, ActorId>,
    pub(super) task_owner: HashMap<TaskId, ActorId>,
    pub(super) by_client_request: HashMap<String, TaskId>,
    /// Public ids of descendants the user's view shows, to the connection that relays them.
    pub(super) routes: HashMap<acp::SessionId, DescendantRoute>,
    pub(super) root: Option<ActorId>,
    pub(super) root_backend: Option<Weak<crate::backend::AcpBackend>>,
    pub(super) next_task: i64,
    pub(super) next_message: i64,
}

/// The tree-wide directory, shared by every orchestrator of one delegation tree.
pub(crate) struct Directory {
    registry: Mutex<Registry>,
    changed: watch::Sender<i64>,
}

impl std::fmt::Debug for Directory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Directory").finish_non_exhaustive()
    }
}

impl Default for Directory {
    fn default() -> Self {
        Self {
            registry: Mutex::new(Registry::default()),
            changed: watch::channel(0).0,
        }
    }
}

impl Directory {
    /// Read or change the registry. Never await inside `f`.
    pub(super) fn with<R>(&self, f: impl FnOnce(&mut Registry) -> R) -> R {
        f(&mut self.registry.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Tell everyone waiting on the directory that something changed.
    pub(super) fn touch(&self) {
        self.changed.send_modify(|revision| *revision += 1);
    }

    pub(super) fn subscribe(&self) -> watch::Receiver<i64> {
        self.changed.subscribe()
    }

    /// A provider's child session was announced or changed state: register or update it.
    #[expect(
        clippy::too_many_arguments,
        reason = "an announcement carries these independent facts"
    )]
    pub(crate) fn observe_native(
        &self,
        session: &acp::SessionId,
        parent_session: &acp::SessionId,
        agent: Option<&str>,
        title: Option<&str>,
        model: Option<&str>,
        running: Option<bool>,
        ended: Option<SubagentOutcome>,
        backend: &Weak<crate::backend::AcpBackend>,
    ) {
        self.with(|registry| {
            registry.observe_native(
                session,
                parent_session,
                agent,
                title,
                model,
                running,
                ended,
                backend,
            );
        });
        self.touch();
    }
}

/// An alias as stored: no scheme, no surrounding space.
pub(crate) fn normalize_alias(raw: &str) -> &str {
    let trimmed = raw.trim();
    trimmed.strip_prefix("actor://").unwrap_or(trimmed).trim()
}

impl Registry {
    pub(super) fn actor(&self, id: &ActorId) -> Option<&Actor> {
        self.actors.get(id)
    }

    pub(super) fn actor_mut(&mut self, id: &ActorId) -> Option<&mut Actor> {
        self.actors.get_mut(id)
    }

    /// The actor a session id belongs to, now or earlier.
    pub(super) fn by_session(&self, session: &acp::SessionId) -> Option<&ActorId> {
        self.by_session.get(session)
    }

    pub(super) fn root(&self) -> Option<&Actor> {
        self.root.as_ref().and_then(|id| self.actors.get(id))
    }

    /// The root session, bound or rebound to the session `session` (after `/compact`, a rewind
    /// or a restart). Aliases and tasks stay with it.
    pub(super) fn bind_root(&mut self, session: acp::SessionId, owner: Weak<Inner>) {
        if let Some(root) = self.root.clone()
            && let Some(actor) = self.actors.get_mut(&root)
        {
            actor.wire = session.clone();
            actor.link = Link::Root(owner);
            self.by_session.insert(session, root);
            return;
        }
        let id = ActorId::new(session.clone());
        let actor = Actor {
            id: id.clone(),
            wire: session,
            kind: ActorKind::Root,
            parent: None,
            announce_parent: None,
            aliases: vec!["root".to_owned()],
            agent: String::new(),
            agent_name: "root".to_owned(),
            model: None,
            options: Vec::new(),
            role: None,
            scope: None,
            title: "root session".to_owned(),
            worktree: None,
            link: Link::Root(owner.clone()),
            owner: Some(owner),
            journal: None,
            tasks: Vec::new(),
            ledger: FenceLedger::default(),
            digest: None,
            queue_shown: Vec::new(),
            native_backend: None,
            busy: false,
            journal_ref: None,
        };
        self.root = Some(id);
        self.insert(actor);
    }

    /// Add an actor. Its alias must be free: callers resolve it first.
    pub(super) fn register(&mut self, new: NewActor) {
        let actor = Actor {
            wire: new.id.session().clone(),
            id: new.id,
            kind: new.kind,
            parent: new.parent,
            announce_parent: new.announce_parent,
            aliases: Vec::new(),
            agent: new.agent,
            agent_name: new.agent_name,
            model: new.model,
            options: new.options,
            role: new.role,
            scope: new.scope,
            title: new.title,
            worktree: new.worktree,
            link: new.link,
            owner: new.owner,
            journal: None,
            tasks: Vec::new(),
            ledger: FenceLedger::default(),
            digest: None,
            queue_shown: Vec::new(),
            native_backend: None,
            busy: false,
            journal_ref: None,
        };
        let id = actor.id.clone();
        self.insert(actor);
        if let Some(alias) = new.alias {
            self.add_alias(&id, &alias);
        }
    }

    fn insert(&mut self, actor: Actor) {
        let id = actor.id.clone();
        self.by_session.insert(actor.wire.clone(), id.clone());
        self.by_session
            .entry(id.session().clone())
            .or_insert_with(|| id.clone());
        for alias in &actor.aliases {
            self.aliases.insert(alias.clone(), id.clone());
        }
        if !self.order.contains(&id) {
            self.order.push(id.clone());
        }
        self.actors.insert(id, actor);
    }

    /// Forget an actor that never became visible, or was rolled back.
    pub(super) fn remove_actor(&mut self, id: &ActorId) {
        let Some(actor) = self.actors.remove(id) else {
            return;
        };
        self.order.retain(|known| known != id);
        self.by_session.retain(|_, owner| owner != id);
        self.aliases.retain(|_, owner| owner != id);
        for task in actor.tasks {
            self.task_owner.remove(&task.id);
        }
    }

    /// Make `alias` name `id`. Returns the actor that already had it, when there is one.
    pub(super) fn add_alias(&mut self, id: &ActorId, alias: &str) -> Option<ActorId> {
        let alias = normalize_alias(alias);
        if alias.is_empty() {
            return None;
        }
        if let Some(existing) = self.aliases.get(alias)
            && existing != id
        {
            return Some(existing.clone());
        }
        if let Some(actor) = self.actors.get_mut(id)
            && !actor.aliases.iter().any(|known| known == alias)
        {
            actor.aliases.push(alias.to_owned());
            self.aliases.insert(alias.to_owned(), id.clone());
        }
        None
    }

    /// Remember that `session` is another name of `id`: the id of the session behind a child's
    /// view, so a provider's children announced on it are found under the child.
    pub(super) fn note_session(&mut self, id: &ActorId, session: acp::SessionId) {
        self.by_session.insert(session, id.clone());
    }

    /// A provider's child session was announced or changed state. Idempotent.
    #[expect(
        clippy::too_many_arguments,
        reason = "an announcement carries these independent facts"
    )]
    pub(super) fn observe_native(
        &mut self,
        session: &acp::SessionId,
        parent_session: &acp::SessionId,
        agent: Option<&str>,
        title: Option<&str>,
        model: Option<&str>,
        running: Option<bool>,
        ended: Option<SubagentOutcome>,
        backend: &Weak<crate::backend::AcpBackend>,
    ) -> ActorId {
        let parent = self.by_session.get(parent_session).cloned();
        let id = match self.by_session.get(session).cloned() {
            Some(id) => id,
            None => {
                let id = ActorId::new(session.clone());
                self.register(NewActor {
                    id: id.clone(),
                    kind: ActorKind::Native,
                    parent: parent.clone(),
                    announce_parent: None,
                    alias: None,
                    agent: agent.unwrap_or_default().to_owned(),
                    agent_name: agent.unwrap_or("provider child").to_owned(),
                    model: model.map(str::to_owned),
                    options: Vec::new(),
                    role: None,
                    scope: None,
                    title: title.unwrap_or_default().to_owned(),
                    worktree: None,
                    link: Link::Native {
                        backend: backend.clone(),
                        running: running.unwrap_or(false),
                    },
                    owner: None,
                });
                id
            }
        };
        let Some(actor) = self.actors.get_mut(&id) else {
            return id;
        };
        if actor.kind == ActorKind::Native {
            actor.native_backend = Some(backend.clone());
            if actor.parent.is_none() {
                actor.parent = parent;
            }
            if let Some(title) = title.filter(|title| !title.is_empty()) {
                actor.title = title.to_owned();
            }
            if let Some(model) = model {
                actor.model = Some(model.to_owned());
            }
            if let Some(agent) = agent.filter(|agent| !agent.is_empty()) {
                actor.agent = agent.to_owned();
                actor.agent_name = agent.to_owned();
            }
            match (ended, running) {
                (Some(outcome), _) => {
                    // What the provider child never read, it never will.
                    for task in &mut actor.tasks {
                        for message in &mut task.messages {
                            if message.state == nori_protocol::MessageDelivery::Queued {
                                message.state = nori_protocol::MessageDelivery::Undeliverable;
                                message.failure =
                                    Some("the provider's child session ended first".to_owned());
                            }
                        }
                    }
                    actor.link = Link::Gone(outcome);
                }
                (None, Some(running_now)) => {
                    actor.link = Link::Native {
                        backend: backend.clone(),
                        running: running_now,
                    };
                    if running_now {
                        for task in &mut actor.tasks {
                            for message in &mut task.messages {
                                if message.state == nori_protocol::MessageDelivery::Queued {
                                    message.state = nori_protocol::MessageDelivery::Delivered;
                                }
                            }
                        }
                    }
                }
                (None, None) => {}
            }
        }
        id
    }

    /// Mint the next task number.
    pub(super) fn next_task_id(&mut self) -> TaskId {
        self.next_task += 1;
        TaskId::numbered(self.next_task)
    }

    /// Mint the next message number.
    pub(super) fn next_message(&mut self) -> String {
        self.next_message += 1;
        format!("msg-{}", self.next_message)
    }

    pub(super) fn task(&self, id: &TaskId) -> Option<(&Actor, &Task)> {
        let actor = self.actors.get(self.task_owner.get(id)?)?;
        actor
            .tasks
            .iter()
            .find(|task| &task.id == id)
            .map(|task| (actor, task))
    }

    pub(super) fn task_mut(&mut self, id: &TaskId) -> Option<(&mut Actor, usize)> {
        let owner = self.task_owner.get(id)?.clone();
        let actor = self.actors.get_mut(&owner)?;
        let index = actor.tasks.iter().position(|task| &task.id == id)?;
        Some((actor, index))
    }

    /// Every task requested by `party`, oldest first within each actor.
    pub(super) fn tasks_requested_by<'a>(
        &'a self,
        party: &'a Party,
    ) -> impl Iterator<Item = (&'a Actor, &'a Task)> + 'a {
        self.order
            .iter()
            .filter_map(|id| self.actors.get(id))
            .flat_map(|actor| actor.tasks.iter().map(move |task| (actor, task)))
            .filter(move |(_, task)| &task.requester == party)
    }

    /// The id a session is shown under in the root's view: a child's own id when the root's
    /// session started it, and otherwise the id each relaying hop between the root and the actor
    /// scoped it to.
    pub(super) fn public_id(&self, id: &ActorId) -> acp::SessionId {
        let Some(actor) = self.actors.get(id) else {
            return id.session().clone();
        };
        let mut public = actor.wire.clone();
        let mut cursor = actor.parent.clone();
        let mut hops = 0;
        while let Some(parent_id) = cursor
            && hops < 64
        {
            hops += 1;
            let Some(parent) = self.actors.get(&parent_id) else {
                break;
            };
            if parent.kind == ActorKind::Root {
                break;
            }
            // A provider child's parent is the session behind its managed owner: scoped by it.
            public = super::child::scoped_id(&parent.wire, &public);
            cursor = parent.parent.clone();
        }
        public
    }

    /// Where the user's view should hear about the actor: its own session, as the root sees it.
    pub(super) fn label(&self, id: &ActorId) -> String {
        self.actors
            .get(id)
            .map_or_else(|| id.to_string(), Actor::label)
    }
}

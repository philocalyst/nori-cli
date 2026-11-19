//! The actor directory in the root conversation's journal, so it survives a resume.
//!
//! The journal is event-sourced. Each change is appended as one small typed delta
//! (`actor-delta`): an actor registered, its session started or ended, one task enqueued or
//! changed (started, ended, a message advancing, a result read), a digest pinned. Every
//! `SNAPSHOT_EVERY` deltas, when the root is compacted, and when the session shuts down, the
//! whole tree is written as one compact `actor-snapshot`. Restoring folds the latest snapshot
//! and the deltas after it, in journal order; rewound records are not visible and so are not
//! folded. A delta costs the size of one task, never the size of an actor's history.
//!
//! Only the text of a stored result is capped; the full text of what a child said stays in the
//! child's own conversation journal, which each record names.
//!
//! On resume every restored actor is *ended*, never running: no session of its old run survives.
//! Nothing is spawned. The first message to an actor revives it (see `Inner::revive`) with its
//! history, and delivers whatever mail was still queued for it first, in order. Messages already
//! delivered are never delivered again; a turn that was in flight when the old run ended is
//! reported as failed, with that cause. Transcripts without these records load as before.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Weak;
use std::time::Instant;

use nori_conversation::Event;
use nori_protocol::MessageDelivery;
use nori_protocol::SubagentOutcome;
use nori_protocol::acp::v1 as acp;
use serde::Deserialize;
use serde::Serialize;

use super::Inner;
use super::Signal;
use super::compaction::ActorDigest;
use super::directory::ActorId;
use super::directory::ActorKind;
use super::directory::Link;
use super::directory::NewActor;
use super::directory::Registry;
use super::fence::LeaseFence;
use super::task::Delivery;
use super::task::Message;
use super::task::Party;
use super::task::SharedProgress;
use super::task::Task;
use super::task::TaskEnd;
use super::task::TaskId;
use super::task::TaskStatus;
use super::task::Wake;
use super::task::stop_reason_label;

/// The command name of one change.
const DELTA: &str = "actor-delta";
/// The command name of a whole-tree snapshot.
const SNAPSHOT: &str = "actor-snapshot";
/// A snapshot is written after this many deltas, so restoring folds a bounded tail.
const SNAPSHOT_EVERY: i64 = 200;
/// Longest stored text of one task's result.
const MAX_RESULT_BYTES: usize = 64 * 1024;
/// How the root is named in records, so they do not depend on this run's root session id.
const ROOT: &str = "root";
const USER: &str = "user";

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct ActorRecord {
    pub(super) id: String,
    #[serde(default)]
    pub(super) root: bool,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    announce_parent: Option<String>,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    agent: String,
    #[serde(default)]
    agent_name: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    options: Vec<(String, String)>,
    #[serde(default)]
    role: Option<String>,
    /// What a program restricted the actor to, restored with it.
    #[serde(default)]
    scope: Option<super::ActorScope>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    worktree: Option<String>,
    /// The actor's own conversation journal: project and session.
    #[serde(default)]
    journal: Option<(String, String)>,
    /// How the session ended, when it had.
    #[serde(default)]
    ended: Option<SubagentOutcome>,
    #[serde(default)]
    digest: Option<ActorDigest>,
    #[serde(default)]
    tasks: Vec<TaskRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TaskRecord {
    id: String,
    requester: String,
    #[serde(default)]
    title: String,
    status: StatusRecord,
    #[serde(default)]
    result: String,
    delivery: DeliveryRecord,
    wake: WakeRecord,
    #[serde(default)]
    brief: Option<String>,
    #[serde(default)]
    merged_into: Option<String>,
    #[serde(default)]
    cc: Option<String>,
    #[serde(default)]
    cc_sent: bool,
    #[serde(default)]
    messages: Vec<MessageRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
struct MessageRecord {
    id: String,
    from: String,
    body: String,
    state: MessageDelivery,
    #[serde(default)]
    failure: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum StatusRecord {
    Queued,
    Dispatched,
    Running,
    Delivered,
    Completed { stop_reason: String },
    Failed { error: String },
    Cancelled,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DeliveryRecord {
    Owed,
    Delivered,
    Waived,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WakeRecord {
    Always,
    IfUnwaited,
    Never,
    AfterNextTurn,
}

/// A result as stored: capped, pointing at the actor's own journal for the rest.
fn clip_result(text: &str) -> String {
    if text.len() <= MAX_RESULT_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_RESULT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[truncated here; the full text is in the actor's own journal]",
        &text[..end]
    )
}

fn party_name(registry: &Registry, party: &Party) -> String {
    match party {
        Party::User => USER.to_owned(),
        Party::Actor(id) if registry.root.as_ref() == Some(id) => ROOT.to_owned(),
        Party::Actor(id) => id.to_string(),
    }
}

fn party_from(registry: &Registry, name: &str) -> Option<Party> {
    match name {
        USER => Some(Party::User),
        ROOT => registry.root.clone().map(Party::Actor),
        other => Some(Party::Actor(ActorId::new(acp::SessionId::new(other)))),
    }
}

fn actor_name(registry: &Registry, id: &ActorId) -> String {
    if registry.root.as_ref() == Some(id) {
        ROOT.to_owned()
    } else {
        id.to_string()
    }
}

/// What the directory knows of `id`, as a record; with its tasks for a snapshot.
fn actor_record(registry: &Registry, id: &ActorId, with_tasks: bool) -> Option<ActorRecord> {
    let actor = registry.actor(id)?;
    if actor.kind == ActorKind::Native {
        return None;
    }
    Some(ActorRecord {
        id: actor_name(registry, id),
        root: actor.kind == ActorKind::Root,
        parent: actor
            .parent
            .as_ref()
            .map(|parent| actor_name(registry, parent)),
        announce_parent: actor.announce_parent.as_ref().map(ToString::to_string),
        aliases: actor.aliases.clone(),
        agent: actor.agent.clone(),
        agent_name: actor.agent_name.clone(),
        model: actor.model.clone(),
        options: actor.options.clone(),
        role: actor.role.clone(),
        scope: actor.scope.as_deref().cloned(),
        title: actor.title.clone(),
        worktree: actor
            .worktree
            .as_ref()
            .map(|path| path.display().to_string()),
        journal: actor.journal_ref.clone(),
        ended: ended_outcome(actor),
        digest: actor.digest.clone(),
        tasks: if with_tasks {
            actor
                .tasks
                .iter()
                .map(|task| task_record(registry, task))
                .collect()
        } else {
            Vec::new()
        },
    })
}

fn ended_outcome(actor: &super::directory::Actor) -> Option<SubagentOutcome> {
    match actor.presence() {
        super::directory::Presence::Ended(outcome) => Some(outcome),
        super::directory::Presence::Starting
        | super::directory::Presence::Idle
        | super::directory::Presence::Working { .. } => None,
    }
}

/// One change to the directory, as journaled.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "delta", rename_all = "snake_case")]
enum Delta {
    /// An actor exists, with its identity and no tasks yet.
    ActorRegistered { actor: Box<ActorRecord> },
    /// An actor's session started (`ended` is empty) or ended, and where its journal is.
    ActorState {
        actor: String,
        ended: Option<SubagentOutcome>,
        journal: Option<(String, String)>,
    },
    /// A task was queued on an actor.
    TaskEnqueued { actor: String, task: TaskRecord },
    /// A task started, ended, advanced a message, or had its result read.
    TaskChanged { actor: String, task: TaskRecord },
    /// What an actor reported last, pinned.
    DigestUpdated { actor: String, digest: ActorDigest },
}

#[derive(Debug, Serialize, Deserialize)]
struct Snapshot {
    actors: Vec<ActorRecord>,
}

/// Fold journal records, in order, into the actors they describe.
fn fold(records: impl IntoIterator<Item = Event>) -> Vec<ActorRecord> {
    let mut actors: BTreeMap<String, ActorRecord> = BTreeMap::new();
    for event in records {
        let Event::Command { name, payload } = event else {
            continue;
        };
        if name == SNAPSHOT {
            if let Ok(snapshot) = serde_json::from_str::<Snapshot>(&payload) {
                actors = snapshot
                    .actors
                    .into_iter()
                    .map(|actor| (actor.id.clone(), actor))
                    .collect();
            }
        } else if name == DELTA
            && let Ok(delta) = serde_json::from_str::<Delta>(&payload)
        {
            apply(&mut actors, delta);
        }
    }
    actors.into_values().collect()
}

fn apply(actors: &mut BTreeMap<String, ActorRecord>, delta: Delta) {
    // The root has no registration of its own: its mail is all there is of it.
    let entry = |actors: &mut BTreeMap<String, ActorRecord>, id: &str| {
        if id == ROOT && !actors.contains_key(ROOT) {
            actors.insert(
                ROOT.to_owned(),
                ActorRecord {
                    id: ROOT.to_owned(),
                    root: true,
                    parent: None,
                    announce_parent: None,
                    aliases: Vec::new(),
                    agent: String::new(),
                    agent_name: String::new(),
                    model: None,
                    options: Vec::new(),
                    role: None,
                    scope: None,
                    title: String::new(),
                    worktree: None,
                    journal: None,
                    ended: None,
                    digest: None,
                    tasks: Vec::new(),
                },
            );
        }
    };
    match delta {
        Delta::ActorRegistered { actor } => {
            actors.insert(actor.id.clone(), *actor);
        }
        Delta::ActorState {
            actor,
            ended,
            journal,
        } => {
            if let Some(record) = actors.get_mut(&actor) {
                record.ended = ended;
                if journal.is_some() {
                    record.journal = journal;
                }
            }
        }
        Delta::TaskEnqueued { actor, task } | Delta::TaskChanged { actor, task } => {
            entry(actors, &actor);
            if let Some(record) = actors.get_mut(&actor) {
                match record.tasks.iter_mut().find(|known| known.id == task.id) {
                    Some(known) => *known = task,
                    None => record.tasks.push(task),
                }
            }
        }
        Delta::DigestUpdated { actor, digest } => {
            if let Some(record) = actors.get_mut(&actor) {
                record.digest = Some(digest);
            }
        }
    }
}

fn task_record(registry: &Registry, task: &Task) -> TaskRecord {
    TaskRecord {
        id: task.id.to_string(),
        requester: party_name(registry, &task.requester),
        title: task.title.clone(),
        status: match &task.status {
            TaskStatus::Queued => StatusRecord::Queued,
            TaskStatus::Dispatched => StatusRecord::Dispatched,
            TaskStatus::Running => StatusRecord::Running,
            TaskStatus::Delivered => StatusRecord::Delivered,
            TaskStatus::Ended(TaskEnd::Completed { stop_reason }) => StatusRecord::Completed {
                stop_reason: stop_reason_label(*stop_reason).to_owned(),
            },
            TaskStatus::Ended(TaskEnd::Failed { error }) => StatusRecord::Failed {
                error: error.clone(),
            },
            TaskStatus::Ended(TaskEnd::Cancelled) => StatusRecord::Cancelled,
        },
        result: clip_result(&task.result),
        delivery: match task.delivery {
            Delivery::Owed => DeliveryRecord::Owed,
            Delivery::Delivered => DeliveryRecord::Delivered,
            Delivery::Waived => DeliveryRecord::Waived,
        },
        wake: match task.wake {
            Wake::Always => WakeRecord::Always,
            Wake::IfUnwaited => WakeRecord::IfUnwaited,
            Wake::Never => WakeRecord::Never,
            Wake::AfterNextTurn => WakeRecord::AfterNextTurn,
        },
        brief: task.brief.clone(),
        merged_into: task.merged_into.as_ref().map(ToString::to_string),
        cc: task.cc.as_ref().map(|actor| actor_name(registry, actor)),
        cc_sent: task.cc_sent,
        messages: task
            .messages
            .iter()
            .map(|message| MessageRecord {
                id: message.id.clone(),
                from: party_name(registry, &message.from),
                body: message.body.clone(),
                state: message.state,
                failure: message.failure.clone(),
            })
            .collect(),
    }
}

fn stop_reason_from(label: &str) -> acp::StopReason {
    match label {
        "max_tokens" => acp::StopReason::MaxTokens,
        "max_turn_requests" => acp::StopReason::MaxTurnRequests,
        "refusal" => acp::StopReason::Refusal,
        "cancelled" => acp::StopReason::Cancelled,
        _ => acp::StopReason::EndTurn,
    }
}

/// A task as it stands after a restart. Turns that were in flight did not survive.
fn restore_task(registry: &Registry, record: TaskRecord, rev: i64) -> Option<Task> {
    let requester = party_from(registry, &record.requester)?;
    let wake = match record.wake {
        WakeRecord::Always => Wake::Always,
        WakeRecord::IfUnwaited => Wake::IfUnwaited,
        WakeRecord::Never => Wake::Never,
        WakeRecord::AfterNextTurn => Wake::AfterNextTurn,
    };
    let mut delivery = match record.delivery {
        DeliveryRecord::Owed => Delivery::Owed,
        DeliveryRecord::Delivered => Delivery::Delivered,
        DeliveryRecord::Waived => Delivery::Waived,
    };
    let (status, result) = match record.status {
        StatusRecord::Queued => (TaskStatus::Queued, record.result),
        StatusRecord::Delivered => (TaskStatus::Delivered, record.result),
        StatusRecord::Completed { stop_reason } => (
            TaskStatus::Ended(TaskEnd::Completed {
                stop_reason: stop_reason_from(&stop_reason),
            }),
            record.result,
        ),
        StatusRecord::Failed { error } => {
            (TaskStatus::Ended(TaskEnd::Failed { error }), record.result)
        }
        StatusRecord::Cancelled => (TaskStatus::Ended(TaskEnd::Cancelled), record.result),
        StatusRecord::Dispatched | StatusRecord::Running => {
            // The messages in it were read, so they are not delivered again; the sender is told
            // the turn is lost.
            if wake == Wake::Never {
                delivery = Delivery::Waived;
            }
            (
                TaskStatus::Ended(TaskEnd::Failed {
                    error: "the actor's session ended with the previous run of Nori while it \
                            worked on this; send it a message to restart it"
                        .to_owned(),
                }),
                record.result,
            )
        }
    };
    let ended = matches!(status, TaskStatus::Ended(_) | TaskStatus::Delivered);
    Some(Task {
        id: TaskId::from_address(&record.id)?,
        requester,
        messages: record
            .messages
            .into_iter()
            .filter_map(|message| {
                Some(Message {
                    id: message.id,
                    from: party_from(registry, &message.from)?,
                    body: message.body,
                    content: None,
                    state: message.state,
                    failure: message.failure,
                })
            })
            .collect(),
        brief: record.brief,
        title: record.title,
        status,
        result,
        delivery: if ended { delivery } else { Delivery::Owed },
        wake,
        fence: LeaseFence { rev, attempt: 0 },
        leased_fence: None,
        awaiting_approval: false,
        pending_approvals: HashSet::new(),
        relayed_approvals: Vec::new(),
        created_at: Instant::now(),
        started_at: None,
        ended_at: ended.then(Instant::now),
        progress: SharedProgress::default(),
        merged_into: record.merged_into.and_then(|id| TaskId::from_address(&id)),
        cc: record
            .cc
            .as_deref()
            .and_then(|name| match party_from(registry, name)? {
                Party::Actor(actor) => Some(actor),
                Party::User => None,
            }),
        cc_sent: record.cc_sent,
    })
}

/// An actor brought back dormant: where it was announced and why it is not running.
pub(in crate::backend) struct RestoredActor {
    pub(in crate::backend) id: acp::SessionId,
    pub(in crate::backend) parent: acp::SessionId,
    pub(in crate::backend) journal: Option<(String, String)>,
    pub(in crate::backend) outcome: SubagentOutcome,
}

impl Registry {
    /// Bring the actors of `records` back, dormant, under `owner`. Actors already present (a
    /// fork adopted by the native restore) are kept as they are.
    fn restore(
        &mut self,
        records: Vec<ActorRecord>,
        owner: &Weak<Inner>,
        root_session: &acp::SessionId,
        rev: i64,
    ) -> (Vec<RestoredActor>, i64) {
        let mut restored = Vec::new();
        let mut revived_tasks = 0;
        let mut highest_task = self.next_task;
        let mut highest_message = self.next_message;
        for record in records {
            let numbers = record.tasks.iter().filter_map(|task| {
                TaskId::from_address(&task.id)
                    .and_then(|_| task.id.strip_prefix("task-")?.parse::<i64>().ok())
            });
            highest_task = numbers.fold(highest_task, i64::max);
            highest_message = record
                .tasks
                .iter()
                .flat_map(|task| task.messages.iter())
                .filter_map(|message| message.id.strip_prefix("msg-")?.parse::<i64>().ok())
                .fold(highest_message, i64::max);
            let target = if record.root {
                self.root.clone()
            } else {
                let id = ActorId::new(acp::SessionId::new(record.id.clone()));
                if self.by_session(id.session()).is_some() {
                    continue;
                }
                Some(id)
            };
            let Some(id) = target else { continue };
            let parent = record
                .parent
                .as_deref()
                .and_then(|name| match name {
                    ROOT => self.root.clone(),
                    other => Some(ActorId::new(acp::SessionId::new(other))),
                })
                .or_else(|| self.root.clone());
            let outcome = record.ended.clone().unwrap_or(SubagentOutcome::Closed);
            if !record.root {
                self.register(NewActor {
                    id: id.clone(),
                    kind: ActorKind::Managed,
                    parent: parent.clone(),
                    announce_parent: record.announce_parent.as_deref().map(acp::SessionId::new),
                    alias: None,
                    agent: record.agent.clone(),
                    agent_name: record.agent_name.clone(),
                    model: record.model.clone(),
                    options: record.options.clone(),
                    role: record.role.clone(),
                    scope: record.scope.clone().map(std::sync::Arc::new),
                    title: record.title.clone(),
                    worktree: record.worktree.as_deref().map(Into::into),
                    link: Link::Gone(outcome.clone()),
                    owner: Some(owner.clone()),
                });
                for alias in &record.aliases {
                    self.add_alias(&id, alias);
                }
            }
            let tasks: Vec<Task> = record
                .tasks
                .into_iter()
                .filter_map(|task| restore_task(self, task, rev))
                .collect();
            for task in &tasks {
                self.task_owner.insert(task.id.clone(), id.clone());
                if !task.status.is_ended() {
                    revived_tasks += 1;
                }
            }
            let parent_wire = parent
                .as_ref()
                .and_then(|parent| self.actor(parent))
                .map_or_else(|| root_session.clone(), |actor| actor.wire.clone());
            if let Some(actor) = self.actor_mut(&id) {
                let known: HashSet<TaskId> =
                    actor.tasks.iter().map(|task| task.id.clone()).collect();
                actor
                    .tasks
                    .extend(tasks.into_iter().filter(|task| !known.contains(&task.id)));
                if !record.root {
                    actor.digest = record.digest;
                    actor.journal_ref = record.journal.clone();
                    restored.push(RestoredActor {
                        id: id.session().clone(),
                        parent: actor.announce_parent.clone().unwrap_or(parent_wire),
                        journal: record.journal,
                        outcome,
                    });
                }
            }
        }
        self.next_task = highest_task;
        self.next_message = highest_message;
        (restored, revived_tasks)
    }
}

/// Serializes journal writes tree-wide, so deltas land in the order the changes happened, and
/// counts the deltas since the last snapshot.
#[derive(Debug, Default)]
pub(super) struct Journal(tokio::sync::Mutex<i64>);

impl Inner {
    /// Append the deltas `build` makes from the directory as it stands now. The gate is held from
    /// building to appending, so a later change is never journaled before an earlier one. After
    /// `SNAPSHOT_EVERY` deltas the whole tree is written as one snapshot. Nothing is written
    /// without a conversation log.
    async fn journal(&self, build: impl FnOnce(&mut Registry) -> Vec<Delta>) {
        let Some(backend) = self.journal_backend() else {
            return;
        };
        let mut since_snapshot = self.seed.tree.journal.0.lock().await;
        let deltas = self.directory().with(build);
        if deltas.is_empty() {
            return;
        }
        let Some(recorder) = backend.transcript_recorder.read().await.clone() else {
            return;
        };
        for delta in &deltas {
            let Ok(payload) = serde_json::to_string(delta) else {
                continue;
            };
            if let Err(error) = recorder.record_command(DELTA, payload).await {
                tracing::warn!(%error, "failed to journal a change to the delegation tree");
                return;
            }
            *since_snapshot += 1;
        }
        if *since_snapshot >= SNAPSHOT_EVERY {
            self.write_snapshot(&recorder, &mut since_snapshot).await;
        }
    }

    async fn write_snapshot(
        &self,
        recorder: &crate::transcript::TranscriptRecorder,
        since_snapshot: &mut i64,
    ) {
        let snapshot = self.directory().with(|registry| Snapshot {
            actors: registry
                .order
                .clone()
                .iter()
                .filter_map(|id| actor_record(registry, id, true))
                .collect(),
        });
        let Ok(payload) = serde_json::to_string(&snapshot) else {
            return;
        };
        match recorder.record_command(SNAPSHOT, payload).await {
            Ok(()) => *since_snapshot = 0,
            Err(error) => tracing::warn!(%error, "failed to snapshot the delegation tree"),
        }
    }

    /// Write the whole tree as a snapshot now: at compaction and at session end.
    pub(super) async fn journal_snapshot(&self) {
        let Some(backend) = self.journal_backend() else {
            return;
        };
        let mut since_snapshot = self.seed.tree.journal.0.lock().await;
        if *since_snapshot == 0 {
            return;
        }
        if let Some(recorder) = backend.transcript_recorder.read().await.clone() {
            self.write_snapshot(&recorder, &mut since_snapshot).await;
        }
    }

    /// An actor now exists.
    pub(super) async fn journal_registered(&self, id: &ActorId) {
        self.journal(|registry| {
            actor_record(registry, id, false)
                .map(|actor| Delta::ActorRegistered {
                    actor: Box::new(actor),
                })
                .into_iter()
                .collect()
        })
        .await;
    }

    /// An actor's session started or ended, or its journal was bound.
    pub(super) async fn journal_state(&self, id: &ActorId) {
        self.journal(|registry| {
            let Some(actor) = registry
                .actor(id)
                .filter(|actor| actor.kind != ActorKind::Native)
            else {
                return Vec::new();
            };
            vec![Delta::ActorState {
                actor: actor_name(registry, id),
                ended: ended_outcome(actor),
                journal: actor.journal_ref.clone(),
            }]
        })
        .await;
    }

    /// What an actor reported last was pinned.
    pub(super) async fn journal_digest(&self, id: &ActorId) {
        self.journal(|registry| {
            registry
                .actor(id)
                .and_then(|actor| actor.digest.clone())
                .map(|digest| Delta::DigestUpdated {
                    actor: actor_name(registry, id),
                    digest,
                })
                .into_iter()
                .collect()
        })
        .await;
    }

    /// Tasks were queued (`enqueued`) or changed.
    pub(super) async fn journal_tasks(&self, ids: &[TaskId], enqueued: bool) {
        if ids.is_empty() {
            return;
        }
        self.journal(|registry| {
            ids.iter()
                .filter_map(|id| {
                    let (actor, task) = registry.task(id)?;
                    if actor.kind == ActorKind::Native {
                        return None;
                    }
                    let actor = actor_name(registry, &actor.id);
                    let task = task_record(registry, task);
                    Some(if enqueued {
                        Delta::TaskEnqueued { actor, task }
                    } else {
                        Delta::TaskChanged { actor, task }
                    })
                })
                .collect()
        })
        .await;
    }
}

impl super::Orchestrator {
    /// Restore the actors of a resumed root session, dormant. Mail still queued and results still
    /// owed are signalled so they are delivered once the session is idle.
    async fn restore_actors(&self, records: Vec<ActorRecord>) -> Vec<RestoredActor> {
        let Some(root_session) = self.inner.parent_session() else {
            return Vec::new();
        };
        let owner = Arc::downgrade(&self.inner);
        let rev = self
            .inner
            .generation
            .load(std::sync::atomic::Ordering::Acquire);
        let (restored, revived) = self
            .inner
            .directory()
            .with(|registry| registry.restore(records, &owner, &root_session, rev));
        for _ in 0..revived {
            self.inner.count_running_task();
        }
        let (owed, mail) = self.inner.directory().with(|registry| {
            let owed = registry
                .actors
                .values()
                .flat_map(|actor| actor.tasks.iter())
                .any(|task| {
                    matches!(task.status, TaskStatus::Ended(_))
                        && task.delivery == Delivery::Owed
                        && task.wake == Wake::Always
                });
            let mail = registry.root().is_some_and(|root| {
                root.tasks
                    .iter()
                    .any(|task| task.status == TaskStatus::Queued)
            });
            (owed, mail)
        });
        if owed {
            self.inner.signal(Signal::ResultsOwed);
        }
        if mail {
            self.inner.signal(Signal::MailboxWake);
        }
        self.inner.directory().touch();
        restored
    }
}

impl crate::backend::AcpBackend {
    /// Bring the delegation tree back from the conversation's journal after a resume. A failure
    /// is logged and the session starts without it.
    pub(in crate::backend) async fn restore_actor_directory(
        &self,
        recorder: &crate::transcript::TranscriptRecorder,
    ) -> Vec<crate::backend::native_child_restore::RestoredChild> {
        // The directory of a conversation is restored into that conversation only. A delegated
        // child's transcript is never a root: resumed as one it would start a tree of its own
        // while the real root's actors waited, unrestored, in a journal nobody opened.
        let loader = crate::transcript::TranscriptLoader::new(self.nori_home.clone());
        if loader
            .load_transcript(recorder.project_id(), recorder.session_id())
            .await
            .is_ok_and(|transcript| transcript.meta.delegated)
        {
            tracing::warn!(
                session = recorder.session_id(),
                "refusing to restore a delegation tree into a delegated child's transcript"
            );
            return Vec::new();
        }
        let records = match recorder.visible_records().await {
            Ok(records) => records,
            Err(error) => {
                tracing::warn!(%error, "could not read the delegation tree from the journal");
                return Vec::new();
            }
        };
        let latest = fold(records.into_iter().map(|record| record.event));
        if latest.is_empty() {
            return Vec::new();
        }
        let restored = self.orchestrator.restore_actors(latest).await;
        let mut children = Vec::new();
        for actor in restored {
            let events = match &actor.journal {
                Some((project, session)) => {
                    let recorder = async {
                        let transcript = loader.load_transcript(project, session).await.ok()?;
                        crate::transcript::TranscriptRecorder::continue_session(
                            loader.session_path(project, session),
                            &transcript.meta,
                        )
                        .await
                        .ok()
                    }
                    .await;
                    match recorder {
                        Some(recorder) => recorder
                            .replay_child_session_events()
                            .await
                            .unwrap_or_default(),
                        None => Vec::new(),
                    }
                }
                None => Vec::new(),
            };
            children.push(crate::backend::native_child_restore::RestoredChild {
                retired: vec![actor.id.clone()],
                id: actor.id,
                parent: actor.parent,
                events,
                state: nori_protocol::SubagentState::Ended(actor.outcome),
            });
        }
        children
    }
}

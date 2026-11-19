//! How actors and their tasks move through their lives: who is asking, a turn starting, a turn
//! ending, a session ending, and the user stopping everything.

use std::sync::Arc;

use nori_protocol::MessageDelivery;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::SubagentMessage;
use nori_protocol::SubagentOutcome;
use nori_protocol::SubagentState;
use nori_protocol::SubagentUpdate;
use nori_protocol::SubagentWire;
use nori_protocol::acp::v1 as acp;

use super::Inner;
use super::Signal;
use super::compaction;
use super::directory::Actor;
use super::directory::ActorId;
use super::directory::ActorKind;
use super::directory::Caller;
use super::directory::Link;
use super::directory::Registry;
use super::fence::FenceLedger;
use super::task::Delivery;
use super::task::Party;
use super::task::Task;
use super::task::TaskEnd;
use super::task::TaskId;
use super::task::TaskStatus;
use super::task::Wake;

impl Inner {
    /// The actor this orchestrator's agent is: a provider-owned caller, a delegated child, or the
    /// root session.
    pub(super) fn me(&self) -> Result<ActorId, String> {
        let view = self.caller_view.as_ref().or(self.seed.actor_view.as_ref());
        self.directory()
            .with(|registry| match view {
                Some(view) => registry.by_session(view).cloned(),
                None => registry.root.clone(),
            })
            .ok_or_else(|| "this session is not ready to delegate yet; try again".to_owned())
    }

    pub(super) fn caller(&self) -> Result<Caller, String> {
        Ok(Caller { id: self.me()? })
    }

    /// The backend whose conversation journal records the tree's actors: the root's.
    pub(super) fn journal_backend(&self) -> Option<Arc<crate::backend::AcpBackend>> {
        self.directory()
            .with(|registry| {
                registry
                    .root_backend
                    .as_ref()
                    .and_then(std::sync::Weak::upgrade)
            })
            .or_else(|| self.backend())
    }

    /// The session an actor is announced under: the one it was forked under, else the current
    /// session of the orchestrator that runs it.
    pub(super) fn parent_for_view(&self, view: &acp::SessionId) -> Option<acp::SessionId> {
        self.directory()
            .with(|registry| {
                registry
                    .by_session(view)
                    .and_then(|id| registry.actor(id))
                    .and_then(|actor| actor.announce_parent.clone())
            })
            .or_else(|| self.parent_session())
    }

    /// How the user's view names a child in a notice: its alias and agent, or just the agent.
    pub(super) fn child_label(&self, view: &acp::SessionId, agent: &str) -> String {
        self.directory().with(|registry| {
            let actor = registry.by_session(view).and_then(|id| registry.actor(id));
            let agent = if agent.is_empty() {
                actor.map_or("child", |actor| actor.agent_name.as_str())
            } else {
                agent
            };
            match actor.and_then(|actor| actor.aliases.first()) {
                Some(alias) => format!("{alias} ({agent})"),
                None => agent.to_owned(),
            }
        })
    }

    /// Whether the task's requester is owed what comes of it.
    pub(super) fn expects_result(&self, task: &TaskId) -> bool {
        self.directory().with(|registry| {
            registry
                .task(task)
                .is_some_and(|(_, task)| task.wake != Wake::Never)
        })
    }

    /// The actors whose delegations this orchestrator answers for: its own agent, and for the
    /// root session the provider-owned callers that share its controls.
    pub(super) async fn scope(&self) -> Vec<ActorId> {
        let mut ids: Vec<ActorId> = self.me().into_iter().collect();
        if self.caller_view.is_none() {
            let views: Vec<_> = self.native_scopes.lock().await.keys().cloned().collect();
            self.directory().with(|registry| {
                ids.extend(
                    views
                        .iter()
                        .filter_map(|view| registry.by_session(view).cloned()),
                );
            });
        }
        ids
    }

    /// Tell the user's view where the actor stands.
    pub(super) async fn announce(&self, id: &ActorId, state: SubagentState) {
        let announcement = self.directory().with(|registry| {
            registry.actor(id).map(|actor| {
                (
                    actor.wire.clone(),
                    actor.announce_parent.clone(),
                    actor.agent.clone(),
                    actor.model.clone(),
                    actor.view_title(),
                    actor.title.clone(),
                )
            })
        });
        let Some((session_id, announce_parent, agent, model, title, description)) = announcement
        else {
            return;
        };
        let Some(parent_session_id) = announce_parent.or_else(|| self.parent_session()) else {
            return;
        };
        self.emit(SessionEvent::Nori(NoriEvent::Subagent(
            SubagentWire::Update(SubagentUpdate {
                parent_session_id,
                session_id,
                agent: nori_protocol::Patch::Set(agent),
                model: model.map_or(nori_protocol::Patch::Keep, nori_protocol::Patch::Set),
                title: nori_protocol::Patch::Set(title),
                description: nori_protocol::Patch::Set(description),
                state: nori_protocol::Patch::Set(state),
            }),
        )))
        .await;
    }

    /// Announce peer messages whose delivery state advanced, on the root's stream under the ids
    /// the user's view knows the two parties by.
    pub(super) async fn announce_messages(&self, events: Vec<SessionEvent>) {
        if events.is_empty() {
            return;
        }
        let root = self.directory().with(|registry| {
            registry.root().and_then(|root| match &root.link {
                Link::Root(owner) => owner.upgrade(),
                Link::Starting
                | Link::Child(_)
                | Link::Staged(_)
                | Link::Native { .. }
                | Link::Gone(_) => None,
            })
        });
        for event in events {
            match &root {
                Some(root) => root.emit(event).await,
                None => self.emit(event).await,
            }
        }
    }

    pub(super) async fn task_has_ended(&self, id: &TaskId) -> bool {
        self.directory().with(|registry| {
            registry
                .task(id)
                .is_none_or(|(_, task)| task.status.is_ended())
        })
    }

    /// A user interruption ends the current turn but keeps its child view available.
    pub(super) async fn user_interrupted(&self, id: &TaskId) {
        self.directory().with(|registry| {
            if let Some((actor, index)) = registry.task_mut(id) {
                actor.tasks[index].wake = Wake::Never;
            }
        });
    }

    /// The actor's turn that carries this task started: its messages are delivered.
    pub(super) async fn task_running(&self, id: &TaskId) {
        let started = self.directory().with(|registry| {
            let (actor, index) = registry.task_mut(id)?;
            let lead = actor.tasks[index].id.clone();
            let mut started_ids = Vec::new();
            for task in &mut actor.tasks {
                if task.id == lead || task.merged_into.as_ref() == Some(&lead) {
                    started_ids.push(task.id.clone());
                    if task.status == TaskStatus::Dispatched {
                        task.status = TaskStatus::Running;
                        task.started_at = Some(std::time::Instant::now());
                    }
                    for message in &mut task.messages {
                        message.state = MessageDelivery::Delivered;
                    }
                }
            }
            let actor_id = actor.id.clone();
            let events = message_events(registry, &actor_id, |task| {
                task.id == lead || task.merged_into.as_ref() == Some(&lead)
            });
            Some((actor_id, events, started_ids))
        });
        let Some((actor_id, events, started_ids)) = started else {
            return;
        };
        self.announce(&actor_id, SubagentState::Running).await;
        self.announce_messages(events).await;
        self.directory().touch();
        self.journal_tasks(&started_ids, false).await;
    }

    /// Record how a task ended, tell the user's view, pin the actor's digest, and owe the result
    /// to whoever asked for it unless a rewound conversation fenced it out. Then start the
    /// actor's next queued task.
    pub(super) async fn task_ended(&self, id: &TaskId, end: TaskEnd, final_text: String) {
        let current_rev = self.generation.load(std::sync::atomic::Ordering::Acquire);
        let Some(finished) = self
            .directory()
            .with(|registry| end_task(registry, id, end, final_text, current_rev))
        else {
            return;
        };
        for _ in 0..finished.ended {
            self.count_ended_task();
        }
        self.journal_tasks(&finished.ended_ids, false).await;
        self.journal_digest(&finished.actor).await;
        if !finished.withdrawn.is_empty() {
            self.signal(Signal::Withdraw(finished.withdrawn));
        }
        if let Some(outcome) = finished.outcome {
            let state = if finished.another_active {
                SubagentState::Running
            } else if finished.wake_never && !matches!(outcome, SubagentOutcome::Failed { .. }) {
                SubagentState::Idle
            } else {
                SubagentState::Ended(outcome)
            };
            self.announce(&finished.actor, state).await;
        }
        self.announce_messages(finished.events).await;
        self.directory().touch();
        if finished.wake {
            self.signal(Signal::ResultsOwed);
        }
        // Boxed: starting the next task can fail, which ends tasks.
        Box::pin(self.pump(&finished.actor)).await;
        Box::pin(self.settle_exchanges(&finished.actor)).await;
    }

    /// The actor's session ended: whatever it was asked and had not finished has failed, and it
    /// stays in the directory, ended, until a message revives it.
    pub(super) async fn actor_gone(&self, actor: &ActorId, outcome: SubagentOutcome) {
        let reason = match &outcome {
            SubagentOutcome::Failed { reason } => reason
                .clone()
                .unwrap_or_else(|| "the actor's session failed".to_owned()),
            SubagentOutcome::Completed { .. }
            | SubagentOutcome::Cancelled
            | SubagentOutcome::Closed => "the actor's session ended".to_owned(),
        };
        let ended: Option<(Vec<TaskId>, bool)> = self.directory().with(|registry| {
            let actor = registry.actor_mut(actor)?;
            if matches!(actor.link, Link::Gone(_)) {
                return None;
            }
            let unpublished = matches!(actor.link, Link::Staged(_));
            actor.link = Link::Gone(outcome.clone());
            Some((
                actor.open_tasks().map(|task| task.id.clone()).collect(),
                unpublished,
            ))
        });
        let Some((open, unpublished)) = ended else {
            return;
        };
        for id in open {
            self.task_ended(
                &id,
                TaskEnd::Failed {
                    error: reason.clone(),
                },
                String::new(),
            )
            .await;
        }
        // An adoption that was never published has no view to tell.
        if !unpublished {
            self.announce(actor, SubagentState::Ended(outcome)).await;
        }
        self.directory().touch();
        self.journal_state(actor).await;
    }

    /// Stop a task. A task that has ended keeps its result. A message batched into another
    /// task's turn is withdrawn alone: the turn goes on for the messages that share it, and only
    /// cancelling the turn's lead task interrupts it (and ends every message in it).
    pub(super) async fn cancel_task(&self, id: &TaskId) {
        let child = self.directory().with(|registry| {
            let (actor, task) = registry.task(id)?;
            if task.status.is_ended() || task.merged_into.is_some() {
                return None;
            }
            matches!(task.status, TaskStatus::Dispatched | TaskStatus::Running)
                .then(|| actor.child().cloned())
                .flatten()
        });
        self.task_ended(id, TaskEnd::Cancelled, String::new()).await;
        if let Some(child) = child {
            let _ = child.cancel().await;
        }
    }

    /// The user stopped the session: cancel every task its agents delegated, fence out in-flight
    /// wake leases, and hold completed results until the user's next turn so none are dropped.
    pub(super) async fn stop_delegations(&self) {
        let scope = self.scope().await;
        let running: Vec<TaskId> = self.directory().with(|registry| {
            let mut released = Vec::new();
            for id in &scope {
                if let Some(actor) = registry.actor_mut(id) {
                    released.extend(actor.ledger.invalidate_on_stop());
                }
            }
            let mut running = Vec::new();
            for actor in registry.actors.values_mut() {
                if !actor
                    .parent
                    .as_ref()
                    .is_some_and(|parent| scope.contains(parent))
                {
                    continue;
                }
                for task in &mut actor.tasks {
                    if !matches!(&task.requester, Party::Actor(id) if scope.contains(id)) {
                        continue;
                    }
                    if (task.leased_fence.take().is_some() || released.contains(&task.id))
                        && task.delivery == Delivery::Owed
                    {
                        task.wake = Wake::AfterNextTurn;
                    }
                    if task.status.is_ended() && task.delivery == Delivery::Owed {
                        task.wake = Wake::AfterNextTurn;
                    }
                    if !task.status.is_ended() {
                        running.push(task.id.clone());
                    }
                }
            }
            running
        });
        futures::future::join_all(running.iter().map(|task| self.cancel_task(task))).await;
        if !running.is_empty() {
            self.directory().with(|registry| {
                for id in &running {
                    if let Some((actor, index)) = registry.task_mut(id) {
                        let task = &mut actor.tasks[index];
                        if task.status.is_ended()
                            && task.delivery == Delivery::Owed
                            && task.wake != Wake::Never
                        {
                            task.wake = Wake::AfterNextTurn;
                        }
                    }
                }
            });
        }
    }
}

/// What ending a task changed, for the work that follows outside the registry's lock.
struct Finished {
    actor: ActorId,
    /// How the lead task ended, as the user's view shows it.
    outcome: Option<SubagentOutcome>,
    wake: bool,
    wake_never: bool,
    withdrawn: Vec<acp::RequestId>,
    another_active: bool,
    /// Tasks that ended, counting the ones merged into the lead.
    ended: i64,
    ended_ids: Vec<TaskId>,
    events: Vec<SessionEvent>,
}

fn end_task(
    registry: &mut Registry,
    id: &TaskId,
    end: TaskEnd,
    final_text: String,
    current_rev: i64,
) -> Option<Finished> {
    let (actor, index) = registry.task_mut(id)?;
    if actor.tasks[index].status.is_ended() {
        return None;
    }
    let completed = matches!(end, TaskEnd::Completed { .. });
    let now = std::time::Instant::now();
    let mut ended = 0;
    let mut ended_ids = Vec::new();
    let mut withdrawn = Vec::new();
    let (mut wake, mut wake_never, mut outcome) = (false, false, None);
    for position in 0..actor.tasks.len() {
        let task = &mut actor.tasks[position];
        let lead = task.id == *id;
        if !lead && task.merged_into.as_ref() != Some(id) {
            continue;
        }
        if task.status.is_ended() {
            continue;
        }
        let fenced_out = FenceLedger::is_task_fenced_out(task.fence, current_rev);
        task.result.clone_from(&final_text);
        task.delivery = if fenced_out {
            Delivery::Waived
        } else {
            match (task.wake, &end) {
                (Wake::Never, _) | (_, TaskEnd::Cancelled) => Delivery::Waived,
                (_, TaskEnd::Completed { .. } | TaskEnd::Failed { .. }) => Delivery::Owed,
            }
        };
        task.status = TaskStatus::Ended(end.clone());
        task.ended_at = Some(now);
        task.leased_fence = None;
        task.pending_approvals.clear();
        task.awaiting_approval = false;
        if completed {
            for message in &mut task.messages {
                message.state = MessageDelivery::Acknowledged;
            }
        } else {
            // A message no turn read will never be read: it ends here, undeliverable, with the
            // reason, so nothing waits as `queued` forever. One a turn did read stays delivered.
            let reason = match &end {
                TaskEnd::Failed { error } => error.clone(),
                TaskEnd::Cancelled | TaskEnd::Completed { .. } => {
                    "cancelled before the actor read it".to_owned()
                }
            };
            for message in &mut task.messages {
                if message.state == MessageDelivery::Queued {
                    message.state = MessageDelivery::Undeliverable;
                    message.failure = Some(reason.clone());
                }
            }
        }
        withdrawn.append(&mut task.relayed_approvals);
        ended += 1;
        ended_ids.push(task.id.clone());
        if lead {
            wake = !fenced_out && task.delivery == Delivery::Owed && task.wake == Wake::Always;
            wake_never = task.wake == Wake::Never;
            outcome = task.end_outcome();
        }
    }
    let another_active = actor.open_tasks().any(|task| task.merged_into.is_none());
    let actor_id = actor.id.clone();
    let digest = compaction::digest_of(actor, Some(id), None, None);
    if digest.fence.rev >= current_rev {
        actor.digest = Some(digest);
    }
    let events = message_events(registry, &actor_id, |task| {
        task.id == *id || task.merged_into.as_ref() == Some(id)
    });
    Some(Finished {
        actor: actor_id,
        outcome,
        wake,
        wake_never,
        withdrawn,
        another_active,
        ended,
        ended_ids,
        events,
    })
}

/// The wire events that report the current delivery state of the peer messages in the actor's
/// tasks selected by `include`.
pub(super) fn message_events(
    registry: &Registry,
    actor_id: &ActorId,
    include: impl Fn(&Task) -> bool,
) -> Vec<SessionEvent> {
    let Some(actor) = registry.actor(actor_id) else {
        return Vec::new();
    };
    let mut events = Vec::new();
    for task in actor.tasks.iter().filter(|task| include(task)) {
        for message in &task.messages {
            let Party::Actor(sender) = &message.from else {
                continue;
            };
            if is_delegation(actor, sender) {
                continue;
            }
            events.push(SessionEvent::Nori(NoriEvent::Subagent(
                SubagentWire::Message(SubagentMessage {
                    session_id: registry.public_id(actor_id),
                    message_id: Some(message.id.clone()),
                    sender_session_id: Some(registry.public_id(sender)),
                    recipient_session_id: Some(registry.public_id(actor_id)),
                    text: message.body.clone(),
                    chunk: false,
                    sender_label: Some(registry.label(sender)),
                    recipient_label: Some(actor.label()),
                    delivery: Some(message.state),
                    failure: message.failure.clone(),
                }),
            )));
        }
    }
    events
}

/// Whether `sender` is the actor that delegated `actor`: its tasks are delegation, not mail.
pub(super) fn is_delegation(actor: &Actor, sender: &ActorId) -> bool {
    actor.parent.as_ref() == Some(sender) && actor.kind != ActorKind::Root
}

impl super::Orchestrator {
    /// Whether the session's agents have work in flight, results not yet delivered, or mail
    /// queued for them: a rewind must wait for it, so nothing from the old conversation is
    /// delivered into the new one.
    pub(crate) async fn has_pending_work(&self) -> bool {
        let scope = self.inner.scope().await;
        self.inner.directory().with(|registry| {
            let mail_waiting = scope.iter().any(|id| {
                registry.actor(id).is_some_and(|actor| {
                    actor
                        .tasks
                        .iter()
                        .any(|task| task.status == TaskStatus::Queued)
                })
            });
            mail_waiting
                || registry
                    .actors
                    .values()
                    .filter(|actor| {
                        actor
                            .parent
                            .as_ref()
                            .is_some_and(|parent| scope.contains(parent))
                    })
                    .flat_map(|actor| actor.tasks.iter())
                    .any(|task| {
                        !task.status.is_ended()
                            || (matches!(task.status, TaskStatus::Ended(_))
                                && task.delivery == Delivery::Owed
                                && task.wake != Wake::Never)
                    })
        })
    }
}

/// What the session's agents still owe, counted: the same work [`Orchestrator::has_pending_work`]
/// waits for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PendingCounts {
    /// Tasks that have not ended.
    pub(crate) running_tasks: i64,
    /// Ended tasks whose results have not been handed to their caller and will wake it.
    pub(crate) owed_results: i64,
    /// Messages queued for the session's actors.
    pub(crate) queued_mail: i64,
}

impl super::Orchestrator {
    /// Count the work in flight, the results owed, and the mail queued.
    pub(crate) async fn pending_counts(&self) -> PendingCounts {
        let scope = self.inner.scope().await;
        self.inner.directory().with(|registry| {
            let queued_mail = scope
                .iter()
                .filter_map(|id| registry.actor(id))
                .flat_map(|actor| actor.tasks.iter())
                .filter(|task| task.status == TaskStatus::Queued)
                .count();
            let delegated = registry
                .actors
                .values()
                .filter(|actor| {
                    actor
                        .parent
                        .as_ref()
                        .is_some_and(|parent| scope.contains(parent))
                })
                .flat_map(|actor| actor.tasks.iter())
                .collect::<Vec<_>>();
            let running_tasks = delegated
                .iter()
                .filter(|task| !task.status.is_ended())
                .count();
            let owed_results = delegated
                .iter()
                .filter(|task| {
                    matches!(task.status, TaskStatus::Ended(_))
                        && task.delivery == Delivery::Owed
                        && task.wake != Wake::Never
                })
                .count();
            PendingCounts {
                running_tasks: i64::try_from(running_tasks).unwrap_or(i64::MAX),
                owed_results: i64::try_from(owed_results).unwrap_or(i64::MAX),
                queued_mail: i64::try_from(queued_mail).unwrap_or(i64::MAX),
            }
        })
    }
}

impl super::Orchestrator {
    /// The root session's turn ledger reports a phase: the root is working while any turn is
    /// open, whoever owns it. Only the root session's own orchestrator publishes it.
    pub(crate) fn publish_root_phase(&self, busy: bool) {
        if self.inner.seed.is_child() || self.inner.caller_view.is_some() {
            return;
        }
        let changed = self.inner.directory().with(|registry| {
            let Some(root) = registry.root.clone() else {
                return false;
            };
            let Some(actor) = registry.actor_mut(&root) else {
                return false;
            };
            let changed = actor.busy != busy;
            actor.busy = busy;
            changed
        });
        if changed {
            self.inner.directory().touch();
        }
    }
}

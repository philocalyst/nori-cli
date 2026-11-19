//! Sending: the one path by which a message reaches an actor, whoever sends it.
//!
//! A message becomes a task queued on the actor. An idle actor takes it at once; a working one at
//! the end of its turn (a steering message interrupts the turn first); an ended one is revived
//! first, with its history. Peers' messages are batched into one turn. Nothing here waits for a
//! peer to reply.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::Ordering;
use std::time::Duration;

use nori_protocol::MessageDelivery;
use nori_protocol::acp::v1 as acp;

use super::DEFAULT_WAIT;
use super::Inner;
use super::Signal;
use super::brief;
use super::child;
use super::delegation::Mode;
use super::directory::ActorId;
use super::directory::Link;
use super::directory::NewTask;
use super::directory::Presence;
use super::directory::Registry;
use super::lifecycle;
use super::report::InboxItem;
use super::report::TaskSnapshot;
use super::spawn::Restored;
use super::status::Waiter;
use super::task::Delivery;
use super::task::Party;
use super::task::TaskEnd;
use super::task::TaskId;
use super::task::TaskStatus;
use super::task::Wake;
use super::text;

/// How many peer messages one turn takes.
const MAX_BATCH: usize = 16;

/// A message to send.
pub(super) struct Outgoing {
    pub(super) from: Party,
    pub(super) body: String,
    /// The user's typed content, with attachments.
    pub(super) content: Option<Vec<acp::ContentBlock>>,
    pub(super) mode: Mode,
    pub(super) timeout: Option<Duration>,
    /// Interrupt the actor's current turn so the message is taken next.
    pub(super) steer: bool,
    /// Tell the sender's parent when the exchange this message starts (or continues) settles.
    pub(super) cc_parent: bool,
}

/// What `pump` decided to do for an actor, found under the registry's lock and done after it.
enum Plan {
    Run(child::Child, child::Turn),
    Native {
        backend: Weak<crate::backend::AcpBackend>,
        wire: acp::SessionId,
        items: Vec<(TaskId, Vec<acp::ContentBlock>)>,
        steer: bool,
    },
    Root(Weak<Inner>),
    Nothing,
}

pub(super) fn wake_for(mode: Mode) -> Wake {
    match mode {
        Mode::Background => Wake::Always,
        Mode::Wait => Wake::IfUnwaited,
    }
}

impl Inner {
    /// Send `outgoing` to `to`, as a delegation of the sending actor's session: the call is
    /// refused when the conversation was rewound before it could start.
    pub(super) async fn send(
        self: &Arc<Self>,
        to: &ActorId,
        outgoing: Outgoing,
    ) -> Result<TaskSnapshot, String> {
        let generation = self.generation.load(Ordering::Acquire);
        let _admission = self.admission.lock().await;
        if generation != self.generation.load(Ordering::Acquire) {
            return Err("The conversation was rewound before the message was sent".to_owned());
        }
        self.send_locked(to, outgoing).await
    }

    /// Send a message from this orchestrator's agent to whatever `to` names: a `task-N` of its
    /// own, an alias, `actor://alias`, or a session id.
    pub(super) async fn send_to_address(
        self: &Arc<Self>,
        to: &str,
        body: String,
        mode: Mode,
        timeout: Option<Duration>,
        steer: bool,
        cc_parent: bool,
    ) -> Result<TaskSnapshot, String> {
        let caller = self.caller()?;
        let target = self
            .directory()
            .with(|registry| registry.resolve(to, &caller))
            .map_err(String::from)?;
        self.send(
            &target.actor,
            Outgoing {
                from: Party::Actor(caller.id),
                body,
                content: None,
                mode,
                timeout,
                steer,
                cc_parent,
            },
        )
        .await
    }

    /// [`Inner::send`] for a caller that already holds the admission gate.
    pub(super) async fn send_locked(
        self: &Arc<Self>,
        to: &ActorId,
        outgoing: Outgoing,
    ) -> Result<TaskSnapshot, String> {
        if outgoing.body.trim().is_empty() && outgoing.content.is_none() {
            return Err("`message` is empty: say what the actor should do".to_owned());
        }
        if self.shut_down.load(Ordering::Acquire) {
            return Err("this session is ending".to_owned());
        }
        if outgoing.from == Party::Actor(to.clone()) {
            return Err("that address is you; a message needs another actor".to_owned());
        }
        let rev = self.generation.load(Ordering::Acquire);
        let target = self
            .directory()
            .with(|registry| {
                registry.actor(to).map(|actor| {
                    (
                        actor.presence(),
                        matches!(&outgoing.from, Party::Actor(from) if lifecycle::is_delegation(actor, from)),
                        actor.address(),
                    )
                })
            })
            .ok_or_else(|| "the actor no longer exists".to_owned())?;
        let (presence, delegation, address) = target;
        let wake = match (&outgoing.from, delegation) {
            (Party::Actor(_), true) => wake_for(outgoing.mode),
            (Party::Actor(_), false) | (Party::User, _) => Wake::Never,
        };
        let steering = outgoing.steer && matches!(presence, Presence::Working { .. });
        let title = brief::task_title(None, &outgoing.body);
        // The enqueue itself refuses an ended actor, so a message is never queued behind a
        // session that is already gone: it revives the actor first and tries again. Of several
        // senders that find the actor ended at once, exactly one restarts it.
        let mut restored = None;
        let mut attempts = 0;
        let (task, events, acked) = loop {
            let attempt = self.directory().with(|registry| {
                if matches!(registry.actor(to)?.presence(), Presence::Ended(_)) {
                    return Some(None);
                }
                let mut events = Vec::new();
                let mut acked = Vec::new();
                if let Party::Actor(sender) = &outgoing.from {
                    let (changed, tasks) = ack_replies(registry, sender, to);
                    events.extend(changed);
                    acked = tasks;
                }
                let task = registry.enqueue_task(
                    to,
                    NewTask {
                        requester: outgoing.from.clone(),
                        body: outgoing.body.clone(),
                        content: outgoing.content.clone(),
                        brief: None,
                        title: title.clone(),
                        wake,
                        front: steering,
                        cc: match &outgoing.from {
                            Party::Actor(sender) if !delegation => {
                                exchange_cc(registry, sender, to, outgoing.cc_parent)
                            }
                            Party::Actor(_) | Party::User => None,
                        },
                    },
                    rev,
                )?;
                events.extend(lifecycle::message_events(registry, to, |queued| {
                    queued.id == task
                }));
                Some(Some((task, events, acked)))
            });
            match attempt {
                None => return Err("the actor no longer exists".to_owned()),
                Some(Some(queued)) => break queued,
                Some(None) => {
                    attempts += 1;
                    if attempts > 3 {
                        return Err(format!(
                            "`{address}` keeps ending as it restarts; read its failure with \
                             task_list and try a different actor"
                        ));
                    }
                    let outcome = self.revive(to, &address).await?;
                    restored = restored.or(Some(outcome));
                }
            }
        };
        self.count_running_task();
        self.announce_messages(events).await;
        self.directory().touch();
        self.emit_user_queue(to).await;
        self.journal_tasks(std::slice::from_ref(&task), true).await;
        self.journal_tasks(&acked, false).await;
        if steering {
            self.interrupt(to).await;
        }
        self.pump_steering(to, steering).await;
        let mut snapshot = self.snapshot(&task)?;
        match restored {
            Some(Restored::History) => snapshot.outcome = Some("revived"),
            Some(Restored::Nothing) => {
                snapshot.outcome = Some("revived");
                snapshot.next = Some(text::REVIVED_WITHOUT_HISTORY.to_owned());
            }
            // Another sender restarted it first; this message simply joined the queue.
            Some(Restored::AlreadyStarted) | None => {}
        }
        self.after_start(&task, outgoing.mode, outgoing.timeout, snapshot)
            .await
    }

    /// Interrupt the actor's current turn: its queued tasks are taken next.
    pub(super) async fn interrupt(&self, actor: &ActorId) {
        let child = self.directory().with(|registry| {
            registry
                .actor(actor)
                .and_then(|actor| actor.child().cloned())
        });
        if let Some(child) = child {
            let _ = child.cancel().await;
        }
    }

    pub(super) async fn pump(&self, actor: &ActorId) {
        self.pump_steering(actor, false).await;
    }

    /// Hand the actor the work waiting for it, when it can take it: one turn at a time for a
    /// session Nori drives, everything for a provider child (which queues by itself), and a wake
    /// for the root session.
    pub(super) async fn pump_steering(&self, actor: &ActorId, steer: bool) {
        let plan = self
            .directory()
            .with(|registry| plan_dispatch(registry, actor, steer));
        self.emit_user_queue(actor).await;
        match plan {
            Plan::Nothing => {}
            Plan::Root(owner) => {
                if let Some(owner) = owner.upgrade() {
                    owner.signal(Signal::MailboxWake);
                }
            }
            Plan::Run(child, turn) => {
                let lead = turn.task.clone();
                if let Err(error) = child.run(turn) {
                    let _ = lead;
                    self.actor_gone(
                        actor,
                        nori_protocol::SubagentOutcome::Failed {
                            reason: Some(error),
                        },
                    )
                    .await;
                }
            }
            Plan::Native {
                backend,
                wire,
                items,
                steer,
            } => {
                let Some(backend) = backend.upgrade() else {
                    for (task, _) in items {
                        self.task_ended(
                            &task,
                            TaskEnd::Failed {
                                error: "the provider session is gone".to_owned(),
                            },
                            String::new(),
                        )
                        .await;
                    }
                    return;
                };
                for (index, (task, content)) in items.into_iter().enumerate() {
                    let sent = if steer && index == 0 {
                        backend.native_children.steer(&wire, content).await
                    } else {
                        backend.native_children.enqueue(&wire, content).await
                    };
                    match sent {
                        Ok(()) => self.handed_over(&task).await,
                        Err(error) => {
                            self.task_ended(
                                &task,
                                TaskEnd::Failed {
                                    error: error.to_string(),
                                },
                                String::new(),
                            )
                            .await;
                        }
                    }
                }
            }
        }
    }

    /// A message went to a recipient whose turns Nori does not follow: it is as delivered as it
    /// can be known to be.
    pub(super) async fn handed_over(&self, task: &TaskId) {
        let handed = self.directory().with(|registry| {
            let (actor, index) = registry.task_mut(task)?;
            let running = matches!(&actor.link, Link::Native { running: true, .. });
            let actor_id = actor.id.clone();
            let handed = &mut actor.tasks[index];
            handed.status = TaskStatus::Delivered;
            handed.delivery = Delivery::Waived;
            for message in &mut handed.messages {
                message.state = if running {
                    MessageDelivery::Queued
                } else {
                    MessageDelivery::Delivered
                };
            }
            let events = lifecycle::message_events(registry, &actor_id, |each| each.id == *task);
            Some((actor_id, events))
        });
        self.count_ended_task();
        if let Some((actor_id, events)) = handed {
            self.announce_messages(events).await;
            let _ = actor_id;
            self.journal_tasks(std::slice::from_ref(task), false).await;
        }
        self.directory().touch();
    }

    /// Take the messages waiting in the caller's own mailbox, so they arrive in the result of the
    /// tool call it is making instead of after its turn: a reply must never sit queued while its
    /// recipient waits for it.
    pub(super) async fn take_inbox(&self) -> Vec<InboxItem> {
        let Ok(me) = self.me() else {
            return Vec::new();
        };
        self.take_inbox_of(&me).await
    }

    /// Take the peer messages queued on `actor`, marking them delivered.
    pub(super) async fn take_inbox_of(&self, actor: &ActorId) -> Vec<InboxItem> {
        let (items, events, taken) = self.directory().with(|registry| {
            let mut senders = HashMap::new();
            for entry in registry.actors.values() {
                senders.insert(entry.id.clone(), entry.address());
            }
            let Some(target) = registry.actor_mut(actor) else {
                return (Vec::new(), Vec::new(), Vec::new());
            };
            let mut items = Vec::new();
            let mut taken = Vec::new();
            for task in &mut target.tasks {
                if task.status != TaskStatus::Queued
                    || task.wake != Wake::Never
                    || task
                        .messages
                        .iter()
                        .any(|message| message.content.is_some())
                {
                    continue;
                }
                let Some(Party::Actor(_)) =
                    task.messages.first().map(|message| message.from.clone())
                else {
                    continue;
                };
                task.status = TaskStatus::Delivered;
                task.delivery = Delivery::Waived;
                taken.push(task.id.clone());
                for message in &mut task.messages {
                    message.state = MessageDelivery::Delivered;
                    if let Party::Actor(sender) = &message.from {
                        items.push(InboxItem {
                            from: senders
                                .get(sender)
                                .cloned()
                                .unwrap_or_else(|| sender.to_string()),
                            message_id: message.id.clone(),
                            message: message.body.clone(),
                            reply_with: text::reply_hint(
                                senders
                                    .get(sender)
                                    .map_or(sender.to_string().as_str(), String::as_str),
                            ),
                        });
                    }
                }
            }
            let events =
                lifecycle::message_events(registry, actor, |task| taken.contains(&task.id));
            (items, events, taken)
        });
        for _ in &items {
            self.count_ended_task();
        }
        self.announce_messages(events).await;
        if !items.is_empty() {
            self.directory().touch();
            self.journal_tasks(&taken, false).await;
        }
        items
    }

    /// Whether messages from peers wait for the caller.
    pub(super) fn has_inbox(&self) -> bool {
        let Ok(me) = self.me() else { return false };
        self.directory().with(|registry| {
            registry.actor(&me).is_some_and(|actor| {
                actor.tasks.iter().any(|task| {
                    task.status == TaskStatus::Queued
                        && task.wake == Wake::Never
                        && matches!(
                            task.messages.first().map(|message| &message.from),
                            Some(Party::Actor(_))
                        )
                })
            })
        })
    }

    /// Return messages taken for a wake that could not be sent to the queue.
    pub(super) fn restore_inbox(&self, actor: &ActorId, message_ids: &[String]) {
        self.directory().with(|registry| {
            let Some(target) = registry.actor_mut(actor) else {
                return;
            };
            for task in &mut target.tasks {
                if task.status == TaskStatus::Delivered
                    && task
                        .messages
                        .iter()
                        .any(|message| message_ids.contains(&message.id))
                {
                    task.status = TaskStatus::Queued;
                    task.delivery = Delivery::Owed;
                    for message in &mut task.messages {
                        message.state = MessageDelivery::Queued;
                    }
                }
            }
        });
        for _ in message_ids {
            self.count_running_task();
        }
    }

    /// Back from a call that started a task: the snapshot, or the task's result after waiting.
    pub(super) async fn after_start(
        &self,
        task: &TaskId,
        mode: Mode,
        timeout: Option<Duration>,
        snapshot: TaskSnapshot,
    ) -> Result<TaskSnapshot, String> {
        match mode {
            Mode::Background => {
                let mut snapshot = snapshot;
                if snapshot.in_flight() {
                    snapshot.next = Some(text::DELIVERED_LATER.to_owned());
                }
                Ok(snapshot)
            }
            Mode::Wait => {
                let mut waited = self
                    .wait_on(
                        std::slice::from_ref(task),
                        timeout.unwrap_or(DEFAULT_WAIT),
                        Waiter::Agent,
                    )
                    .await?;
                let mut result = waited.tasks.remove(0);
                result.outcome = snapshot.outcome;
                Ok(result)
            }
        }
    }
}

/// The parent to tell when the exchange between `sender` and `to` settles: the sender's, when it
/// asked for that, or when the message answers one that did (a reply stays in the exchange).
fn exchange_cc(
    registry: &Registry,
    sender: &ActorId,
    to: &ActorId,
    asked: bool,
) -> Option<ActorId> {
    let actor = registry.actor(sender)?;
    let parent = actor.parent.clone().filter(|parent| parent != to)?;
    let inherited = actor.tasks.iter().any(|task| {
        task.requester == Party::Actor(to.clone())
            && task.cc.as_ref() == Some(&parent)
            && !task.cc_sent
    });
    (asked || inherited).then_some(parent)
}

/// Mark the messages `replier` was sent by `to` as acknowledged: answering is acknowledging.
fn ack_replies(
    registry: &mut Registry,
    replier: &ActorId,
    to: &ActorId,
) -> (Vec<nori_protocol::SessionEvent>, Vec<TaskId>) {
    let Some(actor) = registry.actor_mut(replier) else {
        return (Vec::new(), Vec::new());
    };
    let mut changed = Vec::new();
    for task in &mut actor.tasks {
        for message in &mut task.messages {
            if message.from == Party::Actor(to.clone())
                && message.state == MessageDelivery::Delivered
            {
                message.state = MessageDelivery::Acknowledged;
                changed.push(task.id.clone());
            }
        }
    }
    let events = lifecycle::message_events(registry, replier, |task| changed.contains(&task.id));
    (events, changed)
}

/// Decide what to hand the actor, and mark it handed.
fn plan_dispatch(registry: &mut Registry, actor_id: &ActorId, steer: bool) -> Plan {
    let addresses: HashMap<ActorId, String> = registry
        .actors
        .values()
        .map(|actor| (actor.id.clone(), actor.address()))
        .collect();
    let Some(actor) = registry.actor_mut(actor_id) else {
        return Plan::Nothing;
    };
    let queued: Vec<usize> = actor
        .tasks
        .iter()
        .enumerate()
        .filter(|(_, task)| task.status == TaskStatus::Queued)
        .map(|(index, _)| index)
        .collect();
    if queued.is_empty() {
        return Plan::Nothing;
    }
    let sender = |party: &Party| match party {
        Party::User => None,
        Party::Actor(id) => Some(addresses.get(id).cloned().unwrap_or_else(|| id.to_string())),
    };
    match &actor.link {
        Link::Starting | Link::Staged(_) | Link::Gone(_) => Plan::Nothing,
        Link::Root(owner) => Plan::Root(owner.clone()),
        Link::Native { backend, .. } => {
            let backend = backend.clone();
            let items = queued
                .into_iter()
                .map(|index| {
                    let task = &mut actor.tasks[index];
                    task.status = TaskStatus::Dispatched;
                    let content = task.messages[0].content.clone().unwrap_or_else(|| {
                        vec![acp::ContentBlock::Text(acp::TextContent::new(
                            text::turn_prompt(
                                task.brief.as_deref(),
                                &task
                                    .messages
                                    .iter()
                                    .map(|message| text::Rendered {
                                        from: sender(&message.from),
                                        id: message.id.clone(),
                                        body: message.body.clone(),
                                    })
                                    .collect::<Vec<_>>(),
                            ),
                        ))]
                    });
                    (task.id.clone(), content)
                })
                .collect();
            Plan::Native {
                backend,
                wire: actor.wire.clone(),
                items,
                steer,
            }
        }
        Link::Child(child) => {
            if actor
                .tasks
                .iter()
                .any(|task| matches!(task.status, TaskStatus::Dispatched | TaskStatus::Running))
            {
                return Plan::Nothing;
            }
            let child = child.clone();
            let lead = queued[0];
            let mergeable = |task: &super::task::Task| {
                task.wake == Wake::Never
                    && task.brief.is_none()
                    && matches!(task.requester, Party::Actor(_))
                    && task
                        .messages
                        .iter()
                        .all(|message| message.content.is_none())
            };
            let mut batch = vec![lead];
            if mergeable(&actor.tasks[lead]) {
                batch.extend(
                    queued[1..]
                        .iter()
                        .copied()
                        .filter(|index| mergeable(&actor.tasks[*index]))
                        .take(MAX_BATCH - 1),
                );
            }
            let lead_id = actor.tasks[lead].id.clone();
            let rendered: Vec<text::Rendered> = batch
                .iter()
                .flat_map(|index| actor.tasks[*index].messages.iter())
                .map(|message| text::Rendered {
                    from: sender(&message.from),
                    id: message.id.clone(),
                    body: message.body.clone(),
                })
                .collect();
            let delegated = matches!(&actor.tasks[lead].requester, Party::Actor(requester) if Some(requester) == actor.parent.as_ref());
            let prompt = if delegated || matches!(actor.tasks[lead].requester, Party::User) {
                actor.tasks[lead].brief.clone().unwrap_or_else(|| {
                    rendered
                        .first()
                        .map(|message| message.body.clone())
                        .unwrap_or_default()
                })
            } else {
                text::turn_prompt(actor.tasks[lead].brief.as_deref(), &rendered)
            };
            for (position, index) in batch.iter().enumerate() {
                let task = &mut actor.tasks[*index];
                task.status = TaskStatus::Dispatched;
                if position > 0 {
                    task.merged_into = Some(lead_id.clone());
                }
            }
            let progress = actor.tasks[lead].progress.clone();
            let content = actor.tasks[lead].messages[0].content.clone();
            Plan::Run(
                child,
                child::Turn {
                    task: lead_id,
                    view_id: actor.wire.clone(),
                    prompt,
                    content,
                    progress,
                },
            )
        }
    }
}

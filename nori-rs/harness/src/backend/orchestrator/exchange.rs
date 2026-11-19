//! Mail between a parent's descendants: what the parent can read about it on demand, and the one
//! notice it gets when an exchange it asked about (`cc_parent`) has settled.
//!
//! Reading never wakes the parent. A notice is ordinary mail from the actor whose turn ended last,
//! so the parent learns it at its next idle moment without polling.

use nori_protocol::MessageDelivery;
use serde::Serialize;

use super::Inner;
use super::directory::Actor;
use super::directory::ActorId;
use super::directory::NewTask;
use super::directory::Presence;
use super::directory::Registry;
use super::lifecycle;
use super::task::Party;
use super::task::TaskId;
use super::task::TaskStatus;
use super::task::Wake;
use super::task::delivery_label;

/// How many recent messages a listing shows.
const RECENT_MAIL: usize = 20;
/// How much of a message a listing shows.
const MAIL_PREVIEW_CHARS: usize = 160;

/// One peer message between two actors, as a parent reads it.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct MailEntry {
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) message_id: String,
    /// The start of the message.
    pub(crate) message: String,
    /// `queued`, `delivered` or `acknowledged`.
    pub(crate) delivery: &'static str,
}

fn is_descendant(registry: &Registry, actor: &ActorId, ancestor: &ActorId) -> bool {
    let mut cursor = registry.actor(actor).and_then(|entry| entry.parent.clone());
    for _ in 0..64 {
        match cursor {
            Some(parent) if &parent == ancestor => return true,
            Some(parent) => {
                cursor = registry
                    .actor(&parent)
                    .and_then(|entry| entry.parent.clone())
            }
            None => return false,
        }
    }
    false
}

/// The recent mail between `me`'s descendants (both ends below `me`), oldest first, optionally
/// only what involves `involving`; and how many of those messages are not acknowledged yet.
pub(super) fn mail_among_descendants(
    registry: &Registry,
    me: &ActorId,
    involving: Option<&ActorId>,
) -> (Vec<MailEntry>, i64) {
    let mut found: Vec<(i64, MailEntry)> = Vec::new();
    for recipient in registry.actors.values() {
        if !is_descendant(registry, &recipient.id, me) {
            continue;
        }
        for task in &recipient.tasks {
            for message in &task.messages {
                let Party::Actor(sender) = &message.from else {
                    continue;
                };
                if lifecycle::is_delegation(recipient, sender)
                    || !is_descendant(registry, sender, me)
                    || involving.is_some_and(|actor| actor != sender && *actor != recipient.id)
                {
                    continue;
                }
                let number = message
                    .id
                    .strip_prefix("msg-")
                    .and_then(|number| number.parse::<i64>().ok())
                    .unwrap_or(0);
                let mut preview: String = message.body.chars().take(MAIL_PREVIEW_CHARS).collect();
                if message.body.chars().count() > MAIL_PREVIEW_CHARS {
                    preview.push('…');
                }
                found.push((
                    number,
                    MailEntry {
                        from: registry.label(sender),
                        to: recipient.label(),
                        message_id: message.id.clone(),
                        message: preview,
                        delivery: delivery_label(message.state),
                    },
                ));
            }
        }
    }
    found.sort_by_key(|(number, _)| *number);
    let open = i64::try_from(
        found
            .iter()
            .filter(|(_, entry)| {
                entry.delivery != delivery_label(MessageDelivery::Acknowledged)
                    && entry.delivery != delivery_label(MessageDelivery::Undeliverable)
            })
            .count(),
    )
    .unwrap_or(i64::MAX);
    let skip = found.len().saturating_sub(RECENT_MAIL);
    (
        found
            .into_iter()
            .skip(skip)
            .map(|(_, entry)| entry)
            .collect(),
        open,
    )
}

/// A settled exchange: who to tell, from whom, and what.
struct Notice {
    parent: ActorId,
    from: ActorId,
    body: String,
    /// The tasks of the exchange, now marked as told.
    told: Vec<TaskId>,
}

fn busy(actor: &Actor) -> bool {
    matches!(
        actor.presence(),
        Presence::Working { .. } | Presence::Starting
    )
}

/// The exchanges `actor` just stopped being busy in that are now settled, marked as told.
///
/// An exchange is the mail, in both directions, between two actors that one parent asked to be
/// told about; it has settled when none of that mail is waiting or being worked on and neither
/// actor has a turn open.
fn settled_exchanges(registry: &mut Registry, actor: &ActorId) -> Vec<Notice> {
    // Each (parent, partner) the actor has an untold cc'd exchange with.
    let mut exchanges: Vec<(ActorId, ActorId)> = Vec::new();
    for entry in registry.actors.values() {
        for task in &entry.tasks {
            let (Some(parent), false, Party::Actor(requester)) =
                (&task.cc, task.cc_sent, &task.requester)
            else {
                continue;
            };
            let partner = if &entry.id == actor {
                requester.clone()
            } else if requester == actor {
                entry.id.clone()
            } else {
                continue;
            };
            if !exchanges.contains(&(parent.clone(), partner.clone())) {
                exchanges.push((parent.clone(), partner));
            }
        }
    }
    let mut notices = Vec::new();
    for (parent, partner) in exchanges {
        let pair = [actor.clone(), partner.clone()];
        let both_idle = pair
            .iter()
            .all(|id| registry.actor(id).is_some_and(|entry| !busy(entry)));
        let mut mail: Vec<(String, String, String)> = Vec::new();
        let mut open = false;
        let mut told: Vec<TaskId> = Vec::new();
        for entry in registry
            .actors
            .values()
            .filter(|entry| pair.contains(&entry.id))
        {
            for task in &entry.tasks {
                let Party::Actor(requester) = &task.requester else {
                    continue;
                };
                if !pair.contains(requester) || lifecycle::is_delegation(entry, requester) {
                    continue;
                }
                open |= matches!(
                    task.status,
                    TaskStatus::Queued | TaskStatus::Dispatched | TaskStatus::Running
                );
                if task.cc.as_ref() == Some(&parent) && !task.cc_sent {
                    told.push(task.id.clone());
                    mail.push((
                        registry.label(requester),
                        entry.label(),
                        task.messages
                            .first()
                            .map(|message| message.body.chars().take(100).collect())
                            .unwrap_or_default(),
                    ));
                }
            }
        }
        if open || !both_idle || told.is_empty() {
            continue;
        }
        for id in &told {
            if let Some((owner, index)) = registry.task_mut(id) {
                owner.tasks[index].cc_sent = true;
            }
        }
        let (first, second) = (registry.label(&pair[0]), registry.label(&pair[1]));
        let lines: Vec<String> = mail
            .iter()
            .map(|(from, to, text)| format!("- {from} -> {to}: {text}"))
            .collect();
        notices.push(Notice {
            parent,
            from: actor.clone(),
            body: format!(
                "The exchange between {first} and {second} has settled: {} message(s), none \
                 waiting, neither working. Read task_list's `mail` for delivery states.\n{}",
                mail.len(),
                lines.join("\n")
            ),
            told,
        });
    }
    notices
}

impl Inner {
    /// `actor` finished a turn: tell the parents of any exchange of its that has now settled.
    pub(super) async fn settle_exchanges(&self, actor: &ActorId) {
        let rev = self.generation.load(std::sync::atomic::Ordering::Acquire);
        let queued = self.directory().with(|registry| {
            let mut queued = Vec::new();
            for notice in settled_exchanges(registry, actor) {
                let Some(task) = registry.enqueue_task(
                    &notice.parent,
                    NewTask {
                        requester: Party::Actor(notice.from.clone()),
                        body: notice.body,
                        content: None,
                        brief: None,
                        title: "Exchange settled".to_owned(),
                        wake: Wake::Never,
                        front: false,
                        cc: None,
                    },
                    rev,
                ) else {
                    continue;
                };
                let events =
                    lifecycle::message_events(registry, &notice.parent, |each| each.id == task);
                queued.push((notice.parent, task, events, notice.told));
            }
            queued
        });
        for (parent, task, events, told) in queued {
            self.count_running_task();
            self.announce_messages(events).await;
            self.journal_tasks(std::slice::from_ref(&task), true).await;
            self.journal_tasks(&told, false).await;
            self.pump(&parent).await;
        }
        self.directory().touch();
    }
}

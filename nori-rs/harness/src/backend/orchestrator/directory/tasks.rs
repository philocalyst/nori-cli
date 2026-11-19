//! Queueing work for an actor.

use std::collections::HashSet;
use std::time::Instant;

use nori_protocol::MessageDelivery;
use nori_protocol::acp::v1 as acp;

use super::ActorId;
use super::Registry;
use crate::backend::orchestrator::task::Delivery;
use crate::backend::orchestrator::task::Message;
use crate::backend::orchestrator::task::Party;
use crate::backend::orchestrator::task::SharedProgress;
use crate::backend::orchestrator::task::Task;
use crate::backend::orchestrator::task::TaskId;
use crate::backend::orchestrator::task::TaskStatus;
use crate::backend::orchestrator::task::Wake;

/// A request to queue.
pub(in crate::backend::orchestrator) struct NewTask {
    pub(in crate::backend::orchestrator) requester: Party,
    pub(in crate::backend::orchestrator) body: String,
    pub(in crate::backend::orchestrator) content: Option<Vec<acp::ContentBlock>>,
    /// The whole prompt of a task that starts an actor.
    pub(in crate::backend::orchestrator) brief: Option<String>,
    pub(in crate::backend::orchestrator) title: String,
    pub(in crate::backend::orchestrator) wake: Wake,
    /// Ahead of the tasks already waiting: a steering message.
    pub(in crate::backend::orchestrator) front: bool,
    /// The sender's parent, told when the exchange settles.
    pub(in crate::backend::orchestrator) cc: Option<ActorId>,
}

impl Registry {
    /// Queue `new` on `actor`, admitted under revision `rev` of the requester's conversation.
    pub(in crate::backend::orchestrator) fn enqueue_task(
        &mut self,
        actor: &ActorId,
        new: NewTask,
        rev: i64,
    ) -> Option<TaskId> {
        if !self.actors.contains_key(actor) {
            return None;
        }
        let id = self.next_task_id();
        let message_id = self.next_message();
        let ledger_owner = match &new.requester {
            Party::Actor(requester) if self.actors.contains_key(requester) => requester.clone(),
            Party::Actor(_) | Party::User => actor.clone(),
        };
        let fence = self
            .actors
            .get_mut(&ledger_owner)?
            .ledger
            .next_task_fence(rev);
        let target = self.actors.get_mut(actor)?;
        let task = Task {
            id: id.clone(),
            requester: new.requester.clone(),
            messages: vec![Message {
                id: message_id,
                from: new.requester,
                body: new.body,
                content: new.content,
                state: MessageDelivery::Queued,
                failure: None,
            }],
            brief: new.brief,
            title: new.title,
            status: TaskStatus::Queued,
            result: String::new(),
            delivery: Delivery::Owed,
            wake: new.wake,
            fence,
            leased_fence: None,
            awaiting_approval: false,
            pending_approvals: HashSet::new(),
            relayed_approvals: Vec::new(),
            created_at: Instant::now(),
            started_at: None,
            ended_at: None,
            progress: SharedProgress::default(),
            merged_into: None,
            cc: new.cc,
            cc_sent: false,
        };
        let position = if new.front {
            target
                .tasks
                .iter()
                .position(|task| task.status == TaskStatus::Queued)
                .unwrap_or(target.tasks.len())
        } else {
            target.tasks.len()
        };
        target.tasks.insert(position, task);
        self.task_owner.insert(id.clone(), actor.clone());
        Some(id)
    }
}

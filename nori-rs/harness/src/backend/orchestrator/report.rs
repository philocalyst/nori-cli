//! What the task tools return: a task as its requester sees it, with where its messages stand.

use std::time::Instant;

use serde::Serialize;

use super::directory::Actor;
use super::directory::ActorView;
use super::directory::Presence;
use super::task::Task;
use super::task::TaskEnd;
use super::task::TaskId;
use super::task::TaskStatus;
use super::task::delivery_label;
use super::task::stop_reason_label;

/// A task as the tools report it, or an actor that has none of the caller's tasks.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct TaskSnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) task_id: Option<TaskId>,
    /// The actor's address: its alias, or its session id.
    pub(crate) actor: String,
    /// `starting`, `idle`, `working`, `ended`, `failed` or `cancelled`.
    pub(crate) actor_state: &'static str,
    pub(crate) agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    pub(crate) title: String,
    /// `queued`, `starting`, `running`, `delivered`, `completed`, `incomplete`, `failed`,
    /// `cancelled`, or `none` when the caller has no task on the actor.
    pub(crate) status: &'static str,
    /// How far the message that started the task got: `queued` (no turn of the actor has read
    /// it), `delivered` (a turn started with it) or `acknowledged` (that turn ended, or the
    /// actor replied), or `undeliverable` (see `delivery_failure`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) delivery: Option<&'static str>,
    /// Why the message is `undeliverable`: it will never be read, and nothing waits on it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) delivery_failure: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stop_reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
    /// The child's final message, once the task has ended.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) awaiting_approval: Option<bool>,
    /// While it runs: the child's latest tool call or line of text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) activity: Option<String>,
    pub(crate) tool_calls: i64,
    /// Files the child's edit tools changed; edits made by its shell commands are not seen.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) files_changed: Vec<String>,
    pub(crate) elapsed_seconds: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) worktree: Option<String>,
    /// What this call did: `started`, `reused` (the alias named a live actor), `revived`,
    /// `sent`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) outcome: Option<&'static str>,
    /// What to do next, when that is not obvious from the status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) next: Option<String>,
}

impl TaskSnapshot {
    /// The task has not finished and may still produce a result.
    pub(crate) fn in_flight(&self) -> bool {
        matches!(self.status, "queued" | "starting" | "running")
    }
}

impl Actor {
    /// `task` on this actor as its requester sees it; an actor with no task of theirs gets a
    /// snapshot of the actor alone.
    pub(super) fn snapshot(&self, task: Option<&Task>, now: Instant) -> TaskSnapshot {
        let mut snapshot = TaskSnapshot {
            task_id: None,
            actor: self.address(),
            actor_state: self.presence().label(),
            agent: self.agent.clone(),
            model: self.model.clone(),
            title: self.title.clone(),
            status: "none",
            delivery: None,
            delivery_failure: None,
            stop_reason: None,
            error: None,
            result: None,
            awaiting_approval: None,
            activity: None,
            tool_calls: 0,
            files_changed: Vec::new(),
            elapsed_seconds: 0,
            worktree: self
                .worktree
                .as_ref()
                .map(|path| path.display().to_string()),
            outcome: None,
            next: None,
        };
        let Some(task) = task else {
            return snapshot;
        };
        let elapsed = task.ended_at.unwrap_or(now) - task.started_at.unwrap_or(task.created_at);
        snapshot.task_id = Some(task.id.clone());
        snapshot.title.clone_from(&task.title);
        snapshot.status = task.status.label();
        snapshot.delivery = Some(delivery_label(task.delivery_state()));
        // Said where the sender looks: the status says why a message never arrived.
        if let Some(reason) = task
            .messages
            .iter()
            .find_map(|message| message.failure.clone())
        {
            snapshot.delivery_failure = Some(reason);
        }
        match &task.status {
            TaskStatus::Ended(TaskEnd::Completed { stop_reason }) => {
                snapshot.stop_reason = Some(stop_reason_label(*stop_reason));
            }
            TaskStatus::Ended(TaskEnd::Failed { error }) => snapshot.error = Some(error.clone()),
            TaskStatus::Queued
            | TaskStatus::Dispatched
            | TaskStatus::Running
            | TaskStatus::Delivered
            | TaskStatus::Ended(TaskEnd::Cancelled) => {}
        }
        if matches!(task.status, TaskStatus::Ended(_)) {
            snapshot.result = Some(task.result.clone());
        }
        snapshot.awaiting_approval = task.awaiting_approval.then_some(true);
        if let Ok(progress) = task.progress.lock() {
            if !task.status.is_ended() && !progress.activity.is_empty() {
                snapshot.activity = Some(progress.activity.clone());
            }
            snapshot.tool_calls = progress.tool_calls;
            snapshot.files_changed.clone_from(&progress.files_changed);
        }
        snapshot.elapsed_seconds = elapsed.as_secs_f64().round() as i64;
        snapshot
    }

    /// What a caller should do about the actor's state, when that is not obvious.
    pub(super) fn state_hint(&self) -> Option<String> {
        match self.presence() {
            Presence::Ended(_) => Some(super::text::ENDED_ACTOR.to_owned()),
            Presence::Starting | Presence::Idle | Presence::Working { .. } => None,
        }
    }
}

/// A message from a peer that arrived in the result of a tool call.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct InboxItem {
    /// The sender's address.
    pub(crate) from: String,
    pub(crate) message_id: String,
    pub(crate) message: String,
    /// How to answer it.
    pub(crate) reply_with: String,
}

/// `task_status`: one task, plus anything that arrived for the caller meanwhile.
#[derive(Debug, Serialize)]
pub(crate) struct TaskReport {
    #[serde(flatten)]
    pub(crate) snapshot: TaskSnapshot,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) inbox: Vec<InboxItem>,
    /// Recent mail between your descendants that involves this actor.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) mail: Vec<super::exchange::MailEntry>,
}

/// `task_wait`: the tasks waited on.
#[derive(Debug, Serialize)]
pub(crate) struct WaitReport {
    pub(crate) tasks: Vec<TaskSnapshot>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) inbox: Vec<InboxItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) next: Option<String>,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes a reference"
)]
fn is_zero(number: &i64) -> bool {
    *number == 0
}

/// `task_list`: who the caller can address, and what it has asked of them.
#[derive(Debug, Serialize)]
pub(crate) struct ListReport {
    pub(crate) actors: Vec<ActorView>,
    pub(crate) tasks: Vec<TaskSnapshot>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) inbox: Vec<InboxItem>,
    /// Recent mail between your descendants, with delivery states. Reading it wakes no one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) mail: Vec<super::exchange::MailEntry>,
    /// How many of those messages are not acknowledged yet: zero means the exchanges are over.
    #[serde(skip_serializing_if = "is_zero")]
    pub(crate) mail_open: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) next: Option<String>,
}

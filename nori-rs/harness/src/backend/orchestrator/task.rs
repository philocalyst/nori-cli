//! One request to an actor: a turn of work, from the message that asked for it to the result that
//! goes back to whoever asked.
//!
//! A task never exists on its own. It belongs to an [actor](super::directory::Actor): a child
//! session that outlives its tasks and is addressed by alias, so a follow-up is another task of
//! the same actor and never a new child.

use std::collections::HashSet;
use std::time::Instant;

use nori_protocol::MessageDelivery;
use nori_protocol::acp::v1 as acp;
use serde::Serialize;

use super::directory::ActorId;
use super::fence::LeaseFence;

/// A task's id as the agent that asked for it sees it: `task-1`, `task-2`, ... in the order they
/// were created, unique across the whole delegation tree.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct TaskId(String);

impl TaskId {
    pub(super) fn numbered(number: i64) -> Self {
        Self(format!("task-{number}"))
    }

    /// The id a `task-N` address names, when it is one.
    pub(super) fn from_address(text: &str) -> Option<Self> {
        let number = text.trim().strip_prefix("task-")?;
        (!number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| Self::numbered(number.parse().unwrap_or(i64::MAX)))
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Who a message or a task came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Party {
    /// The person at the terminal, typing into the actor's view.
    User,
    Actor(ActorId),
}

/// One thing said to an actor. Delivery advances `Queued -> Delivered -> Acknowledged` and is
/// reported to the sender; nothing is called received while it is only queued.
#[derive(Debug, Clone)]
pub(super) struct Message {
    pub(super) id: String,
    pub(super) from: Party,
    pub(super) body: String,
    /// The user's original typed content, with attachments.
    pub(super) content: Option<Vec<acp::ContentBlock>>,
    pub(super) state: MessageDelivery,
    /// Why it will never be delivered, once it is `Undeliverable`.
    pub(super) failure: Option<String>,
}

/// Where a task is in its life. A task that has ended never runs again: more work for the same
/// actor is a new task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TaskStatus {
    /// Accepted, waiting for the actor: it is starting, or busy with an earlier turn.
    Queued,
    /// Handed to the actor's session; the turn has not been confirmed to start yet.
    Dispatched,
    /// A turn of the actor that contains the request is in progress.
    Running,
    /// Handed to a recipient whose turns Nori does not follow (the root session, a provider's own
    /// child). There is no result to wait for.
    Delivered,
    Ended(TaskEnd),
}

/// How a task ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TaskEnd {
    Completed {
        stop_reason: acp::StopReason,
    },
    Failed {
        error: String,
    },
    /// Stopped by the requester, the user, or the session ending.
    Cancelled,
}

impl TaskStatus {
    /// Nothing more will happen to the task.
    pub(super) fn is_ended(&self) -> bool {
        matches!(self, Self::Ended(_) | Self::Delivered)
    }

    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Dispatched => "starting",
            Self::Running => "running",
            Self::Delivered => "delivered",
            Self::Ended(TaskEnd::Completed {
                stop_reason: acp::StopReason::EndTurn,
            }) => "completed",
            // Stopped by a limit or a refusal rather than finishing.
            Self::Ended(TaskEnd::Completed { .. }) => "incomplete",
            Self::Ended(TaskEnd::Failed { .. }) => "failed",
            Self::Ended(TaskEnd::Cancelled) => "cancelled",
        }
    }
}

/// Whether a finished task's result still has to reach the agent that asked for it.
///
/// Each result reaches the requester exactly once: as the answer to a tool call that read it, or in
/// a completion wake. A cancelled task is never delivered: whoever cancelled it knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Delivery {
    /// Not finished, or finished and not yet read.
    Owed,
    /// The requester has the result.
    Delivered,
    /// Nobody is owed the result.
    Waived,
}

/// When a finished task wakes its requester.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Wake {
    /// Whenever it finishes unread. The default for tasks that run in the background.
    Always,
    /// Only if the call waiting on it gave up first: a wait that returns the result delivers it.
    IfUnwaited,
    /// Nobody is told: messages to peers (the peer answers with its own message), user messages
    /// and history forks.
    Never,
    /// Held by the user stopping the requester; owed again once the user's next turn ends.
    AfterNextTurn,
}

/// What a running turn is doing, written by the actor's relay as the child's events arrive.
#[derive(Debug, Default)]
pub(super) struct Progress {
    /// The latest tool call's title, or the last line the child wrote.
    pub(super) activity: String,
    pub(super) tool_calls: i64,
    /// Files the child's tool calls changed, in the order they were first changed.
    pub(super) files_changed: Vec<String>,
}

pub(super) type SharedProgress = std::sync::Arc<std::sync::Mutex<Progress>>;

/// Everything known about one task.
#[derive(Debug)]
pub(super) struct Task {
    pub(super) id: TaskId,
    pub(super) requester: Party,
    /// What the task was asked, oldest first. Never empty.
    pub(super) messages: Vec<Message>,
    /// The whole prompt of a task that starts an actor: the brief, framed by its role. Later tasks
    /// are rendered from their messages when they are dispatched.
    pub(super) brief: Option<String>,
    pub(super) title: String,
    pub(super) status: TaskStatus,
    /// The child's final message: what it said after its last tool call, or its last words before
    /// it failed.
    pub(super) result: String,
    pub(super) delivery: Delivery,
    pub(super) wake: Wake,
    /// Monotonic fence under which this task was admitted.
    pub(super) fence: LeaseFence,
    /// Active completion wake lease fence when peeked for delivery.
    pub(super) leased_fence: Option<LeaseFence>,
    /// A permission request of the child's is waiting for the user.
    pub(super) awaiting_approval: bool,
    /// Distinct in-flight relayed permission requests awaiting user decision.
    pub(super) pending_approvals: HashSet<acp::RequestId>,
    /// The ids the child's permission requests were asked of the user under.
    pub(super) relayed_approvals: Vec<acp::RequestId>,
    pub(super) created_at: Instant,
    pub(super) started_at: Option<Instant>,
    pub(super) ended_at: Option<Instant>,
    pub(super) progress: SharedProgress,
    /// Delivered to the actor in the same turn as this earlier task, and ending with it.
    pub(super) merged_into: Option<TaskId>,
    /// The sender's parent, which is told once the exchange this message belongs to settles.
    pub(super) cc: Option<ActorId>,
    /// The parent was told already.
    pub(super) cc_sent: bool,
}

impl Task {
    /// The delivery state a sender sees for the task: its least advanced message.
    pub(super) fn delivery_state(&self) -> MessageDelivery {
        self.messages
            .iter()
            .map(|message| message.state)
            .min_by_key(|state| match state {
                MessageDelivery::Queued => 0,
                MessageDelivery::Delivered => 1,
                MessageDelivery::Acknowledged => 2,
                // The one a sender must notice: it sorts first.
                MessageDelivery::Undeliverable => -1,
            })
            .unwrap_or(MessageDelivery::Queued)
    }

    /// How the user's view shows the task's end, once it has ended.
    pub(super) fn end_outcome(&self) -> Option<nori_protocol::SubagentOutcome> {
        match &self.status {
            TaskStatus::Ended(TaskEnd::Completed { stop_reason }) => {
                Some(nori_protocol::SubagentOutcome::Completed {
                    stop_reason: Some(stop_reason_label(*stop_reason).to_owned()),
                })
            }
            TaskStatus::Ended(TaskEnd::Failed { error }) => {
                Some(nori_protocol::SubagentOutcome::Failed {
                    reason: Some(error.clone()),
                })
            }
            TaskStatus::Ended(TaskEnd::Cancelled) => {
                Some(nori_protocol::SubagentOutcome::Cancelled)
            }
            TaskStatus::Queued
            | TaskStatus::Dispatched
            | TaskStatus::Running
            | TaskStatus::Delivered => None,
        }
    }
}

/// The text of a delivery state, for the agents that read it.
pub(super) fn delivery_label(state: MessageDelivery) -> &'static str {
    match state {
        MessageDelivery::Queued => "queued",
        MessageDelivery::Delivered => "delivered",
        MessageDelivery::Acknowledged => "acknowledged",
        MessageDelivery::Undeliverable => "undeliverable",
    }
}

pub(super) fn stop_reason_label(reason: acp::StopReason) -> &'static str {
    match reason {
        acp::StopReason::EndTurn => "end_turn",
        acp::StopReason::MaxTokens => "max_tokens",
        acp::StopReason::MaxTurnRequests => "max_turn_requests",
        acp::StopReason::Refusal => "refusal",
        acp::StopReason::Cancelled => "cancelled",
        // The schema's stop reasons are not exhaustive.
        _ => "other",
    }
}

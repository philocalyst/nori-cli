//! Watching another actor: what a monitor with the `actor` source reads.
//!
//! The directory is the one registry of actors, and it announces every change. A watch holds the
//! tree and the two actors' ids; each observation reads the target's state, the tasks it was
//! given, and what it has said to the watching actor.

use std::sync::Arc;

use tokio::sync::watch;

use super::Orchestrator;
use super::Tree;
use super::directory::ActorId;
use super::directory::Presence;
use super::task::Party;
use super::task::TaskEnd;
use super::task::TaskStatus;
use nori_protocol::SubagentOutcome;

pub(crate) struct ActorWatch {
    tree: Arc<Tree>,
    target: ActorId,
    me: ActorId,
    changes: watch::Receiver<i64>,
}

/// How the watched actor stood at one moment.
pub(crate) struct Observation {
    pub(crate) label: String,
    pub(crate) state: &'static str,
    /// The actor's session ended or failed: nothing more happens until it is messaged.
    pub(crate) finished: bool,
    pub(crate) failed: bool,
    pub(crate) tasks: Vec<TaskObservation>,
    pub(crate) replies: Vec<Reply>,
}

/// One task of the watched actor.
pub(crate) struct TaskObservation {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) status: &'static str,
    pub(crate) ended: bool,
    pub(crate) failed: bool,
    /// The task's final message, or its error.
    pub(crate) detail: String,
}

/// A message the watched actor sent to the watching one.
pub(crate) struct Reply {
    pub(crate) id: String,
    pub(crate) body: String,
}

impl Orchestrator {
    /// Watch the actor `target` names (an alias, `actor://alias` or a session id). The error says
    /// which actors can be addressed.
    pub(crate) fn watch_actor(&self, target: &str) -> Result<ActorWatch, String> {
        let inner = self.inner();
        let caller = inner.caller()?;
        let me = caller.id.clone();
        let resolved = inner
            .directory()
            .with(|registry| registry.resolve(target, &caller))
            .map_err(String::from)?;
        if resolved.actor == me {
            return Err("that is you: a monitor watches another actor".to_owned());
        }
        let tree = Arc::clone(&inner.seed.tree);
        let changes = tree.directory.subscribe();
        Ok(ActorWatch {
            tree,
            target: resolved.actor,
            me,
            changes,
        })
    }
}

impl ActorWatch {
    /// Wait for the directory to change. `false` when it can change no more.
    pub(crate) async fn changed(&mut self) -> bool {
        self.changes.changed().await.is_ok()
    }

    pub(crate) fn observe(&self) -> Observation {
        self.tree.directory.with(|registry| {
            let Some(actor) = registry.actor(&self.target) else {
                return Observation {
                    label: self.target.to_string(),
                    state: "gone",
                    finished: true,
                    failed: true,
                    tasks: Vec::new(),
                    replies: Vec::new(),
                };
            };
            let presence = actor.presence();
            let failed = matches!(
                presence,
                Presence::Ended(SubagentOutcome::Failed { .. } | SubagentOutcome::Cancelled)
            );
            let tasks = actor
                .tasks
                .iter()
                .map(|task| {
                    let (failed, detail) = match &task.status {
                        TaskStatus::Ended(TaskEnd::Failed { error }) => (true, error.clone()),
                        TaskStatus::Ended(TaskEnd::Cancelled) => (true, String::new()),
                        TaskStatus::Ended(TaskEnd::Completed { .. }) => {
                            (false, task.result.clone())
                        }
                        TaskStatus::Queued
                        | TaskStatus::Dispatched
                        | TaskStatus::Running
                        | TaskStatus::Delivered => (false, String::new()),
                    };
                    TaskObservation {
                        id: task.id.to_string(),
                        title: task.title.clone(),
                        status: task.status.label(),
                        ended: matches!(task.status, TaskStatus::Ended(_)),
                        failed,
                        detail,
                    }
                })
                .collect();
            let replies = registry
                .actor(&self.me)
                .into_iter()
                .flat_map(|me| me.tasks.iter())
                .filter(|task| task.requester == Party::Actor(self.target.clone()))
                .flat_map(|task| task.messages.iter())
                .map(|message| Reply {
                    id: message.id.clone(),
                    body: message.body.clone(),
                })
                .collect();
            Observation {
                label: actor.label(),
                state: presence.label(),
                finished: matches!(presence, Presence::Ended(_)),
                failed,
                tasks,
                replies,
            }
        })
    }
}

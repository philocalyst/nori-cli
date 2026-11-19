//! The completion wake: how finished tasks reach an agent that is not waiting on them.
//!
//! A task whose requester is not blocked on it reaches the requester in a hidden turn once it is
//! idle. The turn is leased under a [`LeaseFence`], and the results count as delivered only when
//! the turn was durably submitted under that same fence, so a concurrent stop or rewind can
//! neither lose a result nor deliver a stale one.

use std::sync::atomic::Ordering;
use std::time::Instant;

use super::Orchestrator;
use super::Signal;
use super::WakeTurn;
use super::fence::FenceLedger;
use super::fence::LeaseFence;
use super::report::TaskSnapshot;
use super::task::Delivery;
use super::task::Party;
use super::task::TaskStatus;
use super::task::Wake;

/// Results are inlined in a wake until together they reach this size; the rest are named, to be
/// read with `task_status`.
const INLINE_RESULT_BYTES: usize = 16 * 1024;

/// The hidden prompt that hands an agent the results of tasks that finished unread.
pub(super) fn wake_text(tasks: impl Iterator<Item = TaskSnapshot>) -> String {
    let mut text = String::from(
        "<delegated_task_results>\nTasks you delegated have finished. Continue your work with \
         their results; the user sees each child's work already, so do not repeat it back in \
         full. To give a child more work, task_send to its actor address: it keeps its history.\n",
    );
    let mut inlined = 0;
    for task in tasks {
        let mut attributes = format!(
            "id=\"{}\" actor=\"{}\" agent=\"{}\" status=\"{}\"",
            task.task_id
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            escape(&task.actor),
            escape(&task.agent),
            task.status
        );
        if let Some(model) = &task.model {
            attributes.push_str(&format!(" model=\"{}\"", escape(model)));
        }
        if let Some(stop_reason) = task.stop_reason {
            attributes.push_str(&format!(" stop_reason=\"{stop_reason}\""));
        }
        attributes.push_str(&format!(" title=\"{}\"", escape(&task.title)));
        if let Some(worktree) = &task.worktree {
            attributes.push_str(&format!(" worktree=\"{}\"", escape(worktree)));
        }
        text.push_str(&format!("\n<task {attributes}>\n"));
        if let Some(error) = &task.error {
            text.push_str(&format!("Failed: {error}\n"));
        }
        if !task.files_changed.is_empty() {
            text.push_str(&format!(
                "Files changed: {}\n",
                task.files_changed.join(", ")
            ));
        }
        let result = task.result.as_deref().unwrap_or_default().trim();
        if result.is_empty() {
            text.push_str("(The child ended without a final message.)\n");
        } else if inlined + result.len() <= INLINE_RESULT_BYTES {
            inlined += result.len();
            text.push_str(result);
            text.push('\n');
        } else {
            text.push_str(&format!(
                "(Result not inlined: {} bytes. Read it with task_status.)\n",
                result.len()
            ));
        }
        text.push_str("</task>\n");
    }
    text.push_str("</delegated_task_results>");
    text
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
}

/// An in-flight wake lease bound to a monotonic [`LeaseFence`].
pub(crate) struct LeasedWake {
    pub(crate) fence: LeaseFence,
    pub(crate) wake: WakeTurn,
}

impl Orchestrator {
    /// Peek owed results under a monotonic [`LeaseFence`] without marking them delivered yet.
    /// Callers must commit via [`Orchestrator::commit_wake`] or release via [`Orchestrator::abort_wake`].
    pub(crate) async fn peek_wake(&self) -> Option<LeasedWake> {
        if self.inner.shut_down.load(Ordering::Acquire) {
            return None;
        }
        let me = self.inner.me().ok()?;
        let requester = Party::Actor(me.clone());
        let current_rev = self.inner.generation.load(Ordering::Acquire);
        self.inner.directory().with(|registry| {
            let owed: Vec<_> = registry
                .tasks_requested_by(&requester)
                .filter(|(_, task)| {
                    matches!(task.status, TaskStatus::Ended(_))
                        && task.delivery == Delivery::Owed
                        && task.wake == Wake::Always
                        && task.leased_fence.is_none()
                        && !FenceLedger::is_task_fenced_out(task.fence, current_rev)
                })
                .map(|(_, task)| task.id.clone())
                .collect();
            if owed.is_empty() {
                return None;
            }
            let fence = registry
                .actor_mut(&me)?
                .ledger
                .begin_wake(current_rev, owed.clone());
            let now = Instant::now();
            let mut snapshots = Vec::with_capacity(owed.len());
            let mut delivered = Vec::with_capacity(owed.len());
            for id in &owed {
                let (actor, task) = registry.task(id)?;
                snapshots.push(actor.snapshot(Some(task), now));
                delivered.push(nori_protocol::DeliveredTask {
                    task_id: id.to_string(),
                    title: task.title.clone(),
                    agent: actor.agent_name.clone(),
                });
            }
            for id in &owed {
                if let Some((actor, index)) = registry.task_mut(id) {
                    actor.tasks[index].leased_fence = Some(fence);
                }
            }
            Some(LeasedWake {
                fence,
                wake: WakeTurn {
                    text: wake_text(snapshots.into_iter()),
                    delivered: nori_protocol::TaskResultsDelivered { tasks: delivered },
                },
            })
        })
    }

    /// A completion wake, if results are owed: every owed result is peeked and committed under a fence.
    pub(crate) async fn take_wake(&self) -> Option<WakeTurn> {
        let leased = self.peek_wake().await?;
        if self.commit_wake(leased.fence).await.is_ok() {
            Some(leased.wake)
        } else {
            None
        }
    }

    /// Commit an in-flight wake lease after durable submission.
    /// Returns an error if a concurrent cancel, stop, or rewind fenced out `fence`.
    pub(crate) async fn commit_wake(&self, fence: LeaseFence) -> Result<(), String> {
        let me = self.inner.me()?;
        let current_rev = self.inner.generation.load(Ordering::Acquire);
        let mut delivered = Vec::new();
        let result = self.inner.directory().with(|registry| {
            let committed = registry
                .actor_mut(&me)
                .ok_or_else(|| "the session is gone".to_owned())?
                .ledger
                .commit_wake(fence, current_rev);
            match committed {
                Ok(leased) => {
                    for id in leased {
                        if let Some((actor, index)) = registry.task_mut(&id) {
                            let task = &mut actor.tasks[index];
                            if task.leased_fence == Some(fence) {
                                task.leased_fence = None;
                                if task.wake != Wake::AfterNextTurn
                                    && !FenceLedger::is_task_fenced_out(task.fence, current_rev)
                                {
                                    task.delivery = Delivery::Delivered;
                                    delivered.push(id.clone());
                                }
                            }
                        }
                    }
                    Ok(())
                }
                Err(current) => {
                    for actor in registry.actors.values_mut() {
                        for task in &mut actor.tasks {
                            if task.leased_fence == Some(fence) {
                                task.leased_fence = None;
                            }
                        }
                    }
                    Err(format!(
                        "wake lease {fence} fenced out by current fence {current}"
                    ))
                }
            }
        });
        // Results the wake handed over are not owed again after a restart.
        self.inner.journal_tasks(&delivered, false).await;
        result
    }

    /// Abort an in-flight wake lease so its owed verdicts remain deliverable on the next idle.
    pub(crate) async fn abort_wake(&self, fence: LeaseFence) {
        let Ok(me) = self.inner.me() else { return };
        let resignal = self.inner.directory().with(|registry| {
            let released = registry
                .actor_mut(&me)
                .map(|actor| actor.ledger.release_wake(fence))
                .unwrap_or_default();
            let mut resignal = false;
            for actor in registry.actors.values_mut() {
                for task in &mut actor.tasks {
                    if released.contains(&task.id) || task.leased_fence == Some(fence) {
                        task.leased_fence = None;
                        if task.delivery == Delivery::Owed && task.wake == Wake::Always {
                            resignal = true;
                        }
                    }
                }
            }
            resignal
        });
        if resignal && !self.inner.shut_down.load(Ordering::Acquire) {
            self.inner.signal(Signal::ResultsOwed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::orchestrator::task::TaskId;
    use pretty_assertions::assert_eq;

    fn finished(id: i64, result: &str) -> TaskSnapshot {
        TaskSnapshot {
            task_id: Some(TaskId::numbered(id)),
            actor: "reader".to_owned(),
            actor_state: "idle",
            agent: "codex".to_owned(),
            model: Some("GPT-5.1".to_owned()),
            title: "Read \"a.txt\"".to_owned(),
            status: "completed",
            delivery: Some("acknowledged"),
            delivery_failure: None,
            stop_reason: Some("end_turn"),
            error: None,
            result: Some(result.to_owned()),
            awaiting_approval: None,
            activity: None,
            tool_calls: 1,
            files_changed: Vec::new(),
            elapsed_seconds: 3,
            worktree: None,
            outcome: None,
            next: None,
        }
    }

    #[test]
    fn results_are_inlined_until_the_budget_is_spent() {
        let big = "x".repeat(INLINE_RESULT_BYTES);
        let text = wake_text([finished(1, "alpha"), finished(2, &big)].into_iter());
        assert!(text.contains(
            "<task id=\"task-1\" actor=\"reader\" agent=\"codex\" status=\"completed\" model=\"GPT-5.1\" stop_reason=\"end_turn\" title=\"Read &quot;a.txt&quot;\">\nalpha\n</task>"
        ));
        assert!(text.contains("(Result not inlined: 16384 bytes. Read it with task_status.)"));
        assert_eq!(text.lines().last(), Some("</delegated_task_results>"));
    }
}

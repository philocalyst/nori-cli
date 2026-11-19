//! Reading, waiting on and cancelling tasks, for the task tools.

use std::time::Duration;
use std::time::Instant;

use super::Inner;
use super::MAX_WAIT;
use super::directory::Registry;
use super::directory::Resolved;
use super::report::ListReport;
use super::report::TaskReport;
use super::report::TaskSnapshot;
use super::report::WaitReport;
use super::task::Delivery;
use super::task::Party;
use super::task::TaskId;
use super::task::Wake;
use super::text;

/// Who is waiting: a model, which must never hold its turn for something only a new turn can
/// deliver, or a conversation program's host, which waits for exactly what it asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Waiter {
    Agent,
    Program,
}

/// The snapshot a resolved address stands for: the task itself, or the actor's latest task of
/// `requester`'s (the one still open, when there is one).
fn snapshot_of(
    registry: &Registry,
    resolved: &Resolved,
    requester: &Party,
    now: Instant,
) -> Option<TaskSnapshot> {
    let actor = registry.actor(&resolved.actor)?;
    let task = match &resolved.task {
        Some(id) => actor.tasks.iter().find(|task| &task.id == id),
        None => actor
            .tasks
            .iter()
            .rev()
            .filter(|task| &task.requester == requester)
            .find(|task| !task.status.is_ended())
            .or_else(|| {
                actor
                    .tasks
                    .iter()
                    .rev()
                    .find(|task| &task.requester == requester)
            }),
    };
    let mut snapshot = actor.snapshot(task, now);
    snapshot.next = actor.state_hint().or_else(|| match task {
        Some(_) => text::next_for(&snapshot),
        None => Some(text::NO_TASK_ON_ACTOR.to_owned()),
    });
    Some(snapshot)
}

impl Inner {
    fn resolve_for_caller(&self, raw: &str) -> Result<Resolved, String> {
        let caller = self.caller()?;
        self.directory()
            .with(|registry| registry.resolve(raw, &caller))
            .map_err(String::from)
    }

    /// A task's snapshot.
    pub(super) fn snapshot(&self, id: &TaskId) -> Result<TaskSnapshot, String> {
        self.directory()
            .with(|registry| {
                registry
                    .task(id)
                    .map(|(actor, task)| actor.snapshot(Some(task), Instant::now()))
            })
            .ok_or_else(|| format!("there is no task `{id}`"))
    }

    /// An actor on its own, with no task.
    pub(super) fn actor_snapshot(
        &self,
        id: &super::directory::ActorId,
    ) -> Result<TaskSnapshot, String> {
        self.directory()
            .with(|registry| {
                registry
                    .actor(id)
                    .map(|actor| actor.snapshot(None, Instant::now()))
            })
            .ok_or_else(|| "the actor no longer exists".to_owned())
    }

    /// `task_status`: read one task, or an actor's latest task of the caller's. Reading a
    /// finished task delivers its result.
    pub(super) async fn status(&self, raw: &str) -> Result<TaskReport, String> {
        let resolved = self.resolve_for_caller(raw)?;
        let me = self.me()?;
        let requester = Party::Actor(me.clone());
        let mut snapshot = self
            .directory()
            .with(|registry| {
                let snapshot = snapshot_of(registry, &resolved, &requester, Instant::now())?;
                if snapshot.result.is_some()
                    && let Some(id) = &snapshot.task_id
                    && let Some((actor, index)) = registry.task_mut(id)
                    && actor.tasks[index].delivery == Delivery::Owed
                {
                    actor.tasks[index].delivery = Delivery::Delivered;
                }
                Some(snapshot)
            })
            .ok_or_else(|| format!("`{raw}` no longer exists"))?;
        if snapshot.result.is_some()
            && let Some(id) = &snapshot.task_id
        {
            self.journal_tasks(std::slice::from_ref(id), false).await;
        }
        let inbox = self.take_inbox().await;
        if let Some(stop) = self.poll_instruction(std::slice::from_ref(&snapshot), &inbox) {
            snapshot.next = Some(stop);
        }
        let mail = self.directory().with(|registry| {
            super::exchange::mail_among_descendants(registry, &me, Some(&resolved.actor)).0
        });
        Ok(TaskReport {
            snapshot,
            inbox,
            mail,
        })
    }

    /// `task_wait`: block until the tasks finish, one awaits approval, a message arrives for the
    /// caller, or the timeout passes. A task nothing will come back from is not waited on.
    pub(super) async fn wait(
        &self,
        raws: &[String],
        timeout: Duration,
    ) -> Result<WaitReport, String> {
        let requester = Party::Actor(self.me()?);
        let mut ids = Vec::new();
        let mut idle = Vec::new();
        for raw in raws {
            let resolved = self.resolve_for_caller(raw)?;
            let open = match &resolved.task {
                Some(id) => Some(id.clone()),
                None => self.directory().with(|registry| {
                    registry.actor(&resolved.actor).and_then(|actor| {
                        actor
                            .tasks
                            .iter()
                            .rev()
                            .find(|task| task.requester == requester && !task.status.is_ended())
                            .map(|task| task.id.clone())
                    })
                }),
            };
            match open {
                Some(id) => ids.push(id),
                None => {
                    if let Some(snapshot) = self.directory().with(|registry| {
                        snapshot_of(registry, &resolved, &requester, Instant::now())
                    }) {
                        idle.push(snapshot);
                    }
                }
            }
        }
        let mut report = self.wait_on(&ids, timeout, Waiter::Agent).await?;
        report.tasks.extend(idle);
        if let Some(stop) = self.poll_instruction(&report.tasks, &report.inbox) {
            report.next = Some(stop);
        }
        Ok(report)
    }

    /// A program host's wait on one task of its own: no model-facing shortcuts.
    pub(super) async fn wait_program(
        &self,
        raws: &[String],
        timeout: Duration,
    ) -> Result<WaitReport, String> {
        let mut ids = Vec::new();
        for raw in raws {
            ids.push(
                self.resolve_for_caller(raw)?
                    .task
                    .ok_or_else(|| format!("`{raw}` is not a task id (`task-N`)"))?,
            );
        }
        self.wait_on(&ids, timeout, Waiter::Program).await
    }

    /// Wait on tasks known by id. Ended tasks are delivered by this; those still running will
    /// wake their requester when they end.
    pub(super) async fn wait_on(
        &self,
        ids: &[TaskId],
        timeout: Duration,
        waiter: Waiter,
    ) -> Result<WaitReport, String> {
        let deadline = tokio::time::Instant::now() + timeout.min(MAX_WAIT);
        let mut changed = self.directory().subscribe();
        // Whatever happens to this call, a task it was waiting on is not lost: it wakes the
        // requester unless this call returns its result.
        self.directory().with(|registry| {
            for id in ids {
                registry
                    .task(id)
                    .ok_or_else(|| format!("there is no task `{id}`"))?;
                if let Some((actor, index)) = registry.task_mut(id)
                    && actor.tasks[index].wake != Wake::Never
                {
                    actor.tasks[index].wake = Wake::Always;
                }
            }
            Ok::<(), String>(())
        })?;
        let dependent;
        let timed_out;
        loop {
            // A child waiting for the user's approval cannot finish until the user answers, so
            // the waiter hears of it now. A task nothing comes back from would never settle.
            let (settled, approval, nothing_returns) = self.directory().with(|registry| {
                let tasks = ids
                    .iter()
                    .filter_map(|id| registry.task(id).map(|(_, task)| task));
                let mut settled = true;
                let mut approval = false;
                let mut nothing_returns = false;
                for task in tasks {
                    settled &= task.status.is_ended();
                    approval |= task.awaiting_approval;
                    nothing_returns |= !task.status.is_ended() && task.wake == Wake::Never;
                }
                (settled, approval, nothing_returns)
            });
            let inbox_waiting = waiter == Waiter::Agent && self.has_inbox();
            if settled || approval || (nothing_returns && waiter == Waiter::Agent) || inbox_waiting
            {
                dependent = nothing_returns && waiter == Waiter::Agent;
                timed_out = false;
                break;
            }
            if tokio::time::timeout_at(deadline, changed.changed())
                .await
                .is_err()
            {
                dependent = false;
                timed_out = true;
                break;
            }
        }
        let inbox = match waiter {
            Waiter::Agent => self.take_inbox().await,
            Waiter::Program => Vec::new(),
        };
        let now = Instant::now();
        let mut read = Vec::new();
        let tasks = self.directory().with(|registry| {
            let mut snapshots = Vec::with_capacity(ids.len());
            for id in ids {
                let Some((actor, task)) = registry.task(id) else {
                    continue;
                };
                let mut snapshot = actor.snapshot(Some(task), now);
                snapshot.next = if task.status.is_ended() {
                    None
                } else if task.wake == Wake::Never && waiter == Waiter::Agent {
                    Some(text::PEER_RESULT_NEVER_RETURNS.to_owned())
                } else {
                    Some(text::STILL_RUNNING.to_owned())
                };
                snapshots.push(snapshot);
                if let Some((actor, index)) = registry.task_mut(id) {
                    let task = &mut actor.tasks[index];
                    if task.status.is_ended() && task.delivery == Delivery::Owed {
                        task.delivery = Delivery::Delivered;
                        read.push(id.clone());
                    }
                }
            }
            snapshots
        });
        self.journal_tasks(&read, false).await;
        let next = if !inbox.is_empty() {
            Some(text::INBOX_ARRIVED.to_owned())
        } else if dependent {
            Some(text::END_YOUR_TURN.to_owned())
        } else if timed_out {
            Some(text::WAIT_TIMED_OUT.to_owned())
        } else {
            None
        };
        Ok(WaitReport { tasks, inbox, next })
    }

    /// `task_cancel`: stop a task, or whatever the caller asked of an actor.
    pub(super) async fn cancel(&self, raw: &str) -> Result<TaskSnapshot, String> {
        let resolved = self.resolve_for_caller(raw)?;
        let requester = Party::Actor(self.me()?);
        let open: Vec<TaskId> = match &resolved.task {
            Some(id) => vec![id.clone()],
            None => self.directory().with(|registry| {
                registry
                    .actor(&resolved.actor)
                    .map(|actor| {
                        actor
                            .open_tasks()
                            .filter(|task| task.requester == requester)
                            .map(|task| task.id.clone())
                            .collect()
                    })
                    .unwrap_or_default()
            }),
        };
        for id in &open {
            self.cancel_task(id).await;
        }
        let mut snapshot = self
            .directory()
            .with(|registry| snapshot_of(registry, &resolved, &requester, Instant::now()))
            .ok_or_else(|| format!("`{raw}` no longer exists"))?;
        if open.is_empty() && resolved.task.is_none() {
            snapshot.next = Some(text::NOTHING_TO_CANCEL.to_owned());
        }
        Ok(snapshot)
    }

    /// `task_list`: the actors the caller can address and the tasks it asked of them.
    pub(super) async fn list(&self) -> Result<ListReport, String> {
        let me = self.me()?;
        let requester = Party::Actor(me.clone());
        let now = Instant::now();
        let (actors, tasks, mail, mail_open) = self.directory().with(|registry| {
            let tasks: Vec<TaskSnapshot> = registry
                .tasks_requested_by(&requester)
                .map(|(actor, task)| TaskSnapshot {
                    result: None,
                    ..actor.snapshot(Some(task), now)
                })
                .collect();
            let (mail, mail_open) = super::exchange::mail_among_descendants(registry, &me, None);
            (registry.views(&me), tasks, mail, mail_open)
        });
        let inbox = self.take_inbox().await;
        let next = self.poll_instruction(&tasks, &inbox);
        Ok(ListReport {
            actors,
            tasks,
            inbox,
            mail,
            mail_open,
            next,
        })
    }
}

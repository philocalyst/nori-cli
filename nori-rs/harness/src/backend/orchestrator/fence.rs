//! Monotonic lease fences for task lifecycle and wake delivery. Each actor keeps a ledger for the
//! results owed to it.
//!
//! A [`LeaseFence`] pairs a revision (`rev`, bumped on rewind/session-swap) with
//! a monotonic `attempt` (bumped on wake issue, parent cancel, and retry). Wakes
//! are peeked under the active fence and only marked delivered once committed
//! under that exact fence, preventing lost verdicts during concurrent cancel.

use super::task::TaskId;

/// Monotonic fence guarding a task execution or completion wake lease.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
pub(crate) struct LeaseFence {
    pub(crate) rev: i64,
    pub(crate) attempt: i64,
}

impl std::fmt::Display for LeaseFence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "r{}:a{}", self.rev, self.attempt)
    }
}

/// Tracks the monotonic attempt counter and in-flight wake lease for one actor.
#[derive(Debug, Default)]
pub(super) struct FenceLedger {
    attempt: i64,
    active_wake_fence: Option<LeaseFence>,
    leased_tasks: Vec<TaskId>,
}

impl FenceLedger {
    /// The current fence for a given revision.
    pub(super) fn current(&self, rev: i64) -> LeaseFence {
        LeaseFence {
            rev,
            attempt: self.attempt,
        }
    }

    /// Reserve an in-flight wake lease for `task_ids` at `rev`.
    pub(super) fn begin_wake(&mut self, rev: i64, task_ids: Vec<TaskId>) -> LeaseFence {
        self.attempt += 1;
        let fence = LeaseFence {
            rev,
            attempt: self.attempt,
        };
        self.active_wake_fence = Some(fence);
        self.leased_tasks = task_ids;
        fence
    }

    /// Commit an in-flight wake lease if neither `rev` nor `attempt` has moved on.
    pub(super) fn commit_wake(
        &mut self,
        fence: LeaseFence,
        current_rev: i64,
    ) -> Result<Vec<TaskId>, LeaseFence> {
        let current = self.current(current_rev);
        if fence.rev != current_rev || self.active_wake_fence != Some(fence) {
            return Err(current);
        }
        self.active_wake_fence = None;
        Ok(std::mem::take(&mut self.leased_tasks))
    }

    /// Release an in-flight wake lease without marking its tasks delivered.
    pub(super) fn release_wake(&mut self, fence: LeaseFence) -> Vec<TaskId> {
        if self.active_wake_fence == Some(fence) {
            self.active_wake_fence = None;
            return std::mem::take(&mut self.leased_tasks);
        }
        Vec::new()
    }

    /// Invalidate any in-flight wake lease on parent stop/cancel and return its task ids.
    pub(super) fn invalidate_on_stop(&mut self) -> Vec<TaskId> {
        self.attempt += 1;
        self.active_wake_fence = None;
        std::mem::take(&mut self.leased_tasks)
    }

    /// Allocate the next monotonic attempt for a task follow-up or retry.
    pub(super) fn next_task_fence(&mut self, rev: i64) -> LeaseFence {
        self.attempt += 1;
        LeaseFence {
            rev,
            attempt: self.attempt,
        }
    }

    /// Whether a task from `task_fence` belongs to a superseded revision.
    pub(super) fn is_task_fenced_out(task_fence: LeaseFence, current_rev: i64) -> bool {
        task_fence.rev < current_rev
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn ledger_fences_out_stale_attempt_and_revision() {
        let mut ledger = FenceLedger::default();
        let first = ledger.begin_wake(0, vec![TaskId::numbered(1)]);
        assert_eq!(first, LeaseFence { rev: 0, attempt: 1 });

        let invalidated = ledger.invalidate_on_stop();
        assert_eq!(invalidated, vec![TaskId::numbered(1)]);
        assert_eq!(
            ledger.commit_wake(first, 0),
            Err(LeaseFence { rev: 0, attempt: 2 })
        );

        let second = ledger.begin_wake(0, vec![TaskId::numbered(1)]);
        assert_eq!(second, LeaseFence { rev: 0, attempt: 3 });
        // Revision bump (rewind) fences out the lease from rev 0.
        assert_eq!(
            ledger.commit_wake(second, 1),
            Err(LeaseFence { rev: 1, attempt: 3 })
        );
        assert!(FenceLedger::is_task_fenced_out(
            LeaseFence { rev: 0, attempt: 1 },
            1
        ));
        assert!(!FenceLedger::is_task_fenced_out(
            LeaseFence { rev: 1, attempt: 1 },
            1
        ));
    }
}

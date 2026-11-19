//! Noticing an agent that keeps asking whether anything changed.

use std::hash::Hash;
use std::hash::Hasher;
use std::time::Duration;
use std::time::Instant;

use super::Inner;
use super::report::InboxItem;
use super::report::TaskSnapshot;
use super::text;

/// An unchanged answer older than this is not a poll loop.
const MEMORY: Duration = Duration::from_secs(600);
/// This many identical answers in a row get the instruction to stop.
const UNCHANGED_LIMIT: i64 = 2;

/// What a caller's latest status reads returned.
#[derive(Default)]
pub(super) struct PollTracker {
    last: Option<u64>,
    unchanged: i64,
    at: Option<Instant>,
}

impl PollTracker {
    /// Record a read that returned `fingerprint`; how many times in a row it returned that.
    fn observe(&mut self, fingerprint: u64, now: Instant) -> i64 {
        let fresh = self.at.is_some_and(|at| now.duration_since(at) < MEMORY);
        if fresh && self.last == Some(fingerprint) {
            self.unchanged += 1;
        } else {
            self.unchanged = 0;
        }
        self.last = Some(fingerprint);
        self.at = Some(now);
        self.unchanged
    }
}

impl Inner {
    /// A direct instruction to stop, when the caller's last reads all returned the same thing.
    pub(super) fn poll_instruction(
        &self,
        snapshots: &[TaskSnapshot],
        inbox: &[InboxItem],
    ) -> Option<String> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for snapshot in snapshots {
            snapshot.task_id.hash(&mut hasher);
            snapshot.status.hash(&mut hasher);
            snapshot.actor_state.hash(&mut hasher);
            snapshot.delivery.hash(&mut hasher);
            snapshot.activity.hash(&mut hasher);
            snapshot.tool_calls.hash(&mut hasher);
        }
        inbox.len().hash(&mut hasher);
        let unchanged = self
            .polls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .observe(hasher.finish(), Instant::now());
        (unchanged >= UNCHANGED_LIMIT).then(|| text::STOP_POLLING.to_owned())
    }
}

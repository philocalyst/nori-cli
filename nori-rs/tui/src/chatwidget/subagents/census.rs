//! How many subagents there are, by state, counted in one place.
//!
//! The status line, the lanes' header and the rows under a subagent all say how many subagents
//! run or finished. They read the numbers from here, so two of them can never disagree: a
//! subagent is counted once, in the state the tree holds it in, wherever it sits in the
//! hierarchy.

use std::collections::BTreeSet;

use nori_protocol::SubagentOutcome;

use super::SubagentTree;
use super::lanes::Counts;
use super::node::NodeId;

impl SubagentTree {
    /// Every subagent of the current burst of work, by state: those running or idle now, and
    /// those that finished since the burst began. A subagent that finished and is running again
    /// counts as running only.
    pub(super) fn census(&self) -> Counts {
        let mut counts = Counts::default();
        for node in self.live.values() {
            if node.idle {
                counts.idle += 1;
            } else {
                counts.running += 1;
            }
        }
        for (id, record) in &self.ended {
            if self.live.contains_key(id) || record.ended_at < self.burst_since {
                continue;
            }
            match &record.finished.outcome {
                SubagentOutcome::Completed { .. } => counts.done += 1,
                SubagentOutcome::Failed { .. } => counts.failed += 1,
                SubagentOutcome::Cancelled | SubagentOutcome::Closed => counts.cancelled += 1,
            }
        }
        counts
    }

    /// Whether a running subagent above `id` shows it: a subagent is a lane of its own only when
    /// nothing above it is running, because a running parent already says how many run under it.
    pub(super) fn has_running_ancestor(&self, id: NodeId) -> bool {
        let mut seen = BTreeSet::from([id]);
        let mut at = self.live.get(&id).and_then(|node| node.parent);
        while let Some(parent) = at {
            if !seen.insert(parent) {
                return false;
            }
            let Some(node) = self.live.get(&parent) else {
                return false;
            };
            if !node.idle {
                return true;
            }
            at = node.parent;
        }
        false
    }
}

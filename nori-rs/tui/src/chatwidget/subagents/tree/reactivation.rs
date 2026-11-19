//! Restore completed containers when a descendant starts another turn.

use std::collections::BTreeSet;
use std::sync::Arc;

use super::NodeId;
use super::SubagentTree;

impl SubagentTree {
    pub(super) fn restore_ancestor_chain(&mut self, mut child: NodeId, mut parent: Option<NodeId>) {
        let mut seen = BTreeSet::from([child]);
        while let Some(id) = parent {
            if !seen.insert(id) {
                break;
            }
            if let Some(node) = self.live.get_mut(&id) {
                node.children.insert(child);
                node.nested_by_id.remove(&child);
                node.nested = node.nested_by_id.values().cloned().collect();
                node.touch();
                break;
            }
            let Some(record) = self.ended.get(&id) else {
                break;
            };
            let next = record.parent_id;
            self.pending
                .entry(id)
                .or_insert_with(|| record.finished.clone());
            let nested = self.pending_nested.entry(id).or_insert_with(|| {
                record
                    .finished
                    .nested
                    .iter()
                    .map(|summary| (summary.node_id(), Arc::clone(summary)))
                    .collect()
            });
            nested.remove(&child);
            self.pending_children.entry(id).or_default().insert(child);
            self.finished.retain(|finished| finished.node_id != id);
            child = id;
            parent = next;
        }
    }
}

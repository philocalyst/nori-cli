//! Invariants of the subagent tree under arbitrary interleavings, and the work a frame costs.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use nori_protocol::Patch;
use nori_protocol::SubagentMessage;
use nori_protocol::SubagentOutcome;
use nori_protocol::SubagentState;
use nori_protocol::SubagentUpdate;
use nori_protocol::acp::v1 as acp;
use pretty_assertions::assert_eq;
use proptest::prelude::*;

use super::ChildTarget;
use super::SubagentTree;

const SESSIONS: usize = 6;

fn session(index: usize) -> acp::SessionId {
    acp::SessionId::new(format!("c{index}"))
}

fn update(parent: acp::SessionId, child: usize, state: SubagentState) -> SubagentUpdate {
    SubagentUpdate {
        parent_session_id: parent,
        session_id: session(child),
        agent: Patch::Keep,
        model: Patch::Keep,
        name: Patch::Keep,
        title: Patch::Set("general-purpose".into()),
        description: Patch::Set(format!("task {child}")),
        state: Patch::Set(state),
    }
}

fn text(delta: &str) -> acp::SessionUpdate {
    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
        acp::TextContent::new(delta),
    )))
}

fn tool(id: &str) -> acp::SessionUpdate {
    acp::SessionUpdate::ToolCall(
        acp::ToolCall::new(id.to_string(), format!("Read {id}"))
            .kind(acp::ToolKind::Read)
            .raw_input(serde_json::json!({"path": format!("{id}.txt")})),
    )
}

#[derive(Debug, Clone)]
enum Op {
    /// Announce `child` under the root or under another child.
    Announce {
        child: usize,
        parent: Option<usize>,
    },
    Text {
        child: usize,
    },
    Tool {
        child: usize,
        call: usize,
    },
    End {
        child: usize,
        failed: bool,
    },
    /// A launcher tool call on the root, then the message that links it to `child`.
    Launch {
        child: usize,
    },
}

fn op() -> impl Strategy<Value = Op> {
    let child = 0..SESSIONS;
    prop_oneof![
        (child.clone(), proptest::option::of(0..SESSIONS))
            .prop_map(|(child, parent)| Op::Announce { child, parent }),
        child.clone().prop_map(|child| Op::Text { child }),
        (child.clone(), 0..4_usize).prop_map(|(child, call)| Op::Tool { child, call }),
        (child.clone(), any::<bool>()).prop_map(|(child, failed)| Op::End { child, failed }),
        child.prop_map(|child| Op::Launch { child }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Whatever order announcements, output and endings arrive in: every running child is shown
    /// exactly once, subtree completion never duplicates a child, and ending the root loses no nodes.
    #[test]
    fn any_interleaving_keeps_the_tree_consistent(ops in proptest::collection::vec(op(), 1..80)) {
        let base = Instant::now();
        let root = acp::SessionId::new("root");
        let mut tree = SubagentTree::new(PathBuf::from("/work"), false);
        tree.set_root(root.clone());
        let mut ended: BTreeSet<usize> = BTreeSet::new();
        for (step, op) in ops.into_iter().enumerate() {
            let now = base + Duration::from_millis(u64::try_from(step).unwrap_or(0));
            match op {
                Op::Announce { child, parent } => {
                    let parent = parent.filter(|parent| *parent != child).map_or(root.clone(), session);
                    tree.on_update(update(parent, child, SubagentState::Running), now);
                    // Only an explicit Running update can begin another turn on
                    // a completed session. Late output alone cannot revive it.
                    if tree.live.values().any(|node| node.session_id.as_ref() == Some(&session(child))) {
                        ended.remove(&child);
                    }
                }
                Op::Text { child } => {
                    tree.on_child_update(ChildTarget::Session(&session(child)), &text("more\n"), now);
                }
                Op::Tool { child, call } => {
                    tree.on_child_update(
                        ChildTarget::Session(&session(child)),
                        &tool(&format!("t{child}-{call}")),
                        now,
                    );
                }
                Op::End { child, failed } => {
                    let outcome = if failed {
                        SubagentOutcome::Failed { reason: None }
                    } else {
                        SubagentOutcome::Completed { stop_reason: None }
                    };
                    tree.on_update(update(root.clone(), child, SubagentState::Ended(outcome)), now);
                    ended.insert(child);
                }
                Op::Launch { child } => {
                    let call_id = format!("launch-{child}");
                    let call = acp::ToolCall::new(call_id.clone(), "Agent");
                    tree.on_launcher_update(&acp::SessionUpdate::ToolCall(call), true, now);
                    tree.on_message(
                        SubagentMessage {
                            session_id: session(child),
                            message_id: Some(call_id),
                            sender_session_id: Some(root.clone()),
                            recipient_session_id: Some(session(child)),
                            text: "go".into(),
                            chunk: false,
                            sender_label: None,
                            recipient_label: None,
                            delivery: None,
                            failure: None,
                        },
                        now,
                    );
                }
            }
            let mut emitted = Vec::new();
            for finished in tree.take_finished() {
                emitted.push(finished.node_id);
                for nested in finished.nested {
                    nested.collect_ids(&mut emitted);
                }
            }
            prop_assert_eq!(emitted.len(), emitted.iter().copied().collect::<BTreeSet<_>>().len(),
                "a completed subtree emitted the same child twice");
            for id in tree.live.keys() {
                let mut ancestors = BTreeSet::new();
                let mut current = Some(*id);
                while let Some(ancestor) = current {
                    prop_assert!(ancestors.insert(ancestor), "cyclic subagent ancestry");
                    current = tree.live.get(&ancestor).and_then(|node| node.parent)
                        .or_else(|| tree.ended.get(&ancestor).and_then(|record| record.parent_id));
                }
            }
            let shown = tree.display_order(usize::MAX);
            prop_assert_eq!(shown.len(), tree.running_count());
            let mut seen = BTreeSet::new();
            for (_id, node, _depth) in &shown {
                prop_assert!(seen.insert(node as *const _ as usize), "a node is shown twice");
                if let Some(id) = &node.session_id {
                    let index: usize = id.0.trim_start_matches('c').parse().unwrap_or(usize::MAX);
                    prop_assert!(!ended.contains(&index), "an ended child is running again");
                }
            }
        }
        // Cancelling the root must conserve every retained live or pending
        // subtree, regardless of which parent finished or reactivated first.
        let mut expected: Vec<_> = tree.live.keys().chain(tree.pending.keys()).copied().collect();
        for node in tree.live.values() {
            for nested in &node.nested {
                nested.collect_ids(&mut expected);
            }
        }
        for pending in tree.pending.values() {
            for nested in &pending.nested {
                nested.collect_ids(&mut expected);
            }
        }
        for nested in tree.pending_nested.values().flat_map(|children| children.values()) {
            nested.collect_ids(&mut expected);
        }
        let expected: BTreeSet<_> = expected.into_iter().collect();
        tree.on_root_turn_end(true, base + Duration::from_secs(60));
        prop_assert_eq!(tree.running_count(), 0);
        let mut emitted = Vec::new();
        for finished in tree.take_finished() {
            emitted.push(finished.node_id);
            for nested in finished.nested {
                nested.collect_ids(&mut emitted);
            }
        }
        let actual: BTreeSet<_> = emitted.iter().copied().collect();
        prop_assert_eq!(emitted.len(), actual.len(), "a child was emitted twice when the root ended");
        prop_assert_eq!(actual, expected, "a retained subtree was lost when the root ended");
        prop_assert!(tree.pending.is_empty(), "root cancellation left a pending ancestor");
        prop_assert!(tree.take_finished().is_empty());
    }
}

#[test]
fn a_finished_grandchild_is_nested_in_its_parent_and_reported_once() {
    let now = Instant::now();
    let root = acp::SessionId::new("root");
    let mut tree = SubagentTree::new(PathBuf::from("/work"), false);
    tree.set_root(root.clone());
    tree.on_update(update(root.clone(), 1, SubagentState::Running), now);
    tree.on_update(update(session(1), 2, SubagentState::Running), now);
    assert_eq!(
        tree.display_order(usize::MAX)
            .iter()
            .map(|(_id, node, depth)| (node.label(), *depth))
            .collect::<Vec<_>>(),
        vec![("task 1".to_owned(), 0), ("task 2".to_owned(), 1)]
    );
    let done = SubagentState::Ended(SubagentOutcome::Completed { stop_reason: None });
    tree.on_update(update(session(1), 2, done.clone()), now);
    let finished = tree.take_finished();
    assert!(finished.is_empty());
    // A late update for the finished grandchild changes nothing.
    tree.on_child_update(ChildTarget::Session(&session(2)), &text("late"), now);
    tree.on_update(update(session(1), 2, SubagentState::Running), now);
    assert_eq!(tree.running_count(), 2);
    tree.on_update(update(root, 1, done), now);
    assert!(tree.take_finished().is_empty());
    tree.on_update(
        update(
            session(1),
            2,
            SubagentState::Ended(SubagentOutcome::Completed { stop_reason: None }),
        ),
        now,
    );
    let parent = tree.take_finished();
    assert_eq!(parent.len(), 1);
    assert_eq!(
        parent[0].detail.len(),
        1,
        "the grandchild's summary is in its parent's detail"
    );
}

#[test]
fn a_parent_summary_waits_for_a_grandchild_that_finishes_later() {
    let now = Instant::now();
    let root = acp::SessionId::new("root");
    let mut tree = SubagentTree::new(PathBuf::from("/work"), false);
    tree.set_root(root.clone());
    tree.on_update(update(root.clone(), 1, SubagentState::Running), now);
    tree.on_update(update(session(1), 2, SubagentState::Running), now);
    tree.on_update(update(session(2), 3, SubagentState::Running), now);
    let done = SubagentState::Ended(SubagentOutcome::Completed { stop_reason: None });

    tree.on_update(update(root, 1, done.clone()), now);
    assert!(tree.take_finished().is_empty());
    tree.on_update(update(session(1), 2, done.clone()), now);
    assert!(tree.take_finished().is_empty());
    tree.on_update(update(session(2), 3, done), now);

    let finished = tree.take_finished();
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].label, "task 1");
    assert_eq!(finished[0].nested.len(), 1);
    assert_eq!(finished[0].detail.len(), 1);
}

#[test]
fn reactivated_parent_keeps_a_live_child_visible() {
    let now = Instant::now();
    let root = acp::SessionId::new("root");
    let mut tree = SubagentTree::new(PathBuf::from("/work"), false);
    tree.set_root(root.clone());
    tree.on_child_update(ChildTarget::Session(&session(5)), &text("first"), now);
    tree.on_update(update(session(5), 0, SubagentState::Running), now);
    tree.on_update(
        update(
            session(5),
            5,
            SubagentState::Ended(SubagentOutcome::Completed { stop_reason: None }),
        ),
        now,
    );
    tree.on_update(update(root, 5, SubagentState::Running), now);
    let shown = tree.display_order(usize::MAX);
    assert_eq!(shown.len(), 2);
    assert!(
        shown
            .iter()
            .any(|(_, node, _)| node.session_id == Some(session(0)))
    );
}

#[test]
fn a_completed_native_child_reactivates_with_the_same_key() {
    let now = Instant::now();
    let root = acp::SessionId::new("root");
    let child = session(1);
    let mut tree = SubagentTree::new(PathBuf::from("/work"), false);
    tree.set_root(root.clone());
    tree.on_update(update(root.clone(), 1, SubagentState::Running), now);
    tree.on_child_update(ChildTarget::Session(&child), &text("first turn"), now);
    tree.on_update(
        update(
            root.clone(),
            1,
            SubagentState::Ended(SubagentOutcome::Completed { stop_reason: None }),
        ),
        now,
    );
    let first = tree.take_finished();
    assert_eq!(first.len(), 1);
    let key = first[0].label.clone();

    tree.on_update(update(root, 1, SubagentState::Running), now);
    assert_eq!(tree.running_count(), 1);
    tree.on_child_update(ChildTarget::Session(&child), &text("second turn"), now);
    tree.on_update(
        update(
            acp::SessionId::new("root"),
            1,
            SubagentState::Ended(SubagentOutcome::Completed { stop_reason: None }),
        ),
        now,
    );
    let second = tree.take_finished();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].label, key);
    assert!(second[0].detail.len() >= 2);
}

#[test]
fn the_final_message_is_what_the_child_said_after_its_last_tool() {
    let now = Instant::now();
    let root = acp::SessionId::new("root");
    let mut tree = SubagentTree::new(PathBuf::from("/work"), false);
    tree.set_root(root.clone());
    tree.on_update(update(root.clone(), 1, SubagentState::Running), now);
    let child = session(1);
    tree.on_child_update(ChildTarget::Session(&child), &text("Let me look."), now);
    tree.on_child_update(ChildTarget::Session(&child), &tool("t1"), now);
    tree.on_child_update(ChildTarget::Session(&child), &text("The file holds "), now);
    tree.on_child_update(ChildTarget::Session(&child), &text("alpha."), now);
    tree.on_update(
        update(
            root,
            1,
            SubagentState::Ended(SubagentOutcome::Completed { stop_reason: None }),
        ),
        now,
    );
    let finished = tree.take_finished();
    assert_eq!(finished[0].message, "The file holds alpha.");
    assert_eq!(finished[0].tool_count, 1);
    assert_eq!(finished[0].detail.len(), 3);
}

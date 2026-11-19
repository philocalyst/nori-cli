//! Completion, reactivation, and pruning behavior for nested subagents.

use std::path::PathBuf;
use std::time::Instant;

use nori_protocol::Patch;
use nori_protocol::SubagentOutcome;
use nori_protocol::SubagentState;
use nori_protocol::SubagentUpdate;
use nori_protocol::acp::v1 as acp;
use pretty_assertions::assert_eq;
use ratatui::text::Line;

use super::ChildTarget;
use super::SubagentSummaryCell;
use super::SubagentTree;
use super::node::NodeId;
use crate::history_cell::HistoryCell;
use crate::subagent_switcher::Status;
use crate::subagent_switcher::SubagentKey;
use crate::subagent_switcher::Target;

const ROOT: &str = "root";

fn session(index: i64) -> acp::SessionId {
    acp::SessionId::new(format!("c{index}"))
}

fn update(
    parent: &acp::SessionId,
    child: i64,
    title: &str,
    description: &str,
    state: SubagentState,
) -> SubagentUpdate {
    SubagentUpdate {
        parent_session_id: parent.clone(),
        session_id: session(child),
        agent: Patch::Keep,
        model: Patch::Keep,
        title: Patch::Set(title.to_owned()),
        description: Patch::Set(description.to_owned()),
        state: Patch::Set(state),
    }
}

fn announce(tree: &mut SubagentTree, parent: &acp::SessionId, child: i64, label: &str) {
    tree.on_update(
        update(parent, child, "worker-kind", label, SubagentState::Running),
        Instant::now(),
    );
}

fn finish(tree: &mut SubagentTree, parent: &acp::SessionId, child: i64) {
    tree.on_update(
        SubagentUpdate {
            parent_session_id: parent.clone(),
            session_id: session(child),
            agent: Patch::Keep,
            model: Patch::Keep,
            title: Patch::Keep,
            description: Patch::Keep,
            state: Patch::Set(SubagentState::Ended(SubagentOutcome::Completed {
                stop_reason: None,
            })),
        },
        Instant::now(),
    );
}

fn text(message: &str) -> acp::SessionUpdate {
    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
        acp::TextContent::new(message),
    )))
}

fn plain(lines: &[Line<'static>]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect()
}

fn make_tree() -> (SubagentTree, acp::SessionId) {
    let root = acp::SessionId::new(ROOT);
    let mut tree = SubagentTree::new(PathBuf::from("/work"), false);
    tree.set_root(root.clone());
    (tree, root)
}

#[test]
fn pending_sibling_summaries_keep_creation_order_after_reverse_completion() {
    let (mut tree, root) = make_tree();
    announce(&mut tree, &root, 1, "parent");
    announce(&mut tree, &session(1), 2, "first child");
    announce(&mut tree, &session(1), 3, "second child");

    finish(&mut tree, &root, 1);
    assert!(tree.take_finished().is_empty());
    finish(&mut tree, &session(1), 3);
    assert!(tree.take_finished().is_empty());
    finish(&mut tree, &session(1), 2);

    let finished = tree.take_finished();
    assert_eq!(finished.len(), 1);
    let lines = plain(&SubagentSummaryCell::new(finished[0].clone()).transcript_lines(100));
    let first = lines
        .iter()
        .position(|line| line.contains("first child"))
        .expect("first child summary");
    let second = lines
        .iter()
        .position(|line| line.contains("second child"))
        .expect("second child summary");
    assert!(first < second, "{lines:?}");
}

#[test]
fn pending_nested_parent_survives_ended_record_pruning() {
    let (mut tree, root) = make_tree();
    announce(&mut tree, &root, 1, "root parent");
    announce(&mut tree, &session(1), 2, "nested parent");
    announce(&mut tree, &session(2), 3, "grandchild");
    finish(&mut tree, &session(1), 2);
    assert!(tree.take_finished().is_empty());

    for child in 4..=260 {
        announce(&mut tree, &root, child, &format!("unrelated {child}"));
        finish(&mut tree, &root, child);
        let _ = tree.take_finished();
    }

    let (entries, _) = tree.entries(Instant::now());
    assert!(entries.iter().any(|entry| {
        entry.target == Target::Subagent(SubagentKey(2)) && entry.status == Status::Completed
    }));

    finish(&mut tree, &session(2), 3);
    let _ = tree.take_finished();
    finish(&mut tree, &root, 1);
    let finished = tree.take_finished();
    assert_eq!(finished.len(), 1);
    let lines = plain(&SubagentSummaryCell::new(finished[0].clone()).transcript_lines(100));
    assert!(
        lines.iter().any(|line| line.contains("nested parent")),
        "{lines:?}"
    );
    assert!(
        lines.iter().any(|line| line.contains("grandchild")),
        "{lines:?}"
    );
}

#[test]
fn a_running_descendant_stays_visible_after_its_parent_finishes() {
    let (mut tree, root) = make_tree();
    announce(&mut tree, &root, 1, "parent");
    announce(&mut tree, &session(1), 2, "running child");
    finish(&mut tree, &root, 1);

    let running: Vec<i64> = tree.live.keys().map(|id| id.0).collect();
    assert_eq!(running, vec![2]);
    assert_eq!(tree.depth_of(NodeId(2)), 1);
    assert!(tree.take_finished().is_empty());
}

#[test]
fn reactivated_grandchild_rebuilds_completed_parent_under_running_grandparent() {
    let (mut tree, root) = make_tree();
    announce(&mut tree, &root, 1, "grandparent");
    announce(&mut tree, &session(1), 2, "parent");
    announce(&mut tree, &session(2), 3, "child");
    tree.on_child_update(
        ChildTarget::Session(&session(3)),
        &text("first turn"),
        Instant::now(),
    );
    finish(&mut tree, &session(2), 3);
    finish(&mut tree, &session(1), 2);
    assert!(tree.take_finished().is_empty());
    announce(&mut tree, &session(2), 3, "child");
    tree.on_child_update(
        ChildTarget::Session(&session(3)),
        &text("second turn"),
        Instant::now(),
    );
    finish(&mut tree, &session(2), 3);
    assert!(tree.take_finished().is_empty());
    finish(&mut tree, &root, 1);
    let finished = tree.take_finished();
    assert_eq!(finished.len(), 1);
    let mut ids = vec![finished[0].node_id];
    for child in &finished[0].nested {
        child.collect_ids(&mut ids);
    }
    assert_eq!(ids.len(), 3);
    assert_eq!(
        ids.iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3
    );
    let parent = &finished[0].nested[0];
    let lines = plain(&parent.display_lines(100));
    assert!(lines.iter().any(|line| line.contains("child")), "{lines:?}");
    let record = tree
        .ended
        .values()
        .find(|record| record.session_id.as_ref() == Some(&session(2)))
        .unwrap();
    let detail: Vec<_> = record
        .finished
        .detail
        .iter()
        .flat_map(|cell| cell.transcript_lines(100))
        .collect();
    let detail = plain(&detail);
    assert!(
        detail.iter().any(|line| line.contains("first turn")),
        "{detail:?}"
    );
    assert!(
        detail.iter().any(|line| line.contains("second turn")),
        "{detail:?}"
    );
}

#[test]
fn resumed_nested_child_keeps_parent_kind_and_archived_body_until_parent_finishes() {
    let (mut tree, root) = make_tree();
    announce(&mut tree, &root, 1, "parent");
    announce(&mut tree, &session(1), 2, "nested child");
    tree.on_child_update(
        ChildTarget::Session(&session(2)),
        &text("first turn"),
        Instant::now(),
    );
    finish(&mut tree, &session(1), 2);
    let _ = tree.take_finished();

    tree.on_update(
        SubagentUpdate {
            parent_session_id: session(1),
            session_id: session(2),
            agent: Patch::Keep,
            model: Patch::Keep,
            title: Patch::Keep,
            description: Patch::Keep,
            state: Patch::Set(SubagentState::Running),
        },
        Instant::now(),
    );

    let (entries, _) = tree.entries(Instant::now());
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.target == Target::Subagent(SubagentKey(2)))
            .count(),
        1
    );
    let child = entries
        .iter()
        .find(|entry| entry.target == Target::Subagent(SubagentKey(2)))
        .expect("resumed child");
    assert_eq!(child.status, Status::Running);
    assert_eq!(
        child.parent.as_ref().map(|parent| parent.label.as_str()),
        Some("parent")
    );
    assert_eq!(child.agent.as_deref(), Some("worker-kind"));
    tree.on_child_update(
        ChildTarget::Session(&session(2)),
        &text("second turn"),
        Instant::now(),
    );
    let (_, body) = tree.body(SubagentKey(2), 100, 20).expect("live child body");
    let body = body
        .iter()
        .flat_map(|line| &line.spans)
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(body.contains("first turn"), "{body}");
    assert!(body.contains("second turn"), "{body}");

    finish(&mut tree, &root, 1);
    assert!(tree.take_finished().is_empty());
    finish(&mut tree, &session(1), 2);
    let finished = tree.take_finished();
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].label, "parent");
    assert!(finished[0].nested.iter().any(|nested| {
        plain(&nested.display_lines(100))
            .iter()
            .any(|line| line.contains("nested child"))
    }));
}

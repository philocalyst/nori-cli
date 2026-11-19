//! What the subagent switcher is told about the tree.

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

use super::ChildTarget;
use super::SubagentTree;
use crate::subagent_switcher::Entry;
use crate::subagent_switcher::Status;
use crate::subagent_switcher::SubagentKey;
use crate::subagent_switcher::Target;

fn session(index: usize) -> acp::SessionId {
    acp::SessionId::new(format!("c{index}"))
}

fn announce(tree: &mut SubagentTree, parent: &acp::SessionId, index: usize, now: Instant) {
    tree.on_update(
        SubagentUpdate {
            parent_session_id: parent.clone(),
            session_id: session(index),
            agent: Patch::Keep,
            model: Patch::Keep,
            title: Patch::Set("general-purpose".into()),
            description: Patch::Set(format!("task {index}")),
            state: Patch::Set(SubagentState::Running),
        },
        now,
    );
}

fn end(tree: &mut SubagentTree, index: usize, outcome: SubagentOutcome, now: Instant) {
    tree.on_update(
        SubagentUpdate {
            parent_session_id: acp::SessionId::new("root"),
            session_id: session(index),
            agent: Patch::Keep,
            model: Patch::Keep,
            title: Patch::Keep,
            description: Patch::Keep,
            state: Patch::Set(SubagentState::Ended(outcome)),
        },
        now,
    );
}

fn say(tree: &mut SubagentTree, index: usize, delta: &str, now: Instant) {
    let chunk = acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(delta)));
    tree.on_child_update(
        ChildTarget::Session(&session(index)),
        &acp::SessionUpdate::AgentMessageChunk(chunk),
        now,
    );
}

fn tool(tree: &mut SubagentTree, index: usize, id: &str, now: Instant) {
    let call = acp::ToolCall::new(id.to_string(), format!("Read {id}"))
        .kind(acp::ToolKind::Read)
        .raw_input(serde_json::json!({ "path": format!("{id}.txt") }));
    tree.on_child_update(
        ChildTarget::Session(&session(index)),
        &acp::SessionUpdate::ToolCall(call),
        now,
    );
}

fn new_tree() -> (SubagentTree, acp::SessionId) {
    let root = acp::SessionId::new("root");
    let mut tree = SubagentTree::new(PathBuf::from("/work"), false);
    tree.set_root(root.clone());
    (tree, root)
}

fn find(entries: &[Entry], index: i64) -> &Entry {
    entries
        .iter()
        .find(|entry| entry.target == Target::Subagent(SubagentKey(index)))
        .expect("entry")
}

#[test]
fn finished_children_stay_in_the_roster_with_their_outcome() {
    let base = Instant::now();
    let (mut tree, root) = new_tree();
    announce(&mut tree, &root, 1, base);
    announce(&mut tree, &root, 2, base);
    tool(&mut tree, 1, "a", base);
    end(
        &mut tree,
        1,
        SubagentOutcome::Failed {
            reason: Some("usage limit".into()),
        },
        base + Duration::from_secs(30),
    );
    let _ = tree.take_finished();

    let (entries, replaced) = tree.entries(base + Duration::from_secs(90));
    assert_eq!(replaced, Vec::new());
    assert_eq!(entries.len(), 2);
    let failed = find(&entries, 1);
    assert_eq!(failed.status, Status::Failed);
    assert_eq!(failed.detail.as_deref(), Some("usage limit"));
    assert_eq!(failed.title, "task 1");
    assert_eq!(failed.agent.as_deref(), Some("general-purpose"));
    assert_eq!(failed.elapsed, Duration::from_secs(30));
    assert_eq!(failed.ended_ago, Some(Duration::from_secs(60)));
    assert_eq!(failed.tool_count, 1);
    let running = find(&entries, 2);
    assert_eq!(running.status, Status::Running);
    assert_eq!(running.elapsed, Duration::from_secs(90));
    assert_eq!(running.ended_ago, None);
}

#[test]
fn a_nested_child_names_its_parent_even_after_the_parent_is_gone() {
    let base = Instant::now();
    let (mut tree, root) = new_tree();
    announce(&mut tree, &root, 1, base);
    announce(&mut tree, &session(1), 2, base);
    let (entries, _) = tree.entries(base);
    assert_eq!(
        find(&entries, 2)
            .parent
            .as_ref()
            .map(|parent| parent.label.as_str()),
        Some("task 1")
    );
    assert_eq!(find(&entries, 1).parent, None);

    end(
        &mut tree,
        2,
        SubagentOutcome::Completed { stop_reason: None },
        base,
    );
    let (entries, _) = tree.entries(base);
    assert_eq!(
        find(&entries, 2)
            .parent
            .as_ref()
            .map(|parent| parent.label.as_str()),
        Some("task 1"),
        "a finished child keeps the name of the parent it ran under"
    );
}

#[test]
fn a_launcher_call_is_replaced_by_the_session_it_started() {
    let base = Instant::now();
    let (mut tree, root) = new_tree();
    let call = acp::ToolCall::new("launch".to_string(), "Agent")
        .kind(acp::ToolKind::Think)
        .raw_input(serde_json::json!({ "description": "Look around", "subagent_type": "Explore" }));
    tree.on_launcher_update(&acp::SessionUpdate::ToolCall(call), true, base);
    let (before, _) = tree.entries(base);
    assert_eq!(before.len(), 1);
    let launcher = before[0].target;

    tree.on_message(
        SubagentMessage {
            session_id: session(1),
            message_id: Some("launch".into()),
            sender_session_id: Some(root),
            recipient_session_id: Some(session(1)),
            text: "go".into(),
            chunk: false,
            sender_label: None,
            recipient_label: None,
            delivery: None,
            failure: None,
        },
        base,
    );
    let (after, replaced) = tree.entries(base);
    assert_eq!(after.len(), 1);
    let Target::Subagent(old) = launcher else {
        panic!("a subagent");
    };
    assert_eq!(replaced.len(), 1);
    assert_eq!(replaced[0].0, old);
    assert_eq!(Target::Subagent(replaced[0].1), after[0].target);
}

#[test]
fn a_body_costs_the_rows_asked_for_not_the_length_of_the_conversation() {
    let base = Instant::now();
    let (mut tree, root) = new_tree();
    announce(&mut tree, &root, 1, base);
    for call in 0..400 {
        say(&mut tree, 1, &format!("thinking about step {call}\n"), base);
        tool(&mut tree, 1, &format!("call{call}"), base);
    }
    say(&mut tree, 1, "All done: the answer is 42.", base);
    let (revision, lines) = tree.body(SubagentKey(1), 80, 12).expect("live body");
    assert_eq!(lines.len(), 12);
    let text: String = lines
        .last()
        .expect("a last row")
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(text.ends_with("All done: the answer is 42."), "{text}");
    let (entries, _) = tree.entries(base);
    assert_eq!(revision, find(&entries, 1).revision);

    // The same tail is what the finished child shows.
    end(
        &mut tree,
        1,
        SubagentOutcome::Completed { stop_reason: None },
        base,
    );
    let (ended_revision, ended_lines) = tree.body(SubagentKey(1), 80, 12).expect("ended body");
    let (ended_entries, _) = tree.entries(base);
    assert_eq!(ended_revision, find(&ended_entries, 1).revision);
    assert_eq!(ended_lines.len(), 12);
    assert!(tree.body(SubagentKey(77), 80, 12).is_none());
}

#[test]
fn only_the_most_recent_finished_children_are_kept() {
    let base = Instant::now();
    let (mut tree, root) = new_tree();
    for index in 0..300 {
        announce(&mut tree, &root, index, base);
        end(&mut tree, index, SubagentOutcome::Cancelled, base);
        let _ = tree.take_finished();
    }
    let (entries, _) = tree.entries(base);
    assert_eq!(entries.len(), 256);
    assert!(
        entries
            .iter()
            .all(|entry| entry.status == Status::Cancelled)
    );
}

#[test]
fn idle_child_resumes_with_the_same_history_and_identity() {
    let base = Instant::now();
    let (mut tree, root) = new_tree();
    announce(&mut tree, &root, 1, base);
    say(&mut tree, 1, "first turn", base);
    let state = |state| SubagentUpdate {
        parent_session_id: root.clone(),
        session_id: session(1),
        agent: Patch::Keep,
        model: Patch::Keep,
        title: Patch::Keep,
        description: Patch::Keep,
        state: Patch::Set(state),
    };
    tree.on_update(state(SubagentState::Idle), base);
    assert_eq!(tree.running_count(), 0);
    assert_eq!(find(&tree.entries(base).0, 1).status, Status::Idle);
    tree.on_update(state(SubagentState::Running), base);
    say(&mut tree, 1, "second turn", base);
    assert_eq!(tree.running_count(), 1);
    assert_eq!(find(&tree.entries(base).0, 1).status, Status::Running);
    let (_, lines) = tree.body(SubagentKey(1), 80, 20).expect("child history");
    let text = lines
        .iter()
        .flat_map(|line| &line.spans)
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(text.contains("first turn"));
    assert!(text.contains("second turn"));
}

#[test]
fn announced_agent_identity_survives_child_completion() {
    let now = Instant::now();
    let (mut tree, root) = new_tree();
    tree.on_update(
        SubagentUpdate {
            parent_session_id: root.clone(),
            session_id: session(1),
            agent: Patch::Set("claude-code".to_owned()),
            model: Patch::Set("anthropic/claude-sonnet-4".to_owned()),
            title: Patch::Set("Research".to_owned()),
            description: Patch::Keep,
            state: Patch::Set(SubagentState::Running),
        },
        now,
    );
    assert_eq!(tree.agent_for_key(SubagentKey(1)), Some("claude-code"));
    assert_eq!(
        find(&tree.entries(now).0, 1).agent.as_deref(),
        Some("claude-code")
    );
    assert_eq!(
        find(&tree.entries(now).0, 1).model.as_deref(),
        Some("anthropic/claude-sonnet-4")
    );
    end(
        &mut tree,
        1,
        SubagentOutcome::Completed { stop_reason: None },
        now,
    );
    assert_eq!(tree.agent_for_key(SubagentKey(1)), Some("claude-code"));
    assert_eq!(
        find(&tree.entries(now).0, 1).agent.as_deref(),
        Some("claude-code")
    );
    assert_eq!(
        find(&tree.entries(now).0, 1).model.as_deref(),
        Some("anthropic/claude-sonnet-4")
    );
    tree.on_update(
        SubagentUpdate {
            parent_session_id: root,
            session_id: session(1),
            agent: Patch::Keep,
            model: Patch::Set("deepseek/deepseek-chat".to_owned()),
            title: Patch::Keep,
            description: Patch::Keep,
            state: Patch::Keep,
        },
        now,
    );
    assert_eq!(
        find(&tree.entries(now).0, 1).model.as_deref(),
        Some("deepseek/deepseek-chat")
    );
}

#[test]
fn parent_hierarchy_survives_parent_and_child_completion() {
    let now = Instant::now();
    let (mut tree, root) = new_tree();
    announce(&mut tree, &root, 1, now);
    announce(&mut tree, &session(1), 2, now);
    assert_eq!(tree.parent_for_key(SubagentKey(2)), Some(SubagentKey(1)));
    let done = SubagentOutcome::Completed { stop_reason: None };
    end(&mut tree, 1, done.clone(), now);
    assert_eq!(tree.parent_for_key(SubagentKey(2)), Some(SubagentKey(1)));
    end(&mut tree, 2, done, now);
    assert_eq!(tree.parent_for_key(SubagentKey(2)), Some(SubagentKey(1)));
    assert_eq!(tree.parent_for_key(SubagentKey(1)), None);
}

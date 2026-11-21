//! Subagent presentation, driven by wire traffic recorded from a real adapter.

use super::*;
use crate::render::renderable::Renderable;
use nori_protocol::AcpEvent;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::acp::v1 as acp;
use pretty_assertions::assert_eq;
use std::time::Instant;

fn fixture(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../mock-acp-agent/fixtures")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// The harness events a recorded `session/update` stream turns into, plus the root session id.
fn replay_events(name: &str) -> (String, Vec<SessionEvent>) {
    let mut root: Option<String> = None;
    let mut events = Vec::new();
    for line in fixture(name).lines() {
        let record: serde_json::Value = serde_json::from_str(line).expect("jsonl line");
        let message = &record["msg"];
        if record["dir"] != "in" || message["method"] != "session/update" {
            continue;
        }
        let params = &message["params"];
        let session = params["sessionId"].as_str().expect("session id").to_owned();
        let root_id = root.get_or_insert_with(|| session.clone()).clone();
        if let Some(wire) = nori_protocol::parse_subagent_update(params) {
            events.push(SessionEvent::Nori(NoriEvent::Subagent(wire)));
            continue;
        }
        let notification: acp::SessionNotification =
            serde_json::from_value(params.clone()).expect("typed notification");
        events.push(SessionEvent::Acp(if session == root_id {
            AcpEvent::Notification(acp::AgentNotification::SessionNotification(notification))
        } else {
            AcpEvent::ChildNotification(notification)
        }));
    }
    (root.expect("root session"), events)
}

fn plain(lines: &[ratatui::text::Line<'static>]) -> Vec<String> {
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

fn history_text(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>) -> String {
    drain_insert_history(rx)
        .into_iter()
        .map(|lines| lines_to_single_string(&lines))
        .collect()
}

fn block_lines(chat: &ChatWidget, width: u16) -> Vec<String> {
    let hits = std::cell::RefCell::new(Vec::new());
    let block = crate::chatwidget::subagents::InChatSubagents {
        tree: &chat.subagents,
        focused: None,
        now: Instant::now(),
        screen_rows: 0,
        hits: &hits,
    };
    let height = block.desired_height(width);
    let area = ratatui::layout::Rect::new(0, 0, width, height);
    let mut buffer = ratatui::buffer::Buffer::empty(area);
    block.render(area, &mut buffer);
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

#[test]
fn announced_children_get_a_live_block_and_a_summary_each() {
    let (root, events) = replay_events("claude_subagents.jsonl");
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual();
    let generation = chat.session_generation;
    chat.subagents.set_root(acp::SessionId::new(root));

    // Up to the point where both children are announced and working.
    let both_running = events
        .iter()
        .position(|event| matches!(event, SessionEvent::Acp(AcpEvent::ChildNotification(_))))
        .expect("a child update");
    for event in events[..both_running].iter().cloned() {
        chat.handle_session_event(generation, event);
    }
    let live = block_lines(&chat, 80);
    assert!(
        live.iter().any(|line| line.contains("Read a.txt contents")),
        "{live:#?}"
    );
    // The launcher's own tool call must not be painted as a root tool cell.
    assert!(chat.active_cell.as_ref().is_none(), "{live:#?}");

    for event in events[both_running..].iter().cloned() {
        chat.handle_session_event(generation, event);
    }
    chat.commit_stream_for_frame();
    chat.flush_answer_stream_with_separator();
    chat.flush_active_cell();

    assert_eq!(chat.subagents.running_count(), 0);
    let history = history_text(&mut rx);
    // One summary per child, in completion order (a.txt finished first).
    let first = history
        .find("Read a.txt contents")
        .expect("summary for a.txt");
    let second = history
        .find("Count lines in b.txt")
        .expect("summary for b.txt");
    assert!(first < second, "{history}");
    // The children's own words are in their summaries, never in the root answer's cells.
    assert!(
        history.contains("The file exists and I read it"),
        "{history}"
    );
    // The root's text is all there, once.
    assert_eq!(
        history.matches("Both subagents are running").count(),
        1,
        "{history}"
    );
    assert_eq!(history.matches("a.txt contains").count(), 1, "{history}");
}

#[test]
fn a_launcher_call_that_reports_completion_does_not_finish_the_child() {
    let (root, events) = replay_events("claude_subagents.jsonl");
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual();
    let generation = chat.session_generation;
    chat.subagents.set_root(acp::SessionId::new(root));
    // Everything up to the first child's tool call: both Agent calls already reported
    // "completed", the children are still working.
    let stop = events
        .iter()
        .position(|event| matches!(event, SessionEvent::Acp(AcpEvent::ChildNotification(_))))
        .expect("a child update");
    for event in events[..stop].iter().cloned() {
        chat.handle_session_event(generation, event);
    }
    assert!(chat.subagents.running_count() >= 1);
}

#[test]
fn without_the_capability_tagged_tool_calls_group_under_their_launcher() {
    let (root, events) = replay_events("claude_plain.jsonl");
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual();
    let generation = chat.session_generation;
    chat.subagents.set_root(acp::SessionId::new(root));

    // The children's tool calls arrive on the root session; stop once both launched and one child
    // tool call was seen.
    let mut saw_child_tool = false;
    for event in events.iter().cloned() {
        let is_tool = matches!(
            &event,
            SessionEvent::Acp(AcpEvent::Notification(
                acp::AgentNotification::SessionNotification(n)
            )) if matches!(n.update, acp::SessionUpdate::ToolCall(_))
                && nori_protocol::ClaudeToolMeta::read(match &n.update {
                    acp::SessionUpdate::ToolCall(call) => call.meta.as_ref(),
                    _ => None,
                })
                .parent_tool_use_id
                .is_some()
        );
        chat.handle_session_event(generation, event);
        if is_tool && !saw_child_tool {
            saw_child_tool = true;
            let live = block_lines(&chat, 80);
            assert!(
                live.iter().any(|line| line.contains("Read a.txt contents")),
                "{live:#?}"
            );
            assert!(
                live.iter().any(|line| line.contains("Read a.txt")),
                "{live:#?}"
            );
            // Nothing of the child's work is in the root's conversation.
            assert!(chat.active_cell.as_ref().is_none());
        }
    }
    assert!(saw_child_tool);

    // The root turn ending ends the children that only a launcher call stood for.
    chat.subagents_turn_ended(false);
    chat.flush_answer_stream_with_separator();
    chat.flush_active_cell();
    assert_eq!(chat.subagents.running_count(), 0);
    let history = history_text(&mut rx);
    assert!(history.contains("Read a.txt contents"), "{history}");
    assert!(history.contains("Count lines in b.txt"), "{history}");
    assert!(
        history.contains("2 tools") || history.contains("1 tool"),
        "{history}"
    );
}

#[test]
fn cancelling_the_turn_shows_running_children_cancelled() {
    let (root, events) = replay_events("claude_subagents.jsonl");
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual();
    let generation = chat.session_generation;
    chat.subagents.set_root(acp::SessionId::new(root));
    let stop = events
        .iter()
        .position(|event| matches!(event, SessionEvent::Acp(AcpEvent::ChildNotification(_))))
        .expect("a child update");
    for event in events[..stop].iter().cloned() {
        chat.handle_session_event(generation, event);
    }
    chat.subagents_turn_ended(true);
    chat.flush_active_cell();

    assert_eq!(chat.subagents.running_count(), 0);
    let history = history_text(&mut rx);
    assert!(history.contains("cancelled"), "{history}");
}

#[test]
fn a_childs_permission_request_names_the_child() {
    let (root, events) = replay_events("claude_subagents.jsonl");
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual();
    let generation = chat.session_generation;
    chat.subagents.set_root(acp::SessionId::new(root));
    for event in events[..events.len().min(15)].iter().cloned() {
        chat.handle_session_event(generation, event);
    }
    let child = chat
        .subagents
        .label_for_session(&acp::SessionId::new("aecd710fff4bf3369"))
        .expect("the first child is announced");
    assert_eq!(child, "Read a.txt contents");
    assert_eq!(
        chat.subagent_label_for(&acp::SessionId::new("aecd710fff4bf3369"))
            .as_deref(),
        Some("Read a.txt contents")
    );
    // The root is never labelled.
    assert_eq!(
        chat.subagent_label_for(&acp::SessionId::new("04953aff-63b8-4f90-a3e5-da4b0c353046")),
        None
    );
}

#[test]
fn legacy_subagent_block_omits_keyboard_hints() {
    let (root, events) = replay_events("claude_subagents.jsonl");
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual();
    let generation = chat.session_generation;
    chat.subagents.set_root(acp::SessionId::new(root));
    let both_running = events
        .iter()
        .position(|event| matches!(event, SessionEvent::Acp(AcpEvent::ChildNotification(_))))
        .expect("a child update");
    for event in events[..both_running].iter().cloned() {
        chat.handle_session_event(generation, event);
    }

    // The block does not show keyboard help while the parent is working.
    chat.bottom_pane.set_task_running(true);
    assert!(!chat.subagents_stoppable());
    let working = block_lines(&chat, 80);
    assert!(
        !working.iter().any(|line| line.contains("esc to stop")),
        "{working:#?}"
    );

    // The block stays free of keyboard help when its parent becomes idle.
    chat.bottom_pane.set_task_running(false);
    assert!(chat.subagents_stoppable());
    let idle = block_lines(&chat, 80);
    assert!(
        !idle.iter().any(|line| line.contains("esc to stop")),
        "{idle:#?}"
    );
}

#[test]
fn running_subagent_cards_render_in_chat_and_are_clickable() {
    use crate::render::renderable::Renderable;
    use crate::subagent_switcher::Target;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    let (root, events) = replay_events("claude_subagents.jsonl");
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual();
    chat.bottom_pane
        .set_composer_placeholder_for_test("Ask Nori to do anything");
    let generation = chat.session_generation;
    chat.subagents.set_root(acp::SessionId::new(root));
    let both_running = events
        .iter()
        .position(|event| matches!(event, SessionEvent::Acp(AcpEvent::ChildNotification(_))))
        .unwrap();
    for event in events[..both_running].iter().cloned() {
        chat.handle_session_event(generation, event);
    }
    chat.pre_draw_tick();
    chat.set_screen_rows(30);
    let area = Rect::new(0, 3, 110, chat.desired_height(110));
    let mut buffer = Buffer::empty(area);
    chat.render(area, &mut buffer);
    let hits = chat.subagent_card_hits.borrow().clone();
    assert_eq!(
        hits.len(),
        2,
        "two side-by-side in-chat cards on >=100 cols"
    );
    assert_eq!(hits[0].0.y, hits[1].0.y, "side-by-side row alignment");
    let first = chat
        .subagent_at(hits[0].0.x + 1, hits[0].0.y + 1)
        .expect("first card hit");
    let second = chat
        .subagent_at(hits[1].0.x + 1, hits[1].0.y + 1)
        .expect("second card hit");
    assert_ne!(first, second);
    // Card sits ABOVE the composer, never below it.
    let composer_top = chat.bottom_pane_area.get().expect("composer area").y;
    assert!(hits[0].0.bottom() <= composer_top);
    let rendered = (area.y..area.bottom())
        .map(|y| {
            (area.x..area.right())
                .map(|x| buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        rendered
            .lines()
            .any(|line| line.trim_end().ends_with("● 2")),
        "statusline should show compact agent count:\n{rendered}"
    );
    let (entries, _) = chat.subagents.entries(Instant::now());
    assert!(
        entries
            .iter()
            .any(|entry| entry.target == Target::Subagent(first))
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry.target == Target::Subagent(second))
    );
    assert_eq!(chat.subagent_at(area.x, composer_top + 1), None);
}

#[test]
fn focused_child_uses_the_shared_conversation_and_composer() {
    use crate::render::renderable::Renderable;
    use crate::subagent_switcher::SubagentKey;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::text::Line;

    let (mut chat, _rx, _op_rx) = make_chatwidget_manual();
    chat.bottom_pane
        .set_composer_placeholder_for_test("Ask Nori to do anything");
    chat.set_screen_rows(16);
    chat.set_focused_subagent_view(
        Some(SubagentKey(1)),
        vec![
            Line::from("Investigate build · running"),
            Line::from("› Please inspect the build"),
            Line::from("I found the failing dependency."),
            Line::from("Failed to load session sample: not found"),
        ],
        0,
    );
    let area = Rect::new(0, 0, 80, 16);
    let mut buffer = Buffer::empty(area);
    chat.render(area, &mut buffer);
    let rendered = (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!("focused_child_shared_conversation", rendered);
}

#[test]
fn child_login_uses_the_announced_agent_and_never_falls_back_to_main() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual();
    let root = acp::SessionId::new("root");
    chat.subagents.set_root(root.clone());
    chat.subagents.on_update(
        nori_protocol::SubagentUpdate {
            parent_session_id: root,
            session_id: acp::SessionId::new("child"),
            name: nori_protocol::Patch::Keep,
            title: nori_protocol::Patch::Keep,
            description: nori_protocol::Patch::Keep,
            agent: nori_protocol::Patch::Set("unknown-child-provider".to_owned()),
            model: nori_protocol::Patch::Keep,
            state: nori_protocol::Patch::Set(nori_protocol::SubagentState::Idle {
                interrupted: false,
            }),
        },
        Instant::now(),
    );
    chat.set_focused_subagent_view(
        Some(crate::subagent_switcher::SubagentKey(1)),
        Vec::new(),
        0,
    );
    while rx.try_recv().is_ok() {}
    chat.handle_login_command();
    assert!(
        matches!(rx.try_recv(), Ok(AppEvent::ChildSessionOutput { key: crate::subagent_switcher::SubagentKey(1), text }) if text.contains("unknown-child-provider"))
    );
    assert!(rx.try_recv().is_err());
}

#[test]
fn child_login_refuses_missing_agent_identity() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual();
    chat.set_focused_subagent_view(
        Some(crate::subagent_switcher::SubagentKey(7)),
        Vec::new(),
        0,
    );
    while rx.try_recv().is_ok() {}
    chat.handle_login_command();
    assert!(
        matches!(rx.try_recv(), Ok(AppEvent::TransientNotice(message)) if message.contains("does not identify its provider"))
    );
    assert!(rx.try_recv().is_err());
}

#[test]
fn narrow_child_view_keeps_prompt_and_clickable_subagent_rows() {
    use crate::render::renderable::Renderable;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::text::Line;

    let (root, events) = replay_events("claude_subagents.jsonl");
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual();
    chat.bottom_pane
        .set_composer_placeholder_for_test("Ask Nori to do anything");
    let generation = chat.session_generation;
    chat.subagents.set_root(acp::SessionId::new(root));
    let both_running = events
        .iter()
        .position(|event| matches!(event, SessionEvent::Acp(AcpEvent::ChildNotification(_))))
        .unwrap();
    for event in events[..both_running].iter().cloned() {
        chat.handle_session_event(generation, event);
    }
    chat.set_screen_rows(25);
    chat.set_focused_subagent_view(
        Some(crate::subagent_switcher::SubagentKey(1)),
        vec![
            Line::from("Research task · running"),
            Line::from("I found the failure."),
        ],
        0,
    );
    let area = Rect::new(0, 0, 30, 25);
    let mut buffer = Buffer::empty(area);
    chat.render(area, &mut buffer);
    let rendered = (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let rendered = rendered
        .chars()
        .map(|ch| {
            if "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(ch) {
                '•'
            } else {
                ch
            }
        })
        .collect::<String>();
    insta::assert_snapshot!("narrow_child_conversation", rendered);
    assert!(chat.subagent_card_hits.borrow().is_empty());
    assert!(rendered.contains('›'), "{rendered}");
}

#[test]
fn narrow_main_history_scroll_keeps_the_shared_prompt() {
    use crate::render::renderable::Renderable;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::text::Line;

    let (mut chat, _rx, _op_rx) = make_chatwidget_manual();
    chat.bottom_pane
        .set_composer_placeholder_for_test("Ask Nori to do anything");
    chat.set_screen_rows(25);
    chat.set_main_history_view(
        vec![
            Line::from("Earlier user request"),
            Line::from("Earlier agent response"),
            Line::from("Latest response"),
        ],
        1,
    );
    let area = Rect::new(0, 0, 30, 25);
    let mut buffer = Buffer::empty(area);
    chat.render(area, &mut buffer);
    let rendered = (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!("narrow_main_history_scroll", rendered);
    assert!(rendered.contains('›'), "{rendered}");
}

#[test]
fn narrow_strip_is_removed_when_all_children_finish() {
    use crate::render::renderable::Renderable;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    let (mut chat, _rx, _op_rx) = make_chatwidget_manual();
    chat.bottom_pane
        .set_composer_placeholder_for_test("Ask Nori to do anything");
    let root = acp::SessionId::new("root");
    chat.subagents.set_root(root.clone());
    for (session, title, state) in [
        (
            "idle",
            "Research",
            nori_protocol::SubagentState::Idle { interrupted: true },
        ),
        (
            "done",
            "Review",
            nori_protocol::SubagentState::Ended(nori_protocol::SubagentOutcome::Completed {
                stop_reason: None,
            }),
        ),
    ] {
        chat.subagents.on_update(
            nori_protocol::SubagentUpdate {
                parent_session_id: root.clone(),
                session_id: acp::SessionId::new(session),
                agent: nori_protocol::Patch::Keep,
                model: nori_protocol::Patch::Keep,
                name: nori_protocol::Patch::Keep,
                title: nori_protocol::Patch::Set(title.to_owned()),
                description: nori_protocol::Patch::Keep,
                state: nori_protocol::Patch::Set(state),
            },
            Instant::now(),
        );
    }
    chat.set_screen_rows(25);
    let area = Rect::new(0, 0, 30, 25);
    let mut buffer = Buffer::empty(area);
    chat.render(area, &mut buffer);
    let rendered = (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!("narrow_idle_completed_no_strip", rendered);
    assert!(chat.subagent_card_hits.borrow().is_empty());
    assert_eq!(chat.subagent_at(1, 23), None);
    assert_eq!(chat.subagents.running_count(), 0);
}

#[test]
fn child_queue_updates_do_not_replace_the_main_queue() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual();
    let root = acp::SessionId::new("root");
    let child = acp::SessionId::new("child");
    chat.subagents.set_root(root.clone());
    chat.subagents.on_update(
        nori_protocol::SubagentUpdate {
            parent_session_id: root,
            session_id: child.clone(),
            agent: nori_protocol::Patch::Keep,
            model: nori_protocol::Patch::Keep,
            name: nori_protocol::Patch::Keep,
            title: nori_protocol::Patch::Keep,
            description: nori_protocol::Patch::Keep,
            state: nori_protocol::Patch::Set(nori_protocol::SubagentState::Running),
        },
        Instant::now(),
    );
    let generation = chat.session_generation;
    chat.handle_session_event(
        generation,
        SessionEvent::Nori(NoriEvent::QueueChanged(nori_protocol::QueueSnapshot {
            prompts: vec!["main".to_owned()],
        })),
    );
    chat.handle_session_event(
        generation,
        SessionEvent::Nori(NoriEvent::SubagentQueueChanged {
            session_id: child,
            prompts: vec!["child".to_owned()],
        }),
    );
    assert_eq!(chat.main_queued_prompts, vec!["main".to_owned()]);
    chat.set_focused_subagent_view(
        Some(crate::subagent_switcher::SubagentKey(1)),
        Vec::new(),
        0,
    );
    assert!(chat.bottom_pane.has_queued_user_messages());
    chat.handle_session_event(
        generation,
        SessionEvent::Nori(NoriEvent::QueueChanged(nori_protocol::QueueSnapshot {
            prompts: Vec::new(),
        })),
    );
    assert!(chat.bottom_pane.has_queued_user_messages());
    chat.set_focused_subagent_view(None, Vec::new(), 0);
    assert!(!chat.bottom_pane.has_queued_user_messages());
}

#[test]
fn lanes_name_each_subagent_and_count_the_whole_burst_with_idle_as_an_open_circle() {
    use crate::render::renderable::Renderable;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    let (mut chat, _rx, _op_rx) = make_chatwidget_manual();
    chat.bottom_pane
        .set_composer_placeholder_for_test("Ask Nori to do anything");
    chat.subagents.set_root(acp::SessionId::new("root"));
    // lead -> auth, config; auth -> token-scout, session-scout.
    for (session, parent, name, task, state) in [
        (
            "auth",
            "root",
            "auth",
            "Audit the auth middleware",
            nori_protocol::SubagentState::Running,
        ),
        (
            "config",
            "root",
            "config",
            "Trace the config loader's callers",
            nori_protocol::SubagentState::Running,
        ),
        (
            "token-scout",
            "auth",
            "token-scout",
            "Read src/auth/token.rs",
            nori_protocol::SubagentState::Running,
        ),
        (
            "session-scout",
            "auth",
            "session-scout",
            "Read src/auth/session.rs",
            nori_protocol::SubagentState::Idle { interrupted: true },
        ),
    ] {
        chat.subagents.on_update(
            nori_protocol::SubagentUpdate {
                parent_session_id: acp::SessionId::new(parent),
                session_id: acp::SessionId::new(session),
                agent: nori_protocol::Patch::Keep,
                model: nori_protocol::Patch::Keep,
                name: nori_protocol::Patch::Set(name.to_owned()),
                title: nori_protocol::Patch::Set("reviewer".to_owned()),
                description: nori_protocol::Patch::Set(task.to_owned()),
                state: nori_protocol::Patch::Set(state),
            },
            Instant::now(),
        );
    }
    // One scout finished: it counts under the header though it sits below a running parent.
    chat.subagents.on_update(
        nori_protocol::SubagentUpdate {
            parent_session_id: acp::SessionId::new("auth"),
            session_id: acp::SessionId::new("token-scout"),
            agent: nori_protocol::Patch::Keep,
            model: nori_protocol::Patch::Keep,
            name: nori_protocol::Patch::Keep,
            title: nori_protocol::Patch::Keep,
            description: nori_protocol::Patch::Keep,
            state: nori_protocol::Patch::Set(nori_protocol::SubagentState::Ended(
                nori_protocol::SubagentOutcome::Completed { stop_reason: None },
            )),
        },
        Instant::now(),
    );
    chat.set_screen_rows(24);
    let area = Rect::new(0, 0, 100, 24);
    let mut buffer = Buffer::empty(area);
    chat.render(area, &mut buffer);
    let rendered = (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!("lanes_burst_counts_and_names", rendered);
    assert_eq!(chat.subagents.running_count(), 2);
}

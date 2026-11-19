//! Selecting messages in the transcript overlay and opening their actions.

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use crossterm::event::MouseButton;
use crossterm::event::MouseEvent;
use crossterm::event::MouseEventKind;
use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;

use super::MessageAction;
use super::MessageChoice;
use super::TranscriptOverlay;
use crate::history_cell::AgentMarkdownCell;
use crate::history_cell::AgentMessageCell;
use crate::history_cell::HistoryCell;
use crate::history_cell::UserHistoryCell;

const AREA: Rect = Rect {
    x: 0,
    y: 0,
    width: 60,
    height: 20,
};

/// Two turns: a prompt, a two-line answer and its continuation, then a second prompt and answer.
fn conversation() -> TranscriptOverlay {
    let cells: Vec<Arc<dyn HistoryCell>> = vec![
        Arc::new(UserHistoryCell {
            message: "Why does the parser fail?".to_owned(),
        }),
        Arc::new(AgentMessageCell::new(
            vec![
                Line::from("It reads past the end."),
                Line::from("Here is why."),
            ],
            true,
        )),
        Arc::new(AgentMessageCell::new(
            vec![Line::from("And the fix.")],
            false,
        )),
        Arc::new(UserHistoryCell {
            message: "Fix it".to_owned(),
        }),
        Arc::new(AgentMessageCell::new(vec![Line::from("Fixed.")], true)),
    ];
    let mut overlay = TranscriptOverlay::new(cells);
    // Start at the top so every cell is on screen.
    overlay.view.scroll_offset = 0;
    render(&mut overlay);
    overlay
}

fn render(overlay: &mut TranscriptOverlay) -> Buffer {
    let mut buf = Buffer::empty(AREA);
    overlay.render(AREA, &mut buf);
    buf
}

fn text(buf: &Buffer) -> String {
    (0..buf.area.height)
        .map(|y| {
            let row: String = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect();
            row.trim_end().to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The screen row showing `needle`.
fn row_of(overlay: &mut TranscriptOverlay, needle: &str) -> u16 {
    let buf = render(overlay);
    text(&buf)
        .lines()
        .position(|line| line.contains(needle))
        .and_then(|row| u16::try_from(row).ok())
        .unwrap_or_else(|| panic!("{needle:?} is not on screen:\n{}", text(&buf)))
}

fn click(overlay: &mut TranscriptOverlay, row: u16, at: Instant) {
    overlay.on_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row,
            modifiers: KeyModifiers::NONE,
        },
        at,
    );
}

fn key(overlay: &mut TranscriptOverlay, code: KeyCode) {
    overlay.on_key(KeyEvent::new(code, KeyModifiers::NONE));
}

#[test]
fn a_click_selects_the_message_under_it_on_any_of_its_rows() {
    let mut overlay = conversation();
    let now = Instant::now();
    // The second row of a wrapped, two-line answer belongs to that answer.
    let second_row = row_of(&mut overlay, "Here is why.");
    click(&mut overlay, second_row, now);
    assert_eq!(overlay.highlight_cell, Some(1));
    let prompt = row_of(&mut overlay, "Fix it");
    click(&mut overlay, prompt, now + Duration::from_secs(1));
    assert_eq!(overlay.highlight_cell, Some(3));
    assert!(overlay.menu_cell().is_none());

    // Scrolled, the same row shows another message, and the click follows it.
    overlay.view.scroll_offset = 2;
    let row = row_of(&mut overlay, "Fixed.");
    click(&mut overlay, row, now + Duration::from_secs(2));
    assert_eq!(overlay.highlight_cell, Some(4));
}

#[test]
fn a_double_click_opens_the_actions_of_one_message() {
    let mut overlay = conversation();
    let now = Instant::now();
    let prompt = row_of(&mut overlay, "Why does the parser fail?");
    let answer = row_of(&mut overlay, "It reads past the end.");

    // Two clicks on different messages are two selections.
    click(&mut overlay, prompt, now);
    click(&mut overlay, answer, now + Duration::from_millis(100));
    assert!(overlay.menu_cell().is_none());
    // Two clicks on one message too far apart are too.
    click(&mut overlay, answer, now + Duration::from_millis(900));
    assert!(overlay.menu_cell().is_none());

    click(&mut overlay, answer, now + Duration::from_millis(1_100));
    assert_eq!(overlay.menu_cell(), Some(1));
    key(&mut overlay, KeyCode::Enter);
    assert_eq!(
        overlay.take_chosen(),
        Some(MessageAction {
            cell: 1,
            choice: MessageChoice::ForkThrough
        })
    );
    assert!(overlay.menu_cell().is_none());
}

#[test]
fn brackets_step_between_messages_and_enter_opens_the_actions() {
    let mut overlay = conversation();
    // With nothing selected, `[` picks the newest message.
    key(&mut overlay, KeyCode::Char('['));
    assert_eq!(overlay.highlight_cell, Some(4));
    key(&mut overlay, KeyCode::Char('['));
    assert_eq!(overlay.highlight_cell, Some(3));
    // The continuation of an answer is not a message of its own.
    key(&mut overlay, KeyCode::Char('['));
    assert_eq!(overlay.highlight_cell, Some(1));
    key(&mut overlay, KeyCode::Char('['));
    key(&mut overlay, KeyCode::Char('['));
    assert_eq!(overlay.highlight_cell, Some(0));
    key(&mut overlay, KeyCode::Char(']'));
    assert_eq!(overlay.highlight_cell, Some(1));
    key(&mut overlay, KeyCode::Char('['));

    key(&mut overlay, KeyCode::Enter);
    key(&mut overlay, KeyCode::Down);
    key(&mut overlay, KeyCode::Enter);
    assert_eq!(
        overlay.take_chosen(),
        Some(MessageAction {
            cell: 0,
            choice: MessageChoice::Rewind
        })
    );
    // Esc closes the actions without choosing.
    key(&mut overlay, KeyCode::Enter);
    key(&mut overlay, KeyCode::Esc);
    assert!(overlay.menu_cell().is_none());
    assert_eq!(overlay.take_chosen(), None);
}

#[test]
fn the_selected_prompt_and_its_actions() {
    let mut overlay = conversation();
    let now = Instant::now();
    let prompt = row_of(&mut overlay, "Fix it");
    click(&mut overlay, prompt, now);
    click(&mut overlay, prompt, now + Duration::from_millis(200));
    insta::assert_snapshot!(text(&render(&mut overlay)));
}

#[test]
fn response_focus_navigates_answers_and_dispatches_actions() {
    let mut overlay = conversation();
    overlay.focus_responses();
    assert_eq!(overlay.highlight_cell, Some(4));
    key(&mut overlay, KeyCode::Char('['));
    assert_eq!(overlay.highlight_cell, Some(1));
    key(&mut overlay, KeyCode::Char('['));
    assert_eq!(overlay.highlight_cell, Some(1));
    key(&mut overlay, KeyCode::Char(']'));
    assert_eq!(overlay.highlight_cell, Some(4));
    key(&mut overlay, KeyCode::Char(' '));
    assert_eq!(
        overlay.take_chosen(),
        Some(MessageAction {
            cell: 4,
            choice: MessageChoice::ForkSubagent
        })
    );
    key(&mut overlay, KeyCode::Char('['));
    key(&mut overlay, KeyCode::Enter);
    assert_eq!(
        overlay.take_chosen(),
        Some(MessageAction {
            cell: 1,
            choice: MessageChoice::RollbackThrough
        })
    );
}

#[test]
fn response_focus_screen() {
    let mut overlay = conversation();
    overlay.focus_responses();
    insta::assert_snapshot!(text(&render(&mut overlay)));
}

#[test]
fn child_response_focus_screen() {
    use nori_protocol::acp::v1 as acp;
    let chunk = |value: &str| {
        nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
            acp::AgentNotification::SessionNotification(acp::SessionNotification::new(
                "child",
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    acp::ContentBlock::Text(acp::TextContent::new(value)),
                )),
            )),
        ))
    };
    let boundary = |end| {
        nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::ConversationResponseBoundary(
            nori_protocol::ConversationResponseBoundary {
                transcript_id: "child-journal".into(),
                start: 1,
                end,
            },
        ))
    };
    let (cells, sources) = crate::chatwidget::subagents::project_child_history(
        &[
            chunk("First child answer"),
            boundary(3),
            chunk("Second child answer"),
            boundary(7),
        ],
        std::path::Path::new("."),
    );
    let mut overlay = TranscriptOverlay::new(cells);
    overlay.focus_responses();
    assert_eq!(
        sources
            .iter()
            .map(|(index, source)| (*index, source.end))
            .collect::<Vec<_>>(),
        vec![(0, 3), (1, 7)]
    );
    insta::assert_snapshot!(text(&render(&mut overlay)));
}

#[test]
fn response_navigation_includes_consolidated_markdown() {
    let cells: Vec<Arc<dyn HistoryCell>> = vec![
        Arc::new(AgentMessageCell::new(vec![Line::from("first")], true)),
        Arc::new(AgentMarkdownCell::new(
            "second".into(),
            std::path::Path::new("."),
        )),
    ];
    let mut overlay = TranscriptOverlay::new(cells);
    overlay.focus_responses();
    assert_eq!(overlay.highlight_cell, Some(1));
    key(&mut overlay, KeyCode::Char('['));
    assert_eq!(overlay.highlight_cell, Some(0));
}

//! `/continue` after a turn that did not finish, and the confirmation a rewind asks for.

use super::*;
use crate::test_backend::VT100Backend;
use nori_protocol::acp::v1::StopReason;
use pretty_assertions::assert_eq;
use ratatui::Terminal;

fn end_turn(
    chat: &mut ChatWidget,
    stop_reason: StopReason,
    failure: Option<crate::presentation::TurnFailure>,
) {
    chat.handle_client_event(crate::presentation::ClientEvent::PromptCompleted(
        crate::presentation::PromptCompleted {
            stop_reason,
            last_agent_message: None,
            failure,
        },
    ));
}

fn history_text(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>) -> String {
    drain_insert_history(rx)
        .iter()
        .map(|cell| lines_to_single_string(cell))
        .collect()
}

fn active_user_message(chat: &ChatWidget) -> Option<String> {
    chat.active_cell.as_ref().and_then(|cell| {
        cell.as_any()
            .downcast_ref::<history_cell::UserHistoryCell>()
            .map(|cell| cell.message.clone())
    })
}

fn screen(chat: &ChatWidget) -> String {
    let height = chat.desired_height(80);
    let mut terminal = Terminal::new(VT100Backend::new(80, height)).expect("terminal");
    terminal
        .draw(|f| chat.render(f.area(), f.buffer_mut()))
        .expect("draw");
    terminal.backend().vt100().screen().contents()
}

#[test]
fn continue_sends_again_a_prompt_the_agent_never_answered() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual();
    chat.note_turn_prompt("summarize the repository");
    end_turn(
        &mut chat,
        StopReason::EndTurn,
        Some(crate::presentation::TurnFailure::Retryable),
    );
    assert!(
        history_text(&mut rx).contains("▌ stopped · /continue"),
        "the user is told the turn can be continued"
    );

    chat.continue_unfinished_turn();

    assert_eq!(
        active_user_message(&chat).as_deref(),
        Some("summarize the repository")
    );
}

#[test]
fn continue_asks_an_agent_that_had_started_to_go_on() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual();
    chat.note_turn_prompt("refactor the parser");
    chat.note_agent_output();
    end_turn(&mut chat, StopReason::Cancelled, None);

    chat.continue_unfinished_turn();

    assert_eq!(
        active_user_message(&chat).as_deref(),
        Some("Continue where you left off.")
    );
}

#[test]
fn a_turn_that_finished_leaves_nothing_to_continue() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual();
    chat.note_turn_prompt("done soon");
    chat.note_agent_output();
    end_turn(&mut chat, StopReason::EndTurn, None);
    let history = history_text(&mut rx);
    assert!(!history.contains("/continue"), "{history}");

    chat.continue_unfinished_turn();

    assert!(history_text(&mut rx).contains("Nothing to continue"));
    assert_eq!(active_user_message(&chat), None);
}

#[tokio::test]
async fn a_rewind_names_the_turns_whose_files_it_restores() {
    let session = super::part10::start_mock_session().await;
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual();
    chat.harness_handle = Some(session.handle.clone());
    chat.pending_rewind = Some(crate::chatwidget::Rewind {
        cell_index: 4,
        prompt: "fix the parser".to_owned(),
        prompts: vec!["fix the parser".to_owned(), "add a test".to_owned()],
    });
    let snapshot = |index: i64, label: &str, short_id: &str| nori_harness::UndoSnapshot {
        index,
        short_id: short_id.to_owned(),
        label: label.to_owned(),
    };

    chat.on_undo_snapshots_loaded(vec![
        snapshot(0, "add a test", "1 file"),
        snapshot(1, "fix the parser", "2 files"),
        snapshot(2, "set up", "3 files"),
    ]);

    let contents = screen(&chat);
    assert!(
        contents.contains("Rewind to “fix the parser”"),
        "{contents}"
    );
    assert!(
        contents.contains("Restoring undoes what 2 turns changed"),
        "{contents}"
    );
    // The turns it restores, oldest first, beside the choice (wrapped at 80 columns).
    assert!(
        contents.contains("fix the parser (2 files), add a test"),
        "{contents}"
    );
    assert!(!contents.contains("set up"), "{contents}");
    assert!(
        contents.contains("Restore files and edit the prompt"),
        "{contents}"
    );
    assert!(
        contents.contains("Edit the prompt, keep the files"),
        "{contents}"
    );
    session.handle.shutdown().await.expect("shutdown");
}

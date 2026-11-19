#![cfg(unix)]
use std::time::Duration;
use tui_pty_e2e::Key;
use tui_pty_e2e::SessionConfig;
use tui_pty_e2e::TuiSession;

#[test]
fn pageup_reaches_inherited_long_child_history() {
    let response = (0..45)
        .map(|index| format!("long-history-line-{index:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut session =
        TuiSession::spawn_with_config(25, 80, SessionConfig::new().with_mock_response(response))
            .expect("start TUI");
    let timeout = Duration::from_secs(30);
    session.wait_for_text("›", timeout).expect("composer");
    session
        .submit_input("long history parent question")
        .expect("parent prompt");
    session
        .wait_for_text("long-history-line-44", timeout)
        .expect("parent response");
    session.type_input("ROOT-DRAFT-MARKER").expect("root draft");
    session.send_str("\x1b[27u").expect("normal mode");
    session.wait_for_text("NORMAL", timeout).expect("normal");
    session.send_str("[").expect("focus answer");
    session
        .wait_for_text("Turn 1 of 1", timeout)
        .expect("focused");
    session.send_str(" ").expect("fork answer");
    session
        .wait_for_text("History fork", timeout)
        .expect("fork card");
    session.send_str("\x1b[9;5u").expect("focus child");
    session
        .wait_for(
            |screen| {
                !screen.contains("ROOT-DRAFT-MARKER")
                    && screen.contains("long-history-line-44")
                    // Completed children leave the running-only strip while
                    // their focused transcript and inherited history remain.
                    && !screen.contains("History fork")
            },
            timeout,
        )
        .expect("child reply completed");
    session
        .send_str("\x1b[5~\x1b[5~\x1b[5~")
        .expect("three PageUp keys");
    session
        .wait_for_text("Continue from the conversation above", timeout)
        .expect("inherited history after three PageUps");
    session.send_key(Key::Tab).expect("return to parent");
    session
        .wait_for_text("ROOT-DRAFT-MARKER", timeout)
        .expect("parent draft retained");
    session.send_key(Key::Ctrl('d')).expect("quit");
    assert!(
        session.wait_for_process_exit(Duration::from_secs(15)),
        "TUI did not exit"
    );
}

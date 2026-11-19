//! Mouse routing regressions for the live subagent strip and projected views.

use std::path::PathBuf;

use tui_pty_e2e::Key;
use tui_pty_e2e::SessionConfig;
use tui_pty_e2e::TIMEOUT;
use tui_pty_e2e::TuiSession;

fn practice_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("mock-acp-agent")
        .join("fixtures")
        .join("nori_practice_scrollback.jsonl")
}

fn click_text(session: &mut TuiSession, text: &str) {
    let screen = session.screen_contents();
    let (row, line) = screen
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains(text))
        .unwrap_or_else(|| panic!("could not find {text:?} in:\n{screen}"));
    let prefix = &line[..line.find(text).expect("matched text")];
    let column = prefix.chars().count() + 2;
    let row = row + 1;
    session
        .send_str(&format!("\x1b[<0;{column};{row}M\x1b[<0;{column};{row}m"))
        .expect("send SGR click");
}

#[test]
fn picker_clicks_restore_native_main_history() {
    let config = SessionConfig::new()
        .with_agent("mock-model".to_owned())
        .with_agent_env(
            "MOCK_AGENT_WIRE_REPLAY",
            practice_fixture().to_str().expect("fixture path is UTF-8"),
        )
        .with_agent_env("MOCK_AGENT_REPLAY_SCALE", "10");
    let mut session = TuiSession::spawn_with_config(30, 120, config).expect("spawn nori");
    session.start_output_capture();
    session
        .wait_for_text("Mock Default Model", TIMEOUT)
        .expect("mock session was not ready");
    session.submit_input("run the practice fixture").unwrap();

    // Visit a completed child after the root cells have left the pending native
    // history queue; restoring only that queue loses the committed root history.
    session
        .wait_for_text("Parent final response arrives", TIMEOUT)
        .expect("fixture did not finish");
    let mode = |session: &TuiSession| {
        let mut terminal = vt100::Parser::new(30, 120, 0);
        terminal.process(session.output_bytes().expect("capture"));
        terminal.screen().mouse_protocol_mode()
    };
    assert_eq!(mode(&session), vt100::MouseProtocolMode::None);
    session.send_key(Key::Ctrl(' ')).unwrap();
    session
        .wait_for_text("Conversations", TIMEOUT)
        .expect("picker did not open");
    assert_eq!(mode(&session), vt100::MouseProtocolMode::AnyMotion);
    click_text(&mut session, "researcher");
    session
        .wait_for_text("Child A started", TIMEOUT)
        .expect("mouse click did not focus researcher");
    assert_eq!(mode(&session), vt100::MouseProtocolMode::AnyMotion);

    // Return through the picker and verify the native root history is restored.
    session.send_key(Key::Ctrl(' ')).unwrap();
    session
        .wait_for_text("Conversations", TIMEOUT)
        .expect("picker did not reopen");
    click_text(&mut session, "main");
    session
        .wait_for_text("Parent final response arrives", TIMEOUT)
        .expect("root history was not restored after child focus");
    assert_eq!(mode(&session), vt100::MouseProtocolMode::None);
    assert!(
        session.screen_contents().contains("scrollback line 14"),
        "root scrollback line disappeared after child and wheel navigation:\n{}",
        session.screen_contents()
    );

    session.send_str("abcd").unwrap();
    session
        .wait_for_text("abcd", TIMEOUT)
        .expect("draft was not entered");
    session.send_str("X").unwrap();
    session
        .wait_for_text("abcdX", TIMEOUT)
        .expect("composer did not accept input after history restore");
}

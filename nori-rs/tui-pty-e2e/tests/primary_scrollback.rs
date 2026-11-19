use std::time::Duration;

use pretty_assertions::assert_eq;
use tui_pty_e2e::Key;
use tui_pty_e2e::SessionConfig;
use tui_pty_e2e::TuiSession;

#[test]
fn widening_and_growing_keeps_one_composer_and_its_draft() {
    let mut session = TuiSession::spawn(25, 30).expect("start TUI");
    session
        .wait_for_text("›", Duration::from_secs(20))
        .expect("prompt");
    session.send_str("resize draft marker").expect("type draft");
    session
        .wait_for_text("resize draft marker", Duration::from_secs(10))
        .expect("draft");
    session.resize(43, 122).expect("resize");
    session
        .wait_for(
            |screen| {
                screen
                    .lines()
                    .enumerate()
                    .any(|(row, line)| row >= 38 && line.contains("resize draft marker"))
            },
            Duration::from_secs(15),
        )
        .expect("draft repinned after resize");
    let screen = session.screen_contents();
    assert_eq!(screen.matches("resize draft marker").count(), 1, "{screen}");
}

#[test]
fn primary_history_survives_child_views_resize_and_exit() {
    let response = (0..45)
        .map(|i| format!("native-history-line-{i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let config = SessionConfig::new().with_mock_response(response);
    let mut session = TuiSession::spawn_with_config(25, 80, config).expect("start TUI");
    session.start_output_capture();
    session
        .wait_for_text("›", Duration::from_secs(20))
        .expect("prompt");
    session
        .submit_input("native scrollback question")
        .expect("send prompt");
    session
        .wait_for_text("native-history-line-44", Duration::from_secs(20))
        .expect("response");
    let before = session.scrollback_contents(20);
    assert!(before.contains("native-history-line-30"), "{before}");
    let main_output = session.output_bytes().expect("capture");
    let mut terminal = vt100::Parser::new(25, 80, 0);
    terminal.process(main_output);
    assert_eq!(
        terminal.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None,
        "main view must leave wheel and selection to the terminal"
    );
    session.submit_input("/model").expect("open model picker");
    session
        .wait_for_text("Select model", Duration::from_secs(20))
        .expect("model picker");
    let mut terminal = vt100::Parser::new(25, 80, 0);
    terminal.process(session.output_bytes().expect("capture"));
    assert_eq!(
        terminal.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::AnyMotion
    );
    session.send_key(Key::Enter).expect("select model");
    session
        .wait_for(
            |screen| !screen.contains("Select model"),
            Duration::from_secs(10),
        )
        .expect("picker closed");
    let mut terminal = vt100::Parser::new(25, 80, 0);
    terminal.process(session.output_bytes().expect("capture"));
    assert_eq!(
        terminal.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    assert!(
        session
            .scrollback_contents(20)
            .contains("native-history-line-30")
    );
    session
        .send_str("\x1b[27u")
        .expect("enter normal mode for focus-backwards");
    session
        .wait_for_text("NORMAL", Duration::from_secs(10))
        .expect("normal mode");
    session.send_str("[").expect("focus response");
    session
        .wait_for_text("Turn 1 of 1", Duration::from_secs(10))
        .expect("focused");
    session.send_str(" ").expect("fork response");
    session
        .wait_for_text("History fork", Duration::from_secs(20))
        .expect("fork card");
    session
        .send_str("\x1b[9;5u")
        .expect("focus child with Ctrl+Tab");
    session
        .send_str("child-only draft")
        .expect("type child draft");
    session
        .wait_for_text("child-only draft", Duration::from_secs(20))
        .expect("child draft");
    let mut terminal = vt100::Parser::new(25, 80, 0);
    terminal.process(session.output_bytes().expect("capture"));
    assert_eq!(
        terminal.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::AnyMotion
    );
    session.send_key(Key::Tab).expect("return to parent");
    session
        .wait_for(
            |screen| !screen.contains("child-only draft") && screen.contains('›'),
            Duration::from_secs(10),
        )
        .expect("parent view restored");
    let after = session.scrollback_contents(20);
    assert!(after.contains("native-history-line-30"), "{after}");
    let mut terminal = vt100::Parser::new(25, 80, 0);
    terminal.process(session.output_bytes().expect("capture"));
    assert_eq!(
        terminal.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    session.resize(35, 100).expect("resize");
    session
        .wait_for_text("›", Duration::from_secs(10))
        .expect("parent prompt");
    session
        .send_str("i")
        .expect("return parent composer to insert mode");
    session
        .wait_for_text("INSERT", Duration::from_secs(10))
        .expect("insert mode");
    session.submit_input("/quit").expect("quit");
    assert!(
        session.wait_for_process_exit(Duration::from_secs(15)),
        "TUI did not exit"
    );
    let _ = session.poll();
    let output = session.output_bytes().expect("capture");
    assert!(
        !output
            .windows(b"\x1b[?1049h".len())
            .any(|part| part == b"\x1b[?1049h"),
        "child view must project inline on the primary screen without alternate screen"
    );
    assert!(
        !output
            .windows(b"\x1b[?1049l".len())
            .any(|part| part == b"\x1b[?1049l"),
        "temporary view must stay on the primary screen without alt-screen leave sequences"
    );
    assert!(
        !output
            .windows(b"\x1b[3J".len())
            .any(|part| part == b"\x1b[3J"),
        "cleared native scrollback"
    );
}

#[test]
fn immediate_quit_stays_fast_with_large_pending_tool_history() {
    let fixture = tempfile::NamedTempFile::new().expect("fixture file");
    let text = format!("{}\nqueue-tail", "pending output\n".repeat(2_000));
    let steps = [
        serde_json::json!({"t_ms":0,"update":{"sessionUpdate":"tool_call","toolCallId":"slow","title":"Slow command","kind":"execute","status":"in_progress","rawInput":{"command":"sleep"}}}),
        serde_json::json!({"t_ms":20,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text}}}),
        serde_json::json!({"t_ms":60000,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"late"}}}),
    ];
    std::fs::write(
        fixture.path(),
        steps
            .into_iter()
            .map(|step| step.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .expect("write fixture");
    let config = SessionConfig::new().with_agent_env(
        "MOCK_AGENT_WIRE_REPLAY",
        fixture.path().to_str().expect("fixture path"),
    );
    let mut session = TuiSession::spawn_with_config(25, 80, config).expect("start TUI");
    session
        .wait_for_text("›", Duration::from_secs(20))
        .expect("prompt");
    session
        .submit_input("start slow tool")
        .expect("send prompt");
    session
        .wait_for_text("queue-tail", Duration::from_secs(20))
        .expect("pending output shown");
    session.send_key(Key::Ctrl('d')).expect("quit");
    assert!(
        session.wait_for_process_exit(Duration::from_secs(2)),
        "Ctrl+D waited for pending native history"
    );
}

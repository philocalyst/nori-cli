#![cfg(unix)]

use std::time::Duration;

use tui_pty_e2e::Key;
use tui_pty_e2e::SessionConfig;
use tui_pty_e2e::TIMEOUT;
use tui_pty_e2e::TuiSession;

#[test]
fn interleaved_tool_completes_in_place_and_expands_its_evidence() {
    let config = SessionConfig::new()
        .with_agent("mock-model".to_owned())
        .with_agent_env("MOCK_AGENT_INTERLEAVED_TOOL_CALL", "1");
    let mut session = TuiSession::spawn_with_config(24, 80, config).expect("start ACP session");
    session.wait_for_text("›", TIMEOUT).expect("prompt");
    session
        .submit_input("Test interleaved")
        .expect("submit prompt");
    session
        .wait_for_text("Interleaved test done", Duration::from_secs(15))
        .expect("final answer");
    session
        .wait_for_text("Ran test", Duration::from_secs(15))
        .expect("completed tool");
    let contents = session.screen_contents();
    assert_eq!(contents.matches("Ran test").count(), 1, "{contents}");
    assert_eq!(contents.matches("Running test").count(), 0, "{contents}");
    assert!(
        contents.find("Ran test") < contents.find("Interleaved test done"),
        "{contents}"
    );
    session.send_str("\x1be").expect("expand tool with Alt+E");
    let _ = Key::Tab;
    session
        .wait_for_text("Exit code", Duration::from_secs(10))
        .expect("expanded exit status");
    session
        .wait_for_text("Command completed", Duration::from_secs(10))
        .expect("tool output");
    let expanded = session.screen_contents();
    assert!(
        expanded.contains("Exit code") && expanded.contains('0'),
        "{expanded}"
    );
}

//! E2E test for the agent-sourced `/resume` session picker.
//!
//! When the live ACP agent advertises both the `session/list` and
//! `load_session` capabilities, the in-session `/resume` command sources its
//! picker rows from the agent (via `session/list`) instead of the local
//! transcript store, and resuming a row loads it over ACP via `session/load`.
//! This drives the real `nori` binary against the mock agent and verifies the
//! agent's sessions show up in the picker and can be resumed.

#[cfg(target_os = "linux")]
use std::time::Duration;
use tui_pty_e2e::Key;
use tui_pty_e2e::SessionConfig;
use tui_pty_e2e::TIMEOUT;
use tui_pty_e2e::TIMEOUT_INPUT;
use tui_pty_e2e::TuiSession;

#[test]
#[cfg(target_os = "linux")]
fn resume_picker_lists_agent_sessions_when_session_list_supported() {
    let config = SessionConfig::new()
        .with_agent("mock-model".to_owned())
        .with_agent_env("MOCK_AGENT_SUPPORT_SESSION_LIST", "1")
        .with_agent_env("MOCK_AGENT_SUPPORT_LOAD_SESSION", "1");

    let mut session =
        TuiSession::spawn_with_config(30, 100, config).expect("Failed to spawn session");

    session
        .wait_for_text("›", TIMEOUT)
        .expect("TUI should start");
    // Let the session-capability projection arrive before opening the picker.
    std::thread::sleep(TIMEOUT_INPUT);

    // Dispatch the in-session `/resume` slash command.
    session.submit_input("/resume").unwrap();

    session
        .wait_for_text(
            "Resume a live session or start a new one",
            Duration::from_secs(5),
        )
        .expect("Should show the agent-sourced session picker");

    // The mock agent returns two sessions: the first carries a title, the second
    // falls back to its session id because it has none.
    session
        .wait_for_text("First mock session", Duration::from_secs(5))
        .expect("Picker should list the agent's titled session");
    session
        .wait_for_text("mock-session-2", Duration::from_secs(2))
        .expect("Picker should list the untitled session by its id");

    // Row 0 is the pinned "Start a new session" action; the first listed
    // session is one row down. Selecting it resumes over ACP.
    session.send_key(Key::Down).unwrap();
    std::thread::sleep(TIMEOUT_INPUT);
    session.send_key(Key::Enter).unwrap();
    session
        .wait_for_text("Resuming session with", Duration::from_secs(5))
        .expect("Selecting a listed session should resume it over ACP");
}

#[test]
fn resumed_session_keeps_modal_editor_cursor_and_git_branch() {
    let config = SessionConfig::new()
        .with_agent("mock-model".to_owned())
        .with_agent_env("MOCK_AGENT_SUPPORT_SESSION_LIST", "1")
        .with_agent_env("MOCK_AGENT_SUPPORT_LOAD_SESSION", "1")
        .with_agent_env("MOCK_AGENT_LOAD_SESSION_NOTIFICATION_COUNT", "1")
        .with_agent_env(
            "MOCK_AGENT_MODEL_DISPLAY",
            "Long resumed model display name",
        );
    let mut session = TuiSession::spawn_with_config(30, 64, config).expect("spawn TUI");
    session
        .wait_for_text("INSERT", TIMEOUT)
        .expect("initial insert mode");
    session
        .wait_for_text("master", TIMEOUT)
        .expect("initial git branch");
    std::thread::sleep(TIMEOUT_INPUT);
    session.submit_input("/resume").expect("open resume picker");
    session
        .wait_for_text("First mock session", TIMEOUT)
        .expect("saved session listed");
    session.send_key(Key::Down).expect("select saved session");
    session.send_key(Key::Enter).expect("resume saved session");
    session
        .wait_for_text("Resuming session with", TIMEOUT)
        .expect("resume starts");
    session
        .wait_for_text("replay chunk 0", TIMEOUT)
        .expect("resumed widget receives provider history");
    session
        .wait_for_text("INSERT", TIMEOUT)
        .expect("insert mode after resume");
    session
        .wait_for_text("master", TIMEOUT)
        .expect("git branch after resume");
    session.start_output_capture();
    session
        .type_input("draft")
        .expect("type into resumed session");
    session
        .wait_for_text("draft", TIMEOUT)
        .expect("draft visible");
    assert!(
        session
            .output_bytes()
            .expect("captured output")
            .windows(5)
            .any(|bytes| bytes == b"\x1b[6 q"),
        "resumed insert mode uses a bar cursor"
    );
    session.start_output_capture();
    session.send_str("\x1b[27u").expect("enter normal mode");
    session
        .wait_for_text("NORMAL", TIMEOUT)
        .expect("normal mode after resume");
    assert!(
        session
            .output_bytes()
            .expect("captured output")
            .windows(5)
            .any(|bytes| bytes == b"\x1b[2 q"),
        "resumed normal mode uses a block cursor"
    );
}

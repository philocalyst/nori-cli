use std::time::Duration;

use tui_pty_e2e::Key;
use tui_pty_e2e::SessionConfig;
use tui_pty_e2e::TuiSession;

fn header_before(screen: &str, marker: &str) -> bool {
    let lines = screen.lines().collect::<Vec<_>>();
    let Some(header) = lines.iter().position(|line| line.contains("↶ Turn ")) else {
        return false;
    };
    let Some(message) = lines[header + 1..]
        .iter()
        .position(|line| line.contains(marker))
        .map(|rel| header + 1 + rel)
    else {
        return false;
    };
    message <= header + 2
}

#[test]
fn ctrl_h_l_navigate_only_user_messages_and_restore_the_draft() {
    let config = SessionConfig::new().with_mock_response("MODEL-ANSWER");
    let mut session = TuiSession::spawn_with_config(35, 100, config).expect("start TUI");
    let timeout = Duration::from_secs(20);
    session.wait_for_text("›", timeout).expect("composer");
    session
        .submit_input("FIRST-USER-MESSAGE")
        .expect("first prompt");
    session
        .wait_for_text("MODEL-ANSWER", timeout)
        .expect("first answer");
    session
        .submit_input("SECOND-USER-MESSAGE")
        .expect("second prompt");
    session
        .wait_for(
            |screen| screen.matches("MODEL-ANSWER").count() >= 2,
            timeout,
        )
        .expect("second answer");
    session.type_input("draft stays editable").expect("draft");
    session.send_str("\x1b[27u").expect("normal mode");
    session.wait_for_text("NORMAL", timeout).expect("normal");
    session.send_str("[").expect("latest user");
    session
        .wait_for(
            |screen| header_before(screen, "SECOND-USER-MESSAGE"),
            timeout,
        )
        .expect("latest user selected");
    session.send_str("[").expect("older user");
    session
        .wait_for(
            |screen| header_before(screen, "FIRST-USER-MESSAGE"),
            timeout,
        )
        .expect("older user selected");
    session.send_str("]").expect("newer user");
    session
        .wait_for(
            |screen| header_before(screen, "SECOND-USER-MESSAGE"),
            timeout,
        )
        .expect("newer user selected");
    session.send_str("]").expect("return to composer");
    session
        .wait_for(
            |screen| !screen.contains("↶ Turn ") && screen.contains("draft stays editable"),
            timeout,
        )
        .expect("draft restored automatically");
    session.send_str("a!").expect("continue editing");
    session
        .wait_for_text("draft stays editable!", timeout)
        .expect("draft remains editable");
    let _ = Key::Ctrl('l');
}

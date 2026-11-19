use std::time::Duration;

use tui_pty_e2e::Key;
use tui_pty_e2e::SessionConfig;
use tui_pty_e2e::TuiSession;

fn wait_for_physical_rows(
    session: &mut TuiSession,
    predicate: impl Fn(&[String]) -> bool,
) -> Vec<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        session.poll().expect("read terminal");
        let rows = session.screen_rows();
        if predicate(&rows) {
            return rows;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "physical screen rows:\n{}",
            rows.join("\n")
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn resizing_expands_inline_and_transcript_views_to_the_terminal() {
    let mut session = TuiSession::spawn(25, 30).expect("start narrow TUI");
    session
        .wait_for_text("›", Duration::from_secs(20))
        .expect("prompt");
    session.resize(43, 122).expect("expand terminal");
    session
        .wait_for(
            |screen| {
                screen
                    .lines()
                    .enumerate()
                    .any(|(row, line)| row >= 40 && line.contains('›'))
            },
            Duration::from_secs(10),
        )
        .expect("expanded inline prompt reaches the bottom");

    session.send_key(Key::Ctrl('t')).expect("open transcript");
    session
        .wait_for_text("T R A N S C R I P T", Duration::from_secs(5))
        .expect("transcript");
    session.resize(25, 30).expect("shrink transcript");
    let narrow = wait_for_physical_rows(&mut session, |rows| {
        rows.iter()
            .enumerate()
            .any(|(row, line)| row <= 2 && line.contains("T R A N S C R I P T"))
            && rows
                .iter()
                .enumerate()
                .any(|(row, line)| row >= 22 && line.contains("quit"))
    });
    assert!(
        narrow[0].contains("T R A N S C R I P T")
            && narrow
                .iter()
                .filter(|row| row.contains("T R A N S C R I P T"))
                .count()
                == 1,
        "stale overlay rows: {narrow:#?}"
    );
    session.resize(43, 122).expect("expand transcript");
    wait_for_physical_rows(&mut session, |rows| {
        rows.iter().enumerate().any(|(row, line)| {
            row <= 2 && line.contains("T R A N S C R I P T") && line.chars().count() >= 110
        }) && rows
            .iter()
            .enumerate()
            .any(|(row, line)| row >= 40 && line.contains("quit"))
    });
}

#[test]
fn cleared_history_stays_hidden_after_resize() {
    let config = SessionConfig::new().with_mock_response("clear boundary response");
    let mut session = TuiSession::spawn_with_config(25, 30, config).expect("start narrow TUI");
    session
        .wait_for_text("›", Duration::from_secs(20))
        .expect("prompt");
    session
        .submit_input("clear boundary question")
        .expect("submit question");
    session
        .wait_for_text("clear boundary response", Duration::from_secs(20))
        .expect("response");
    session.send_str("\x1b[108;6u").expect("clear screen");
    session
        .wait_for(
            |screen| !screen.contains("clear boundary response") && screen.contains('›'),
            Duration::from_secs(10),
        )
        .expect("history hidden");
    session.resize(43, 122).expect("expand terminal");
    session
        .wait_for(
            |screen| {
                !screen.contains("clear boundary response")
                    && screen
                        .lines()
                        .enumerate()
                        .any(|(row, line)| row >= 40 && line.contains('›'))
            },
            Duration::from_secs(10),
        )
        .expect("cleared state persists after resize");
}

#[test]
fn response_focus_and_draft_survive_a_resize_burst_returning_to_original_size() {
    let response = (0..1000)
        .map(|i| format!("row-{i:04} Unicode café 界 response payload"))
        .collect::<Vec<_>>()
        .join("\n");
    let config = SessionConfig::new().with_mock_response(&response);
    let mut session = TuiSession::spawn_with_config(43, 122, config).expect("start TUI");
    session
        .wait_for_text("›", Duration::from_secs(20))
        .expect("prompt");
    session
        .submit_input("large history probe")
        .expect("submit question");
    session
        .wait_for_text("row-0999", Duration::from_secs(20))
        .expect("long response");
    session.type_input("preserved draft").expect("type draft");
    session.send_str("\x1b[27u").expect("normal mode");
    session
        .wait_for_text("NORMAL", Duration::from_secs(10))
        .expect("normal mode");
    session.send_str("[").expect("focus response");
    session
        .wait_for(
            |screen| screen.contains("↶ Turn 1 of 1") && screen.contains("large history probe"),
            Duration::from_secs(10),
        )
        .expect("focused sent message");
    for (rows, cols) in [(28, 50), (40, 100), (25, 30), (43, 122)] {
        session.resize(rows, cols).expect("resize burst");
    }
    session
        .wait_for(
            |screen| {
                screen.contains("↶ Turn 1 of 1")
                    && screen.contains("large history probe")
                    && screen
                        .lines()
                        .enumerate()
                        .any(|(row, line)| row >= 40 && line.contains("preserved draft"))
            },
            Duration::from_secs(10),
        )
        .expect("focused response and draft repaint at final size");
}

#[test]
fn parent_draft_stays_visible_after_child_switch_and_resize() {
    let config = SessionConfig::new().with_mock_response("fork source response");
    let mut session = TuiSession::spawn_with_config(43, 122, config).expect("start TUI");
    session
        .wait_for_text("›", Duration::from_secs(20))
        .expect("prompt");
    session
        .submit_input("fork source question")
        .expect("submit question");
    session
        .wait_for_text("fork source response", Duration::from_secs(20))
        .expect("response");
    session
        .type_input("draft survives child switch")
        .expect("type draft");
    session.send_str("\x1b[27u").expect("normal mode");
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
        .expect("focus fork with Ctrl+Tab");
    session
        .wait_for(
            |screen| {
                screen.contains("Continue from the conversation above")
                    && !screen.contains("draft survives child switch")
            },
            Duration::from_secs(20),
        )
        .expect("child history");
    session.send_key(Key::Tab).expect("return to parent");
    session
        .wait_for_text("draft survives child switch", Duration::from_secs(10))
        .expect("parent draft restored");
    session.resize(25, 30).expect("shrink terminal");
    session
        .wait_for(
            |screen| screen.contains("draft survives") && screen.contains("child switch"),
            Duration::from_secs(10),
        )
        .expect("draft visible when narrow");
    session.resize(43, 122).expect("expand terminal");
    session
        .wait_for_text("draft survives child switch", Duration::from_secs(10))
        .expect("draft visible when wide again");
}

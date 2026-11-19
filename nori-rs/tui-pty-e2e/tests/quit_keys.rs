//! Application quit chords work independently of editor mode and popup focus.

#![cfg(unix)]

use std::time::Duration;
use tui_pty_e2e::Key;
use tui_pty_e2e::TIMEOUT;
use tui_pty_e2e::TuiSession;

#[test]
fn ctrl_d_exits_from_composer_and_provider_picker_and_restores_terminal() {
    for popup in [false, true] {
        let mut session = TuiSession::spawn(25, 80).expect("start Nori");
        session
            .wait_for_text("›", TIMEOUT)
            .expect("composer appears");
        if popup {
            session
                .submit_input("/providers")
                .expect("open provider picker");
            session
                .wait_for_text("Providers", TIMEOUT)
                .expect("picker appears");
        }
        session.start_output_capture();
        session
            .send_key(Key::Ctrl('d'))
            .expect("send immediate quit");
        assert!(
            session.wait_for_process_exit(Duration::from_secs(2)),
            "Ctrl+D waited for provider cleanup"
        );
        let _ = session.poll();
        let output =
            String::from_utf8_lossy(session.output_bytes().expect("captured terminal output"));
        assert!(
            !output.contains("\x1b[?1049h") && !output.contains("\x1b[?1049l"),
            "quit must never emit alternate-screen escape sequences"
        );
        assert!(
            output.contains("\x1b[?2004l"),
            "bracketed paste mode was not restored"
        );
    }
}

#[test]
fn first_ctrl_c_does_not_exit_and_second_consecutive_press_quits() {
    let mut session = TuiSession::spawn(25, 80).expect("start Nori");
    session
        .wait_for_text("›", TIMEOUT)
        .expect("composer appears");
    session
        .send_key(Key::Ctrl('c'))
        .expect("send first interrupt");
    assert!(
        !session.wait_for_process_exit(Duration::from_millis(200)),
        "first Ctrl+C quit Nori"
    );
    session
        .send_key(Key::Ctrl('c'))
        .expect("send second interrupt");
    assert!(
        session.wait_for_process_exit(Duration::from_secs(2)),
        "second Ctrl+C did not quit promptly"
    );
}

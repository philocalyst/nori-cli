use pretty_assertions::assert_eq;
use tui_pty_e2e::Key;
use tui_pty_e2e::TIMEOUT;
use tui_pty_e2e::TIMEOUT_INPUT;
use tui_pty_e2e::TuiSession;

/// Ctrl+Space (a NUL byte on the wire) opens the subagent switcher even with no subagents, shows
/// the main session and the empty child list, and closes again on Esc and on a second
/// Ctrl+Space.
#[test]
fn ctrl_space_opens_the_switcher_and_closes_it_again() {
    let mut session = TuiSession::spawn(30, 120).unwrap();
    session.start_output_capture();
    session
        .wait_for_text("›", TIMEOUT)
        .expect("Prompt did not appear");
    std::thread::sleep(TIMEOUT_INPUT);
    session.send_key(Key::Ctrl(' ')).unwrap();
    session
        .wait_for_text("No child conversations", TIMEOUT)
        .expect("The switcher did not open");
    let open = session.screen_contents();
    assert!(open.contains("◇ Conversations"), "{open}");
    assert!(open.contains("main"), "{open}");
    // Check what a real terminal receives after the picker redraw. Injecting
    // mouse escape sequences alone would hide a disabled reporting mode.
    let mut terminal = vt100::Parser::new(30, 120, 0);
    let output = session.output_bytes().expect("terminal capture");
    terminal.process(output);
    let open_bytes = output.len();
    assert_eq!(
        (
            terminal.screen().mouse_protocol_mode(),
            terminal.screen().mouse_protocol_encoding(),
        ),
        (
            vt100::MouseProtocolMode::AnyMotion,
            vt100::MouseProtocolEncoding::Sgr
        )
    );

    session.send_key(Key::Escape).unwrap();
    session
        .wait_for(|screen| !screen.contains("No child conversations"), TIMEOUT)
        .expect("Esc did not close the switcher");
    session
        .wait_for_text("›", TIMEOUT)
        .expect("The prompt did not come back");
    terminal.process(&session.output_bytes().expect("terminal capture")[open_bytes..]);
    assert_eq!(
        terminal.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );

    session.send_key(Key::Ctrl(' ')).unwrap();
    session
        .wait_for_text("No child conversations", TIMEOUT)
        .expect("The switcher did not open again");
    session.send_key(Key::Ctrl(' ')).unwrap();
    session
        .wait_for(|screen| !screen.contains("No child conversations"), TIMEOUT)
        .expect("Ctrl+Space did not close the switcher");
}

/// Typing in the switcher filters it instead of reaching the composer, and Enter on `main`
/// returns to the chat.
#[test]
fn the_switcher_takes_typing_and_enter_on_main_returns_to_the_chat() {
    let mut session = TuiSession::spawn(30, 120).unwrap();
    session
        .wait_for_text("›", TIMEOUT)
        .expect("Prompt did not appear");
    std::thread::sleep(TIMEOUT_INPUT);

    session.send_key(Key::Ctrl(' ')).unwrap();
    session
        .wait_for_text("No child conversations", TIMEOUT)
        .expect("The switcher did not open");
    session.send_str("zzz").unwrap();
    session
        .wait_for_text("no matches", TIMEOUT)
        .expect("Typing did not filter the list");
    session.send_key(Key::Ctrl('u')).unwrap();
    session
        .wait_for(|screen| screen.contains("No child conversations"), TIMEOUT)
        .expect("Ctrl+U did not clear the query");
    session.send_key(Key::Enter).unwrap();
    session
        .wait_for(|screen| !screen.contains("No child conversations"), TIMEOUT)
        .expect("Enter on main did not return to the chat");
    assert!(
        !session.screen_contents().contains("zzz"),
        "what was typed in the switcher must not reach the composer"
    );
}

/// Ctrl+Tab cycles through the transient switcher and restores native root scrolling.
#[test]
fn kitty_ctrl_tab_returns_from_the_transient_switcher() {
    let mut session = TuiSession::spawn(30, 120).unwrap();
    session
        .wait_for_text("›", TIMEOUT)
        .expect("Prompt did not appear");
    std::thread::sleep(TIMEOUT_INPUT);
    session.start_output_capture();

    session.send_key(Key::Ctrl(' ')).unwrap();
    session
        .wait_for_text("No child conversations", TIMEOUT)
        .expect("The switcher did not open");

    session.send_str("\x1b[9;5u").unwrap();
    session.send_str("\x1b[9;5:3u").unwrap();
    session
        .wait_for(|screen| !screen.contains("No child conversations"), TIMEOUT)
        .expect("Ctrl+Tab did not cycle back to the only session");

    let output = session
        .output_bytes()
        .expect("terminal output capture was started");
    for sequence in [b"\x1b[?1049h".as_slice(), b"\x1b[?1049l".as_slice()] {
        assert!(
            !output
                .windows(sequence.len())
                .any(|window| window == sequence),
            "switcher must stay on the primary screen and never emit alternate-screen escapes"
        );
    }
}

/// A narrow fresh chat leaves wheel and text-selection ownership with the terminal.
#[test]
fn narrow_fresh_session_keeps_native_mouse_ownership_and_keyboard_editing() {
    let mut session = TuiSession::spawn(25, 30).unwrap();
    session.start_output_capture();
    session
        .wait_for_text("›", TIMEOUT)
        .expect("prompt did not appear");
    session.type_input("abcd").expect("draft renders");
    let mut terminal = vt100::Parser::new(25, 30, 0);
    terminal.process(session.output_bytes().expect("terminal capture"));
    assert_eq!(
        terminal.screen().mouse_protocol_mode(),
        vt100::MouseProtocolMode::None
    );
    session.send_key(Key::Left).unwrap();
    session.send_key(Key::Left).unwrap();
    session.type_input("X").expect("inserted character renders");
    session
        .wait_for_text("abXcd", TIMEOUT)
        .expect("keyboard editing remains available");
}

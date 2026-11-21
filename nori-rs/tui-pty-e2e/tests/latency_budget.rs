//! The 100 ms rule, measured end to end. A session is driven through what people do over and
//! over (typing, a prompt and its answer, model and effort switches with the keyboard and with
//! `/model`, the resume picker, a new session) with `NORI_LATENCY_LOG` set, and the table Nori
//! writes on exit is read back: every path that was exercised is in it, and no handler of the
//! app's loop stalled the interface for anything near a second. The table is printed, so
//! `cargo test -p tui-pty-e2e --test latency_budget -- --nocapture` shows the numbers.
//!
//! Debug builds are several times slower than release, so the hard bound here is a stall of a
//! second, not the 100 ms budget itself; the budget is what the printed table is read against.

#![cfg(unix)]

use std::time::Duration;

use tui_pty_e2e::Key;
use tui_pty_e2e::SessionConfig;
use tui_pty_e2e::TIMEOUT;
use tui_pty_e2e::TuiSession;

/// A pause between keys, as a person types.
const KEY_GAP: Duration = Duration::from_millis(25);

/// Wait while reading what Nori draws, as a terminal does. A screen nobody reads fills the
/// pseudo-terminal's buffer, and then a draw blocks in its write until the next read: a stall of
/// the test's own making, not Nori's.
fn pause(session: &mut TuiSession, duration: Duration) {
    let until = std::time::Instant::now() + duration;
    while std::time::Instant::now() < until {
        session.poll().expect("read the screen");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn type_slowly(session: &mut TuiSession, text: &str) {
    for ch in text.chars() {
        session.send_key(Key::Char(ch)).expect("type");
        pause(session, KEY_GAP);
    }
}

#[test]
fn every_interaction_path_is_timed_and_none_stalls_the_interface() {
    let dir = tempfile::tempdir().expect("scratch directory");
    let log = dir.path().join("latency.txt");
    let config = SessionConfig::new().with_agent_env(
        "NORI_LATENCY_LOG",
        log.to_str().expect("utf-8 path").to_owned(),
    );
    let mut session = TuiSession::spawn_with_config(30, 120, config).expect("start Nori");
    session.wait_for_text("›", TIMEOUT).expect("composer");

    type_slowly(&mut session, "hello there");
    session.send_key(Key::Enter).expect("submit");
    session
        .wait_for_text("Test message 2", TIMEOUT)
        .expect("the answer");

    // Model and effort switches from the keyboard.
    for _ in 0..3 {
        session.send_key(Key::Ctrl('n')).expect("next model");
        pause(&mut session, KEY_GAP * 4);
    }
    session.send_key(Key::Ctrl('k')).expect("more effort");
    pause(&mut session, KEY_GAP * 4);

    // A model chosen in /model.
    type_slowly(&mut session, "/model");
    session.send_key(Key::Enter).expect("open /model");
    session
        .wait_for_text("Mock Fast Model", TIMEOUT)
        .expect("the model picker");
    session.send_key(Key::Down).expect("move");
    session.send_key(Key::Enter).expect("choose");
    pause(&mut session, KEY_GAP * 8);

    // The resume picker, opened and dismissed.
    type_slowly(&mut session, "/resume");
    session.send_key(Key::Enter).expect("open /resume");
    session
        .wait_for_text("this project", TIMEOUT)
        .expect("the resume picker");
    session.send_key(Key::Escape).expect("dismiss");
    pause(&mut session, KEY_GAP * 8);

    // A new session.
    type_slowly(&mut session, "/new");
    session.send_key(Key::Enter).expect("new session");
    pause(&mut session, Duration::from_secs(2));

    session.send_key(Key::Ctrl('d')).expect("quit");
    assert!(
        session.wait_for_process_exit(Duration::from_secs(10)),
        "Nori exits"
    );

    let table = std::fs::read_to_string(&log).expect("the latency table");
    println!("{table}");
    // Each stall over the budget, as it happened, with the handler that caused it.
    if let Some(home) = session.nori_home_path()
        && let Ok(tui_log) = std::fs::read_to_string(home.join("log").join("nori-tui.log"))
    {
        for line in tui_log
            .lines()
            .filter(|line| line.contains("100 ms budget"))
        {
            println!("{line}");
        }
    }
    for path in [
        "interaction: key → frame",
        "interaction: /command → frame",
        "interaction: model/effort/mode switch",
        "interaction: picker open",
        "terminal: key",
    ] {
        assert!(table.contains(path), "{path} was not timed:\n{table}");
    }
    // Columns: path words, count, p50, p95, max, >100ms. No handler may hold the loop a second.
    for row in table.lines().skip(1) {
        let fields: Vec<&str> = row.split_whitespace().collect();
        let Some(max) = fields
            .len()
            .checked_sub(2)
            .and_then(|index| fields.get(index))
            .and_then(|max| max.parse::<f64>().ok())
        else {
            continue;
        };
        if !row.starts_with("interaction:") {
            assert!(max < 1000.0, "a handler stalled the interface:\n{row}");
        }
    }
}

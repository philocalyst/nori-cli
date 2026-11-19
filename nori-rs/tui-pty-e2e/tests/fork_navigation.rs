#![cfg(unix)]

use pretty_assertions::assert_eq;
use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;
use tui_pty_e2e::Key;
use tui_pty_e2e::SessionConfig;
use tui_pty_e2e::TuiSession;

fn session_journals(path: &Path, pkl: &mut BTreeSet<String>, jsonl: &mut BTreeSet<String>) {
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            session_journals(&path, pkl, jsonl);
        } else if path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "sessions")
            && let (Some(stem), Some(ext)) = (path.file_stem(), path.extension())
        {
            if ext == "pkl" {
                pkl.insert(stem.to_string_lossy().into_owned());
            } else if ext == "jsonl" {
                jsonl.insert(stem.to_string_lossy().into_owned());
            }
        }
    }
}

#[test]
fn older_response_fork_keeps_drafts_and_nested_history_separate() {
    let config = SessionConfig::new()
        .with_mock_response("FIRST-RESPONSE-MARKER")
        .with_agent_env("MOCK_AGENT_RESPONSE_MATCH_TEXT", "FUTURE-QUESTION-MARKER")
        .with_agent_env("MOCK_AGENT_RESPONSE_MATCH", "FUTURE-RESPONSE-MARKER");
    let mut session = TuiSession::spawn_with_config(50, 150, config).expect("start TUI");
    let home = session.nori_home_path().expect("isolated home");
    let timeout = Duration::from_secs(30);
    session.wait_for_text("›", timeout).expect("composer");
    session
        .submit_input("FIRST-QUESTION-MARKER")
        .expect("first question");
    session
        .wait_for_text("FIRST-RESPONSE-MARKER", timeout)
        .expect("first response");
    session
        .submit_input("FUTURE-QUESTION-MARKER")
        .expect("second question");
    session
        .wait_for_text("FUTURE-RESPONSE-MARKER", timeout)
        .expect("second response");
    session
        .type_input("PARENT-DRAFT-MARKER")
        .expect("parent draft");
    session.send_str("\x1b[27u").expect("normal mode");
    session.wait_for_text("NORMAL", timeout).expect("normal");
    session.send_str("[").expect("focus latest answer");
    session
        .wait_for_text("Turn 2 of 2", timeout)
        .expect("latest turn");
    session.send_str("[").expect("focus older answer");
    session
        .wait_for_text("Turn 1 of 2", timeout)
        .expect("older turn");
    session.send_str(" ").expect("fork older answer");
    session
        .wait_for_text("History fork", timeout)
        .expect("fork created");
    session.send_str("\x1b[9;5u").expect("Ctrl+Tab to child");
    session
        .wait_for(
            |screen| {
                screen.contains("FIRST-QUESTION-MARKER")
                    && screen.contains("FIRST-RESPONSE-MARKER")
                    && !screen.contains("PARENT-DRAFT-MARKER")
            },
            timeout,
        )
        .expect("child shows selected prefix");
    let child_screen = session.screen_contents();
    assert!(
        !child_screen.contains("FUTURE-QUESTION-MARKER"),
        "fork included future prompt: {child_screen}"
    );
    assert!(
        !child_screen.contains("FUTURE-RESPONSE-MARKER"),
        "fork included future answer: {child_screen}"
    );
    assert_eq!(
        child_screen.matches("FIRST-QUESTION-MARKER").count(),
        1,
        "duplicate selected history"
    );
    session
        .submit_input("CHILD-FOLLOWUP-MARKER")
        .expect("send child follow-up");
    session
        .wait_for_text("CHILD-FOLLOWUP-MARKER", timeout)
        .expect("child follow-up visible");
    session
        .wait_for(
            |screen| {
                screen.contains("CHILD-FOLLOWUP-MARKER")
                    && screen.matches("FIRST-RESPONSE-MARKER").count() >= 3
            },
            timeout,
        )
        .expect("child answer");
    session
        .type_input("CHILD-DRAFT-MARKER")
        .expect("child draft");
    session.send_str("\x1b[27u").expect("normal mode");
    session.wait_for_text("NORMAL", timeout).expect("normal");
    session.send_str("[").expect("focus child answer");
    session.wait_for_text("↶ Turn", timeout).expect("focused");
    session.send_str(" ").expect("nested fork");
    session
        .wait_for(
            |_| {
                let mut pkl = BTreeSet::new();
                let mut jsonl = BTreeSet::new();
                session_journals(&home, &mut pkl, &mut jsonl);
                pkl.len() >= 3
            },
            timeout,
        )
        .expect("nested fork journal created");
    // The most-recently-used cycle can visit main before the newly created nested view.
    let mut found_nested = false;
    for _ in 0..4 {
        session.send_str("\x1b[9;5u").expect("cycle sessions");
        let matched = session
            .wait_for(
                |screen| {
                    screen.contains("CHILD-FOLLOWUP-MARKER")
                        && screen
                            .matches("Continue from the conversation above")
                            .count()
                            >= 2
                        && !screen.contains("CHILD-DRAFT-MARKER")
                        && !screen.contains("PARENT-DRAFT-MARKER")
                },
                Duration::from_millis(800),
            )
            .is_ok();
        if matched {
            found_nested = true;
            break;
        }
    }
    assert!(
        found_nested,
        "nested fork was not reachable through Ctrl+Tab"
    );
    session.send_key(Key::Tab).expect("return to child parent");
    session
        .wait_for_text("CHILD-DRAFT-MARKER", timeout)
        .expect("child draft restored");
    session.send_key(Key::Tab).expect("return to main");
    session
        .wait_for_text("PARENT-DRAFT-MARKER", timeout)
        .expect("parent draft restored");
    session
        .wait_for(
            |screen| screen.contains("PARENT-DRAFT-MARKER") && !screen.contains("Working"),
            timeout,
        )
        .expect("finished forks leave no phantom work");
    session
        .wait_for(
            |_| {
                let mut pkl = BTreeSet::new();
                let mut jsonl = BTreeSet::new();
                session_journals(&home, &mut pkl, &mut jsonl);
                pkl.len() == 3 && pkl == jsonl
            },
            timeout,
        )
        .expect("one canonical journal pair for main and both forks");
    session.send_key(Key::Ctrl('d')).expect("quit");
    assert!(
        session.wait_for_process_exit(Duration::from_secs(15)),
        "TUI did not exit"
    );
}

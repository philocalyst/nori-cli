//! What a person sees when an agent in a fault world fails: words, never a provider's JSON.
//!
//! Drives `nori --world` on a pty against `fake-agent/worlds/claude-coordination.pkl` (the same
//! faults `nori-world`'s fault suite asserts on the event stream; see docs/fault-catalog.md).
#![cfg(unix)]

use std::time::Duration;

use tui_pty_e2e::SessionConfig;
use tui_pty_e2e::TIMEOUT;
use tui_pty_e2e::TuiSession;

const NO_RETRIES: &str = "recovery = new Dynamic { retry = new Dynamic { max_attempts = 0 } }";

fn world_session(agent: &str) -> TuiSession {
    let world = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fake-agent/worlds/claude-coordination.pkl")
        .canonicalize()
        .expect("the claude-coordination world");
    let config = SessionConfig::new()
        .with_agent(agent.to_owned())
        .with_arg("--world")
        .with_arg(world.display().to_string())
        .with_agent_env("NORI_WORLD_CLOCK", "virtual")
        .with_policy_pkl(NO_RETRIES);
    let mut session = TuiSession::spawn_with_config(34, 110, config).unwrap();
    session
        .wait_for_text("›", Duration::from_secs(30))
        .expect("the TUI did not start");
    session
}

/// FAULT: provider-prose. The provider's sentence reaches the screen.
#[test]
fn a_provider_refusal_is_shown_in_the_providers_words() {
    let mut session = world_session("fake-claude");
    session.submit_input("credit").unwrap();
    session
        .wait_for_text("credit balance is too low", TIMEOUT * 3)
        .unwrap_or_else(|error| panic!("{error}\n{}", session.screen_contents()));
}

/// FAULT: nested-json-error. A JSON error body inside an internal error is not printed raw.
#[test]
fn a_nested_json_error_body_is_not_printed_raw() {
    let mut session = world_session("fake-claude");
    session.submit_input("overloaded").unwrap();
    // Wait for the turn to be over: the prompt is ready again.
    std::thread::sleep(Duration::from_secs(3));
    session
        .wait_for_text("›", TIMEOUT * 3)
        .unwrap_or_else(|error| panic!("{error}\n{}", session.screen_contents()));
    let screen = session.screen_contents();
    for raw in ["\"type\":\"error\"", "overloaded_error", "request_id"] {
        assert!(
            !screen.contains(raw),
            "raw provider JSON on screen ({raw}):\n{screen}"
        );
    }
}

/// FAULT: oversize-paste. A prompt over the provider's context budget is refused in words.
#[test]
fn an_oversize_prompt_is_refused_in_words() {
    let world = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fake-agent/worlds/oversize-context.pkl")
        .canonicalize()
        .expect("the oversize-context world");
    let config = SessionConfig::new()
        .with_agent("big-tiny".to_owned())
        .with_arg("--world")
        .with_arg(world.display().to_string())
        .with_agent_env("NORI_WORLD_CLOCK", "virtual");
    let mut session = TuiSession::spawn_with_config(34, 110, config).unwrap();
    session
        .wait_for_text("›", Duration::from_secs(30))
        .expect("the TUI did not start");
    session
        .submit_input(&"a long pasted log. ".repeat(300))
        .unwrap();
    std::thread::sleep(Duration::from_secs(4));
    let screen = session.screen_contents();
    for raw in ["\"type\":\"error\"", "invalid_request_error"] {
        assert!(
            !screen.contains(raw),
            "raw provider JSON on screen ({raw}):\n{screen}"
        );
    }
}

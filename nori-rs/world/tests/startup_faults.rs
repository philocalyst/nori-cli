//! Agents that never answer while starting. The harness's own time runs here, so the clock is
//! paused: a connect timeout of any length passes at once, and a session that waits forever
//! shows as a failed wait instead of a wait.
//!
//! World: `fake-agent/worlds/claude-coordination.pkl`.

mod support;

use std::time::Duration;

use support::is_raw_provider_json;

const WORLD: &str = "claude-coordination.pkl";

async fn start_and_read(agent: &str) -> (nori_world::WorldSession, Vec<String>) {
    let mut options = support::options(agent);
    options.allow_failed_start = true;
    options.timeout = Duration::from_secs(30);
    let session = nori_world::WorldSession::boot(support::world(WORLD), options)
        .await
        .unwrap_or_else(|error| {
            panic!("{agent} never answers and the session start hangs on it: {error:#}")
        });
    let said = support::words(&session.events());
    (session, said)
}

/// FAULT: initialize-never. The CLI started and stalled: `initialize` is never answered.
#[tokio::test(start_paused = true)]
async fn an_agent_that_never_answers_initialize_fails_the_start_in_words() {
    let (_session, said) = start_and_read("fake-claude-mute").await;
    assert!(!said.is_empty(), "the start failed and nothing said why");
    assert!(
        said.iter().all(|text| !is_raw_provider_json(text)),
        "{said:?}"
    );
}

/// FAULT: new-session-never. `session/new` is never answered.
#[tokio::test(start_paused = true)]
async fn an_agent_that_never_answers_session_new_fails_the_start_in_words() {
    let (_session, said) = start_and_read("fake-claude-stalled").await;
    assert!(!said.is_empty(), "the start failed and nothing said why");
    assert!(
        said.iter().all(|text| !is_raw_provider_json(text)),
        "{said:?}"
    );
}

//! A provider that refuses what is too big, against everything Nori builds a big prompt from.
//!
//! World: `fake-agent/worlds/oversize-context.pkl`. `big-codex` refuses any text block over
//! OpenAI's 10 MiB with the Responses API's own body (`string_above_max_length`), which is how
//! Codex children failed when Nori forked a long history, or handed it to a fallback route, as
//! one unbounded block. Three `big` turns of four megabytes each make a twelve-megabyte history.

mod support;

use nori_world::Stuck;
use pretty_assertions::assert_eq;
use support::boot;
use support::failure_messages;
use support::is_raw_provider_json;
use support::largest_block;
use support::refusals;

const WORLD: &str = "oversize-context.pkl";
/// OpenAI's limit for one text block, in bytes.
const BLOCK_LIMIT: i64 = 10_485_760;

async fn grow_history(session: &nori_world::WorldSession) {
    for turn in 1..=3 {
        let events = session
            .ask("big")
            .await
            .unwrap_or_else(|error| panic!("big turn {turn}: {error:#}"));
        assert!(
            events.root().text().contains("Line 40000:"),
            "turn {turn} did not stream the whole answer"
        );
    }
}

/// FAULT: oversize-fork. Forking a long history into a child sends one text block over the
/// provider's limit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forking_a_long_history_sends_no_block_over_the_provider_limit() {
    let session = boot(WORLD, "big-codex").await;
    grow_history(&session).await;
    session.fork_history().await.expect("fork");
    // A fork is not announced as a subagent, so the recorder is what says it ran.
    let ran = support::prompt_reached(
        &session,
        "big-codex",
        "Continue from the conversation above.",
    )
    .await;
    let recorder = session.recorder();
    println!(
        "largest block the fork handed the provider: {} bytes (limit {BLOCK_LIMIT})",
        largest_block(&recorder, "big-codex")
    );
    assert_eq!(refusals(&recorder, "big-codex"), Vec::<String>::new());
    assert!(
        largest_block(&recorder, "big-codex") <= BLOCK_LIMIT,
        "a block of {} bytes went to the provider; the limit is {BLOCK_LIMIT}",
        largest_block(&recorder, "big-codex")
    );
    assert!(
        ran,
        "the fork never reached the agent; failures: {:?}",
        failure_messages(&session.events())
    );
}

/// FAULT: oversize-checkpoint. A fallback route is handed the whole conversation as a
/// checkpoint, in one block.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fallback_checkpoint_sends_no_block_over_the_provider_limit() {
    let session = boot(WORLD, "big-lead").await;
    grow_history(&session).await;
    let events = session
        .ask("quota please")
        .await
        .expect("the fallback turn");

    let recorder = session.recorder();
    assert_eq!(refusals(&recorder, "big-codex"), Vec::<String>::new());
    assert!(largest_block(&recorder, "big-codex") <= BLOCK_LIMIT);
    assert_eq!(
        events.route_changes().len(),
        1,
        "{:?}",
        events.route_changes()
    );
    assert!(
        events.root().text().contains("answered:"),
        "the fallback route never answered: failures {:?}",
        failure_messages(&events)
    );
}

/// FAULT: refusal-prose. A prompt over the provider's context budget is refused with the
/// provider's JSON body; the person is told in words, and the session carries on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_context_refusal_reaches_the_person_as_prose_and_the_session_recovers() {
    let session = boot(WORLD, "big-tiny").await;
    let events = session
        .ask(&"a long pasted log. ".repeat(300))
        .await
        .expect("the refused turn ends");

    let messages = failure_messages(&events);
    assert!(!messages.is_empty(), "the refusal was not reported at all");
    for message in &messages {
        assert!(
            !is_raw_provider_json(message),
            "the person was shown the provider's JSON: {message}"
        );
    }
    assert_eq!(refusals(&session.recorder(), "big-tiny").len(), 1);

    let events = session.ask("hello").await.expect("the next turn");
    assert_eq!(events.root().text(), "answered: hello");
}

/// FAULT: oversize-paste. A person pastes eleven megabytes into one prompt: either Nori keeps
/// every block under the limit, or the refusal is said in words.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_oversize_paste_never_shows_the_providers_json() {
    let session = boot(WORLD, "big-codex").await;
    let events = match session.ask(&"y".repeat(11_000_000)).await {
        Ok(events) => events,
        Err(error) => match error.downcast_ref::<Stuck>() {
            Some(stuck) => panic!("the session hung on an oversize paste: {stuck}"),
            None => panic!("{error:#}"),
        },
    };
    for message in failure_messages(&events) {
        assert!(
            !is_raw_provider_json(&message),
            "the person was shown the provider's JSON: {message}"
        );
    }
    let events = session.ask("hello").await.expect("the next turn");
    assert_eq!(events.root().text(), "answered: hello");
}

//! Automatic compaction and loop detection, end to end: the real harness against agents that
//! report their context filling, and an agent that goes in circles.
//!
//! World: `fake-agent/worlds/context-pressure.pkl`. A 200000-token window triggers compaction below
//! 30000 free tokens (usage over 170000) and prepares a summary below 42000 free (usage over
//! 158000); the policy is the built-in one (`docs/workflows.md`, `compaction` and `loops`).
//!
//! The recorder keeps a prompt's text without Nori's own context, so the checkpoint a session opens
//! with shows in `bytes` (what the prompt weighed on the wire): a prompt that is a few characters
//! of the person's words and thousands of bytes is one that opened with the checkpoint.

mod support;

use nori_world::Entry;
use nori_world::Record;
use nori_world::WorldSession;
use pretty_assertions::assert_eq;
use support::boot;
use support::option_value;
use support::words;

const WORLD: &str = "context-pressure.pkl";

/// What the summary scene of the world writes, so a checkpoint's size can be told from a short
/// prompt's.
const SUMMARY_BYTES: i64 = 300;

/// One prompt an agent received.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Received {
    session: String,
    text: String,
    scene: Option<String>,
    bytes: i64,
}

fn received(recorder: &[Entry], agent: &str) -> Vec<Received> {
    recorder
        .iter()
        .filter_map(|entry| match &entry.record {
            Record::PromptReceived {
                agent: who,
                session,
                text,
                scene,
                bytes,
                ..
            } if who == agent => Some(Received {
                session: session.clone(),
                text: text.clone(),
                scene: scene.clone(),
                bytes: *bytes,
            }),
            _ => None,
        })
        .collect()
}

/// What the agent was asked, in order, by the scene that answered.
fn scenes(session: &WorldSession, agent: &str) -> Vec<String> {
    received(&session.recorder(), agent)
        .into_iter()
        .map(|prompt| prompt.scene.unwrap_or_else(|| "(unscripted)".to_owned()))
        .collect()
}

fn sessions_opened(session: &WorldSession, agent: &str) -> usize {
    session
        .recorder()
        .iter()
        .filter(|entry| matches!(&entry.record, Record::SessionOpened { agent: who, .. } if who == agent))
        .count()
}

fn said(session: &WorldSession, fragment: &str) -> usize {
    words(&session.events())
        .iter()
        .filter(|message| message.contains(fragment))
        .count()
}

/// The prompt after `text` opened with a checkpoint on a new session.
fn assert_opens_with_checkpoint(session: &WorldSession, agent: &str, text: &str) {
    let prompts = received(&session.recorder(), agent);
    let first = prompts.first().expect("the agent was asked something");
    let last = prompts.last().expect("the agent was asked something");
    assert_eq!(last.text, text);
    assert!(
        last.session != first.session,
        "{text:?} went to the session the conversation was compacted away from"
    );
    assert!(
        last.bytes >= i64::try_from(text.len()).unwrap_or(0) + SUMMARY_BYTES,
        "{text:?} weighed {} bytes: it did not open with the checkpoint",
        last.bytes
    );
}

/// Automatic compaction: usage over the trigger at the end of turn two is compacted while the
/// session rests, and the third prompt opens with the checkpoint on a new session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_context_over_the_trigger_is_compacted_before_the_next_prompt() {
    let session = boot(WORLD, "squeezed").await;
    session.ask("use 60k").await.expect("turn one");
    assert_eq!(scenes(&session, "squeezed"), vec!["use 60k"]);
    assert_eq!(sessions_opened(&session, "squeezed"), 1);

    session.ask("use 180k").await.expect("turn two");

    assert_eq!(
        scenes(&session, "squeezed"),
        vec!["use 60k", "use 180k", "summary"],
        "the agent was not asked for a summary while the session rested"
    );
    assert_eq!(said(&session, "compacting before the next prompt"), 1);
    assert_eq!(sessions_opened(&session, "squeezed"), 2);

    let events = session.ask("hello").await.expect("turn three");
    assert_eq!(events.root().text(), "answered: hello");
    assert_opens_with_checkpoint(&session, "squeezed", "hello");
    assert_eq!(
        scenes(&session, "squeezed"),
        vec!["use 60k", "use 180k", "summary", "answer"],
        "the context was compacted a second time"
    );
}

/// A summary prepared while the context was near the trigger is the one the swap uses, and the
/// agent is not asked again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_summary_prepared_ahead_of_need_is_the_one_the_swap_uses() {
    let session = boot(WORLD, "squeezed").await;
    session.ask("use 60k").await.expect("turn one");
    session.ask("use 165k").await.expect("turn two");

    assert_eq!(
        scenes(&session, "squeezed"),
        vec!["use 60k", "use 165k", "summary"],
        "the summary was not prepared while the context was close to the trigger"
    );
    assert_eq!(said(&session, "preparing a summary"), 1);
    assert_eq!(
        sessions_opened(&session, "squeezed"),
        1,
        "preparing a summary swapped the session"
    );

    session.ask("use 175k").await.expect("turn three");

    assert_eq!(
        scenes(&session, "squeezed"),
        vec!["use 60k", "use 165k", "summary", "use 175k"],
        "the agent was asked for a second summary"
    );
    assert_eq!(
        said(&session, "switching to the summary prepared earlier"),
        1
    );
    assert_eq!(said(&session, "compacting before the next prompt"), 0);
    assert_eq!(sessions_opened(&session, "squeezed"), 2);

    session.ask("hello").await.expect("turn four");
    assert_opens_with_checkpoint(&session, "squeezed", "hello");
}

/// FAULT: context-overflow. An agent that never writes the template fails the compaction; the
/// person is told once, and no later prompt is held up by another attempt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_automatic_compaction_is_said_once_and_not_tried_again() {
    let session = boot(WORLD, "stubborn").await;
    session.ask("use 180k").await.expect("turn one");

    assert_eq!(
        scenes(&session, "stubborn"),
        vec!["use 180k", "summary-in-prose", "summary-in-prose"],
        "one summary request and one reminder"
    );
    assert_eq!(said(&session, "Compaction failed"), 1);
    assert_eq!(said(&session, "Automatic compaction is off"), 1);
    assert_eq!(sessions_opened(&session, "stubborn"), 1);

    session.ask("hello").await.expect("the next turn");
    session.ask("use 180k").await.expect("the turn after");
    let events = session.ask("hello again").await.expect("the last turn");

    assert_eq!(events.root().text(), "answered: hello again");
    assert_eq!(
        scenes(&session, "stubborn"),
        vec![
            "use 180k",
            "summary-in-prose",
            "summary-in-prose",
            "answer",
            "use 180k",
            "answer"
        ],
        "automatic compaction tried again after it was backed off"
    );
    assert_eq!(said(&session, "Compaction failed"), 1);
    assert_eq!(said(&session, "Automatic compaction is off"), 1);
    assert_eq!(sessions_opened(&session, "stubborn"), 1);
}

/// An agent that offers `/compact` is asked to compact itself, in the session it has.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_with_its_own_compact_command_is_sent_that_instead_of_a_summary_prompt() {
    let session = boot(WORLD, "native").await;
    session.ask("use 180k").await.expect("turn one");

    assert_eq!(
        scenes(&session, "native"),
        vec!["use 180k", "native-compact"]
    );
    let prompts = received(&session.recorder(), "native");
    assert_eq!(prompts[1].text, "/compact");
    assert_eq!(said(&session, "compacting before the next prompt"), 1);
    assert_eq!(
        sessions_opened(&session, "native"),
        1,
        "a native compaction swapped the session"
    );

    let events = session.ask("hello").await.expect("the next turn");
    assert_eq!(events.root().text(), "answered: hello");
    assert_eq!(
        scenes(&session, "native"),
        vec!["use 180k", "native-compact", "answer"],
        "the agent was compacted again, or asked for a summary"
    );
}

/// The mode and the model the person chose are the ones the session has after the swap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_compaction_keeps_the_model_and_the_mode() {
    let session = boot(WORLD, "squeezed").await;
    let handle = session.handle();
    handle
        .set_session_config_option("model".into(), "fake-sonnet".into())
        .await
        .expect("the model");
    handle
        .set_session_config_option("mode".into(), "plan".into())
        .await
        .expect("the mode");
    let before = handle.get_session_config().await.expect("a snapshot");
    assert_eq!(
        option_value(&before, "model").as_deref(),
        Some("fake-sonnet")
    );
    assert_eq!(option_value(&before, "mode").as_deref(), Some("plan"));

    session.ask("use 180k").await.expect("turn one");
    assert_eq!(sessions_opened(&session, "squeezed"), 2, "no swap happened");

    let after = handle.get_session_config().await.expect("a snapshot");
    assert_eq!(
        option_value(&after, "model").as_deref(),
        Some("fake-sonnet")
    );
    assert_eq!(option_value(&after, "mode").as_deref(), Some("plan"));
    let events = session.ask("who").await.expect("the next turn");
    assert_eq!(
        events.root().text(),
        "model=fake-sonnet",
        "the new session runs another model"
    );
}

/// FAULT: doom-loop. The same call eight times in a turn steers the agent once; when it loops
/// again the turn is cancelled and the person is told why in one line.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_that_repeats_a_call_is_steered_once_and_then_stopped() {
    let session = boot(WORLD, "looper").await;
    session.ask("loop").await.expect("the looping turn ends");
    session
        .settle()
        .await
        .expect("the session settles after the second loop");

    let prompts = received(&session.recorder(), "looper");
    let nudges: Vec<&Received> = prompts
        .iter()
        .filter(|prompt| prompt.text.starts_with("You have called"))
        .collect();
    assert_eq!(
        nudges.len(),
        1,
        "steered {} times: {prompts:?}",
        nudges.len()
    );
    assert!(
        nudges[0].text.contains("\"Read src/lib.rs\"") && nudges[0].text.contains("same input"),
        "{}",
        nudges[0].text
    );
    assert_eq!(prompts.len(), 2, "the turn went on after it was stopped");
    assert_eq!(said(&session, "steering the agent"), 1);
    assert_eq!(said(&session, "stopped the turn"), 1);
    let stopped: Vec<String> = words(&session.events())
        .into_iter()
        .filter(|message| message.contains("stopped the turn"))
        .collect();
    assert_eq!(
        stopped,
        vec![
            "Loop: \"Read src/lib.rs\" called 6 times with the same input; stopped the turn."
                .to_owned()
        ]
    );

    let events = session.ask("hello").await.expect("the next turn");
    assert_eq!(events.root().text(), "answered: hello");
}

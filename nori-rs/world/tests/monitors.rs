//! Monitors through the real harness: what wakes the agent, what only tells the person, and what a
//! clock and the session itself fire.
//!
//! World: `fake-agent/worlds/monitors.pkl`. Monitors run on the harness's own (real) clock, so the
//! waits here are a few real seconds.

mod support;

use nori_protocol::MonitorWire;
use nori_protocol::NoriEvent;
use nori_world::Events;
use nori_world::Record;
use support::boot;

const WORLD: &str = "monitors.pkl";

/// The prompts the lead agent was handed that were monitor wakes.
fn wakes(session: &nori_world::WorldSession) -> Vec<String> {
    session
        .recorder()
        .iter()
        .filter_map(|entry| match &entry.record {
            Record::PromptReceived { agent, text, .. }
                if agent == "lead" && text.contains("<monitor_events") =>
            {
                Some(text.clone())
            }
            _ => None,
        })
        .collect()
}

fn notices(events: &Events) -> Vec<String> {
    events
        .nori()
        .filter_map(|event| match event {
            NoriEvent::Notice(notice) => Some(notice.message.clone()),
            _ => None,
        })
        .collect()
}

fn delivered(events: &Events) -> usize {
    events
        .nori()
        .filter(|event| matches!(event, NoriEvent::Monitor(MonitorWire::Delivered(_))))
        .count()
}

/// A command monitor's failure wakes the agent with the failing line and the exit code.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_build_wakes_the_agent_with_its_failure() {
    let session = boot(WORLD, "lead").await;
    session.ask("build it").await.expect("the arming turn");
    session
        .wait_for("the wake", |events| delivered(events) >= 1)
        .await
        .expect("the build's failure is delivered");
    session.settle().await.expect("the wake turn ends");
    let wakes = wakes(&session);
    assert!(
        wakes
            .iter()
            .any(|wake| wake.contains("FAILED error[E0308]") && wake.contains("exited 2")),
        "{wakes:#?}"
    );
}

/// A timer fires on its period, and ends quietly at its event budget: a clock running out is not a
/// failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clock_fires_and_wakes_the_agent() {
    let session = boot(WORLD, "lead").await;
    session.ask("tick").await.expect("the arming turn");
    session
        .wait_for("two firings", |_| {
            wakes(&session)
                .iter()
                .map(|wake| wake.matches("fired (").count())
                .sum::<usize>()
                >= 2
        })
        .await
        .expect("the clock fires twice");
    let wakes = wakes(&session);
    assert!(
        wakes.iter().all(|wake| !wake.contains("expired")),
        "{wakes:#?}"
    );
}

/// A `notify` action tells the person and never wakes the agent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_notify_action_tells_the_person_without_waking_the_agent() {
    let session = boot(WORLD, "lead").await;
    session.ask("tell me").await.expect("the arming turn");
    let events = session
        .wait_for("the notice", |events| {
            notices(events)
                .iter()
                .any(|notice| notice.contains("Build") && notice.contains("all green"))
        })
        .await
        .expect("the person is told");
    session.settle().await.expect("nothing else runs");
    assert_eq!(wakes(&session), Vec::<String>::new());
    assert_eq!(delivered(&events), 0);
}

/// A `session` monitor sees a request fail and wakes the agent with the words its `wake` action
/// gave, the event in place of `{{event}}`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_request_wakes_the_agent_through_a_session_monitor() {
    let session = boot(WORLD, "lead").await;
    session.ask("guard").await.expect("the arming turn");
    let _ = session.ask("break it").await;
    session
        .wait_for("the wake", |events| delivered(events) >= 1)
        .await
        .expect("the failure is delivered");
    let wakes = wakes(&session);
    assert!(
        wakes
            .iter()
            .any(|wake| wake.contains("A request failed; look into it:")
                && wake.contains("the tool server fell over")),
        "{wakes:#?}"
    );
}

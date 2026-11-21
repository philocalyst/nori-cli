//! The goal judge through the real harness: an agent calls `update_goal` with `complete`, and a
//! fresh reviewer (a second scripted agent that is given none of Nori's tools) decides.
//!
//! World: `fake-agent/worlds/goal-judge.pkl`. Its policy makes `judge` the reviewer, with three
//! seconds of patience and two disagreements allowed; the marker word in each goal's objective
//! picks the reviewer's behaviour. The lead agent claims the goal done on each continuation Nori
//! offers an active goal, so a test opens the goal and waits for the session to settle.

mod support;

use nori_world::Record;
use nori_world::WorldSession;
use pretty_assertions::assert_eq;
use serde_json::Value;
use support::boot;
use support::words;

const WORLD: &str = "goal-judge.pkl";

/// What each `update_goal` call returned, oldest first: its text as JSON when it is, and whether
/// it was an error.
fn updates(session: &WorldSession) -> Vec<(Value, bool)> {
    session
        .recorder()
        .iter()
        .filter_map(|entry| match &entry.record {
            Record::HostTool {
                name,
                output,
                is_error,
                ..
            } if name == "update_goal" => Some((
                serde_json::from_str(output).unwrap_or_else(|_| Value::String(output.clone())),
                *is_error,
            )),
            _ => None,
        })
        .collect()
}

/// The prompts the reviewer was handed, oldest first.
fn judge_prompts(session: &WorldSession) -> Vec<String> {
    session
        .recorder()
        .iter()
        .filter_map(|entry| match &entry.record {
            Record::PromptReceived { agent, text, .. } if agent == "judge" => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// The quiet lines the judge put in front of the person.
fn judge_lines(session: &WorldSession) -> Vec<String> {
    words(&session.events())
        .into_iter()
        .filter(|line| line.starts_with("Goal judge"))
        .collect()
}

fn status(update: &Value) -> &str {
    update["goal"]["status"].as_str().unwrap_or_default()
}

fn message(update: &Value) -> &str {
    update["message"].as_str().unwrap_or_default()
}

/// The reviewer says not yet, and the goal stays active with what is missing in the tool result;
/// then it says done, and the goal is complete and said to be verified.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_judge_that_says_not_yet_keeps_the_goal_active_until_it_says_done() {
    let session = boot(WORLD, "lead").await;
    session.ask("open twice").await.expect("the goal opens");
    session
        .settle()
        .await
        .expect("the claims are made and judged");

    let all = updates(&session);
    assert_eq!(all.len(), 2, "{all:#?}");
    let (refused, is_error) = &all[0];
    assert!(
        is_error,
        "a goal that is not done is not a success: {refused}"
    );
    assert_eq!(status(refused), "active", "{refused}");
    let told = message(refused);
    assert!(told.contains("not complete yet"), "{told}");
    assert!(
        told.contains("The record never shows the tests passing."),
        "{told}"
    );
    assert!(
        told.contains("Next step: Run cargo test and show the result."),
        "{told}"
    );
    assert!(told.contains("again only when"), "{told}");

    let (accepted, is_error) = &all[1];
    assert!(!is_error, "{accepted}");
    assert_eq!(status(accepted), "complete", "{accepted}");
    assert!(
        message(accepted).contains("verified it: The test run in the record passes."),
        "{accepted}"
    );

    // One quiet line per verdict, and nothing else about it.
    assert_eq!(
        judge_lines(&session),
        vec![
            "Goal judge: not yet (The record never shows the tests passing.)".to_owned(),
            "Goal judge: done (The test run in the record passes.)".to_owned(),
        ]
    );

    // A fresh reviewer for each claim, told the objective is data and shown the transcript.
    let prompts = judge_prompts(&session);
    assert_eq!(prompts.len(), 2, "{prompts:#?}");
    for prompt in &prompts {
        assert!(
            prompt.contains("<untrusted_objective>\nTWICE: trim tokens in the parser"),
            "{prompt}"
        );
        assert!(
            prompt.contains("create_goal"),
            "the transcript is in the prompt: {prompt}"
        );
    }
    // It was given none of Nori's tools: the only host tool calls are the lead's.
    let strangers: Vec<String> = session
        .recorder()
        .iter()
        .filter_map(|entry| match &entry.record {
            Record::HostTool { agent, name, .. } if agent != "lead" => {
                Some(format!("{agent}: {name}"))
            }
            _ => None,
        })
        .collect();
    assert_eq!(strangers, Vec::<String>::new());
    assert_eq!(session.divergences(), Vec::<String>::new());
}

/// Claims the reviewer keeps finding not done: after two, the third pauses the goal and says the
/// agent and the judge disagree, without asking the reviewer a third time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claims_the_judge_keeps_refusing_pause_the_goal_for_the_person() {
    let session = boot(WORLD, "lead").await;
    session.ask("open stubborn").await.expect("the goal opens");
    session
        .settle()
        .await
        .expect("the claims are made and judged");

    let all = updates(&session);
    assert_eq!(all.len(), 3, "{all:#?}");
    assert_eq!(
        all.iter()
            .map(|(update, _)| status(update))
            .collect::<Vec<_>>(),
        vec!["active", "active", "paused"]
    );
    assert!(all.iter().all(|(_, is_error)| *is_error), "{all:#?}");
    let told = message(&all[2].0);
    assert!(told.contains("you and the judge disagree"), "{told}");
    assert!(told.contains("the person decides"), "{told}");

    assert_eq!(
        judge_prompts(&session).len(),
        2,
        "the third claim is not reviewed"
    );
    let lines = words(&session.events());
    assert!(
        lines.iter().any(
            |line| line.contains("The agent and the goal judge disagree")
                && line.contains("paused for you to decide")
        ),
        "{lines:#?}"
    );
    assert_eq!(session.divergences(), Vec::<String>::new());
}

/// A reviewer that fails to run never holds the goal: the agent's word is accepted and marked
/// unverified, in the tool result and in one line.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_judge_that_fails_leaves_the_goal_complete_but_unverified() {
    let session = boot(WORLD, "lead").await;
    session.ask("open broken").await.expect("the goal opens");
    session
        .settle()
        .await
        .expect("the claim is made and judged");

    let all = updates(&session);
    assert_eq!(all.len(), 1, "{all:#?}");
    let (update, is_error) = &all[0];
    assert!(!is_error, "{update}");
    assert_eq!(status(update), "complete", "{update}");
    assert!(
        message(update).contains("not independently verified"),
        "{update}"
    );
    assert!(
        message(update).contains("the judge could not run"),
        "{update}"
    );
    let lines = judge_lines(&session);
    assert_eq!(lines.len(), 1, "{lines:#?}");
    assert!(
        lines[0].starts_with("Goal judge: unverified, accepting the agent's word ("),
        "{lines:#?}"
    );
    assert_eq!(session.divergences(), Vec::<String>::new());
}

/// A reviewer that never answers is given up on after the policy's `judge_timeout`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_judge_that_never_answers_is_given_up_on_and_the_goal_is_unverified() {
    let session = boot(WORLD, "lead").await;
    session.ask("open stall").await.expect("the goal opens");
    session
        .settle()
        .await
        .expect("the claim is made and given up on");

    let all = updates(&session);
    assert_eq!(all.len(), 1, "{all:#?}");
    let (update, is_error) = &all[0];
    assert!(!is_error, "{update}");
    assert_eq!(status(update), "complete", "{update}");
    assert!(
        message(update).contains("the judge did not answer within 3 seconds"),
        "{update}"
    );
    assert_eq!(judge_prompts(&session).len(), 1);
}

/// An answer that is not a verdict is not a reason to accept the goal: it counts as "not yet",
/// until the disagreements add up and the goal is paused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreadable_judge_answer_counts_as_not_yet() {
    let session = boot(WORLD, "lead").await;
    session.ask("open gibberish").await.expect("the goal opens");
    session
        .settle()
        .await
        .expect("the claims are made and judged");

    let all = updates(&session);
    assert_eq!(all.len(), 3, "{all:#?}");
    let (update, is_error) = &all[0];
    assert!(is_error, "{update}");
    assert_eq!(status(update), "active", "{update}");
    assert!(
        message(update).contains("the judge's answer could not be read"),
        "{update}"
    );
    assert_eq!(status(&all[2].0), "paused", "{all:#?}");
}

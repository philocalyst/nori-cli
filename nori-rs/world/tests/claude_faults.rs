//! The ways Claude Code's adapter and the harness fall out of step, as one fake agent.
//!
//! World: `fake-agent/worlds/claude-coordination.pkl`. Each test names the fault it reproduces
//! (see `docs/fault-catalog.md`) and asserts what Nori must do: no crash, no hang, a message a
//! person can read, and a session that is still usable afterwards.

mod support;

use nori_protocol::SessionPhase;
use nori_protocol::acp::v1 as acp;
use nori_world::SubagentInfo;
use pretty_assertions::assert_eq;
use support::boot;
use support::failure_messages;
use support::is_raw_provider_json;
use support::option_choices;
use support::option_value;

const WORLD: &str = "claude-coordination.pkl";

fn idle(events: &nori_world::Events) -> bool {
    matches!(events.phase(), Some(SessionPhase::Idle))
}

/// FAULT: unknown-update-kind. The CLI's stream carries message kinds the adapter has never
/// seen (`tool_progress` broke every turn of one session with a parse error).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn update_kinds_nobody_defined_do_not_break_the_turn() {
    let session = boot(WORLD, "fake-claude").await;
    let events = session.ask("unknown-kind").await.expect("the turn ends");
    assert_eq!(events.root().text(), "before. after.");
    assert_eq!(failure_messages(&events), Vec::<String>::new());
    assert!(idle(&events), "{:?}", events.phase());
    session.finish().await.unwrap().is_clean().unwrap();
}

/// FAULT: unknown-content-block. A known update carries a content block of a type from the
/// future.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_content_block_from_the_future_does_not_break_the_turn() {
    let session = boot(WORLD, "fake-claude").await;
    let events = session.ask("unknown-block").await.expect("the turn ends");
    assert!(
        events.root().text().ends_with("after."),
        "{:?}",
        events.root().text()
    );
    assert_eq!(failure_messages(&events), Vec::<String>::new());
    let events = session.ask("who").await.expect("the session is usable");
    assert_eq!(events.root().text(), "model=fake-sonnet");
}

/// FAULT: replace-turn. The adapter's turn policy replaces a running turn, and the old turn takes
/// two seconds to notice. The replacement must run, and the old turn must not leave a tool open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn steering_into_a_turn_that_is_slow_to_cancel_replaces_it() {
    // The turn has to still be running when the user steers: a virtual clock would play out the
    // sixty-second tool before the test could act, so time passes ten times faster than real.
    let mut options = support::options("fake-claude");
    options.clock = nori_fake_agent::ClockMode::Real(10.0);
    let session = nori_world::WorldSession::boot(support::world(WORLD), options)
        .await
        .unwrap();
    session.submit("long").await.unwrap();
    session
        .wait_for("the long turn to start", |events| {
            events.root().text().contains("working.")
        })
        .await
        .unwrap();
    session.steer("who").await.unwrap();
    let events = session.settle().await.expect("the session settles");
    assert!(
        events.root().text().contains("model=fake-sonnet"),
        "the steered prompt never ran: {:?}",
        events.root().text()
    );
    assert!(
        !events.root().text().contains("finished the long job"),
        "the replaced turn ran to its end"
    );
    session.finish().await.unwrap().is_clean().unwrap();
}

/// FAULT: permission-vs-cancel. The user cancels while a permission request is open. The agent
/// is told the question is cancelled, the turn ends, and the request is no longer offered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_while_a_permission_is_open_ends_the_turn_cleanly() {
    let mut options = support::options("fake-claude");
    options.clock = nori_fake_agent::ClockMode::Real(10.0);
    options.configure = Some(Box::new(|config| {
        let gates = config.nori_home.join("gates");
        std::fs::create_dir_all(&gates).expect("create gates directory");
        std::fs::write(
            gates.join("ask-edit.pkl"),
            r#"amends "nori:Gate"
name = "ask-edit"
on = "permission"
rules { new PermissionRule { kinds { "edit" }; decision = "ask" } }
"#,
        )
        .expect("write gate");
    }));
    let session = nori_world::WorldSession::boot(support::world(WORLD), options)
        .await
        .expect("boot fake-claude");
    session.submit("ask").await.unwrap();
    session
        .next_permission()
        .await
        .expect("the request arrives");
    session.cancel().await.unwrap();

    let mut told = false;
    for _ in 0..100 {
        told = session.recorder().iter().any(|entry| {
            matches!(
                &entry.record,
                nori_world::Record::PermissionAnswered {
                    answer: nori_fake_agent::Answer::Cancelled,
                    ..
                }
            )
        });
        if told {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(told, "the agent was never told the question was cancelled");
    let events = session
        .wait_for("the turn to end", idle)
        .await
        .expect("the cancelled turn ends");
    assert!(idle(&events), "{:?}", events.phase());
    assert!(
        session.pending_permissions().is_empty(),
        "the request is still offered to the user: {:?}",
        session.pending_permissions()
    );
    session.finish().await.unwrap().is_clean().unwrap();
}

/// FAULT: stream-eof. The CLI's stream ends in the middle of a tool call (the adapter logged
/// `SDK background reader: stream ended unexpectedly`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dying_cli_fails_the_turn_without_hanging_and_without_a_ghost_tool() {
    let session = boot(WORLD, "fake-claude").await;
    session.submit("eof").await.unwrap();
    let events = session
        .wait_for("the failure to be reported", |events| {
            !events.failures().is_empty() || idle(events)
        })
        .await
        .expect("a dead agent must end the turn, not hang it");
    for message in failure_messages(&events) {
        assert!(!is_raw_provider_json(&message), "{message}");
    }
    let violations = session.violations();
    assert!(
        violations
            .iter()
            .all(|violation| violation.invariant != "no_ghost_running_tools"),
        "{violations:?}"
    );
}

/// FAULT: provider-prose. The provider refuses with its own sentence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_providers_sentence_reaches_the_person() {
    let session = boot(WORLD, "fake-claude").await;
    let events = session.ask("credit").await.expect("the turn ends");
    let messages = failure_messages(&events);
    assert!(
        messages
            .iter()
            .any(|message| message.contains("credit balance is too low")),
        "{messages:?}"
    );
    let events = session.ask("who").await.expect("the session is usable");
    assert_eq!(events.root().text(), "model=fake-sonnet");
}

/// FAULT: nested-json-error. A JSON error body arrives as the `detail` of a JSON-RPC internal
/// error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_nested_json_error_body_is_never_shown_raw() {
    let session = boot(WORLD, "fake-claude").await;
    let events = session.ask("overloaded").await.expect("the turn ends");
    let messages = failure_messages(&events);
    assert!(!messages.is_empty(), "the failure was not reported");
    for message in &messages {
        assert!(!is_raw_provider_json(message), "{message}");
    }
}

/// FAULT: child-dies. A child session the agent reports fails mid-turn; the parent carries on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_child_does_not_fail_its_parent() {
    let session = boot(WORLD, "fake-claude").await;
    let events = session.ask("scout-dies").await.expect("the turn ends");
    assert!(
        events.root().text().contains("carrying on without it"),
        "{:?}",
        events.root().text()
    );
    assert_eq!(failure_messages(&events), Vec::<String>::new());
    let scouts: Vec<SubagentInfo> = events.subagents();
    assert_eq!(scouts.len(), 1, "{scouts:?}");
    assert!(
        matches!(
            scouts[0].state,
            Some(nori_protocol::SubagentState::Ended(
                nori_protocol::SubagentOutcome::Failed { .. }
            ))
        ),
        "{scouts:?}"
    );
}

/// FAULT: model-switch-lies. Three ways a model switch goes wrong: acknowledged and ignored,
/// acknowledged and substituted, refused. What Nori reports as the model must be the model that
/// answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_model_nori_reports_is_the_model_that_answers() {
    let session = boot(WORLD, "fake-claude").await;
    let handle = session.handle();

    // Acknowledged, and the agent keeps running the old model.
    let after = handle
        .set_session_config_option("model".into(), "fake-opus".into())
        .await
        .expect("the agent said yes");
    let reported = option_value(&after, "model");
    let answering = session.ask("who").await.unwrap().root().text();
    assert_eq!(
        format!("model={}", reported.as_deref().unwrap_or("?")),
        answering,
        "Nori says the model is {reported:?}; the agent answers as {answering:?}"
    );
    let snapshot = handle
        .get_session_config()
        .await
        .expect("a config snapshot");
    assert_eq!(
        option_value(&snapshot, "model").map(|model| format!("model={model}")),
        Some(answering),
        "the session's own config disagrees with the agent"
    );

    // Acknowledged, and the agent picks a different one.
    let after = handle
        .set_session_config_option("model".into(), "fake-fable".into())
        .await
        .expect("the agent said yes");
    assert_eq!(option_value(&after, "model").as_deref(), Some("fake-opus"));
    let answering = session.ask("who").await.unwrap().root().text();
    assert_eq!(answering, "model=fake-opus");

    // Refused: the person reads why, and the model stays.
    let refused = handle
        .set_session_config_option("model".into(), "fake-haiku".into())
        .await
        .expect_err("the agent refused");
    let message = format!("{refused:#}");
    assert!(!is_raw_provider_json(&message), "{message}");
    assert!(message.contains("Could not reach"), "{message}");
    let answering = session.ask("who").await.unwrap().root().text();
    assert_eq!(answering, "model=fake-opus");
}

/// FAULT: models-arrive-late. `session/new` lists no models; the CLI reports them later.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn models_announced_after_session_new_reach_the_session() {
    // Real time, so the first look is before the announcement.
    let mut options = support::options("fake-claude-late");
    options.clock = nori_fake_agent::ClockMode::Real(1.0);
    let session = nori_world::WorldSession::boot(support::world(WORLD), options)
        .await
        .unwrap();
    let handle = session.handle();
    let early = handle.get_session_config().await.unwrap_or_default();
    assert_eq!(
        option_choices(&early, "model"),
        0,
        "models were offered at session/new"
    );

    let mut offered = 0;
    for _ in 0..200 {
        let config: Vec<acp::SessionConfigOption> =
            handle.get_session_config().await.unwrap_or_default();
        offered = option_choices(&config, "model");
        if offered > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(offered, 4, "the announced models never reached the session");
}

/// FAULT: cli-missing. The adapter's CLI is not installed: the person is told what to install.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_cli_is_reported_as_not_installed() {
    let mut options = support::options("fake-claude-missing");
    options.allow_failed_start = true;
    let session = nori_world::WorldSession::boot(support::world(WORLD), options)
        .await
        .unwrap();
    session
        .wait_for("the start to fail", |events| {
            !events.failures().is_empty() || events.root_session().is_some()
        })
        .await
        .ok();
    let said = support::words(&session.events());
    assert!(
        said.iter().any(|text| text.contains("CLAUDE_CLI_PATH")
            || text.to_lowercase().contains("not found")
            || text.to_lowercase().contains("install")),
        "nothing told the person the CLI is missing: {said:?}"
    );
    assert!(
        said.iter().all(|text| !is_raw_provider_json(text)),
        "{said:?}"
    );
}

/// FAULT: sessions-locked. `session/load` and `session/resume` fail (the session is held by
/// another process). Nori does not hang, and the person ends up in a session that works.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_that_cannot_be_reopened_leaves_a_usable_session() {
    let session = boot(WORLD, "fake-claude-locked").await;
    let events = session.ask("remember plum").await.unwrap();
    assert_eq!(events.root().text(), "noted: remember plum");

    session.resume().await.unwrap();
    session
        .wait_for("a session after the failed reopen", |events| {
            events
                .nori()
                .filter(|event| matches!(event, nori_protocol::NoriEvent::SessionStarted(_)))
                .count()
                >= 2
        })
        .await
        .expect("after a failed reopen Nori must start a session, not hang");
    let events = session.ask("anything").await.unwrap();
    assert!(
        events.root().text().contains("answered: anything"),
        "{:?}; failures {:?}",
        events.root().text(),
        failure_messages(&events)
    );
}

/// FAULT: list-unreachable. `session/list` cannot reach the service behind the agent (OpenCode's
/// `/api/info` was down). Nori lists sessions while it prepares a connection; the list is only a
/// catalog for resuming, so its failure must not stop a new session from starting.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_whose_session_list_is_unreachable_still_starts_a_session() {
    let session = boot(WORLD, "fake-claude-nolist").await;
    let events = session
        .ask("anything")
        .await
        .expect("a failed session/list must not abort the start");
    assert_eq!(events.root().text(), "unreachable");
    assert_eq!(failure_messages(&events), Vec::<String>::new());
    session.finish().await.unwrap().is_clean().unwrap();
}

/// FAULT: close-unsupported. Nori sent `session/close` to agents that never offered it (catalog
/// probes did, for every agent they probed), and logged the agent's "method not found". An agent
/// without the capability is not sent one: closing is refused in words, and the session goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_without_session_close_is_never_sent_one() {
    let session = boot(WORLD, "fake-claude-noclose").await;
    let refused = session
        .handle()
        .close_session()
        .await
        .expect_err("closing is refused");
    let message = format!("{refused:#}");
    assert!(message.contains("does not support closing"), "{message}");
    let events = session.ask("anything").await.expect("the session goes on");
    assert_eq!(events.root().text(), "still here");
}

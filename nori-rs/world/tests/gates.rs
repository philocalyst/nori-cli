//! Gates (`nori:Gate`) through the real harness: a module in `$NORI_HOME/gates/` decides an
//! agent's permission request or tool call before it goes through, and carries a turn on that
//! the agent ended, with every decision journaled and one quiet line for the person.
//!
//! World: `fake-agent/worlds/gates.pkl`. The gate modules are written into the session's home
//! before it boots (`WorldOptions::configure`); two of them are the shipped examples.

mod support;

use nori_fake_agent::Answer;
use nori_protocol::NoriEvent;
use nori_world::Record;
use nori_world::WorldSession;
use pretty_assertions::assert_eq;

const WORLD: &str = "gates.pkl";

const NO_FORCE_PUSH: &str = include_str!("../../pkl/examples/gates/no-force-push.pkl");
const TESTS_BEFORE_DONE: &str = include_str!("../../pkl/examples/gates/tests-before-done.pkl");

const BASH_GATE: &str = r#"amends "nori:Gate"

name = "echo-dry-run"
description = "Refuse the unthinkable, rewrite one command"
on = "host_tool"
rules {
  new HostToolRule {
    tools { "bash" }
    arguments { ["command"] = "^rm -rf /" }
    decision = "deny"
    reason = "Never run that. Delete only what you created."
  }
  new HostToolRule {
    tools { "bash" }
    arguments { ["command"] = "^echo pushing" }
    decision = "rewrite"
    append { ["command"] = "&& echo dry-run" }
  }
}
"#;

async fn boot() -> WorldSession {
    let mut options = support::options("lead");
    options.configure = Some(Box::new(|config: &mut nori_config::NoriConfig| {
        let directory = config.nori_home.join("gates");
        std::fs::create_dir_all(&directory).expect("a gates directory");
        for (name, source) in [
            ("no-force-push.pkl", NO_FORCE_PUSH),
            ("tests-before-done.pkl", TESTS_BEFORE_DONE),
            ("echo-dry-run.pkl", BASH_GATE),
        ] {
            std::fs::write(directory.join(name), source).expect("a gate");
        }
    }));
    WorldSession::boot(support::world(WORLD), options)
        .await
        .unwrap_or_else(|error| panic!("boot: {error:#}"))
}

fn notices(session: &WorldSession) -> Vec<String> {
    session
        .events()
        .nori()
        .filter_map(|event| match event {
            NoriEvent::Notice(notice) => Some(notice.message.clone()),
            _ => None,
        })
        .collect()
}

fn prompts(session: &WorldSession) -> Vec<String> {
    session
        .recorder()
        .iter()
        .filter_map(|entry| match &entry.record {
            Record::PromptReceived { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// The `gate` records of the conversation journal, as written (JSON, one per decision).
async fn journaled_decisions(session: &WorldSession) -> Vec<serde_json::Value> {
    session
        .handle()
        .flush_transcript()
        .await
        .expect("the journal is written");
    let mut decisions = Vec::new();
    let mut directories = vec![session.nori_home().to_owned()];
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(&directory)
            .expect("a directory")
            .flatten()
        {
            let path = entry.path();
            if path.is_dir() {
                directories.push(path);
            } else if path.extension().is_some_and(|extension| extension == "pkl")
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                // `event_N = new { kind = "command"; data = "<the command as a JSON string>" }`
                for line in text.lines() {
                    let Some(literal) = line
                        .split_once("kind = \"command\"; data = ")
                        .and_then(|(_, rest)| rest.strip_suffix(" }"))
                    else {
                        continue;
                    };
                    let command: serde_json::Value = serde_json::from_str(
                        &serde_json::from_str::<String>(literal).expect("a string literal"),
                    )
                    .expect("a command");
                    if command["name"] == "gate" {
                        let mut payload: serde_json::Value =
                            serde_json::from_str(command["payload"].as_str().expect("a payload"))
                                .expect("a gate record");
                        if let Some(record) = payload.as_object_mut() {
                            record.remove("at");
                        }
                        decisions.push(payload);
                    }
                }
            }
        }
    }
    decisions
}

/// A force-push is refused before the person is asked; the agent is answered "denied".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_force_push_is_refused_without_asking_the_person() {
    let session = boot().await;

    let events = session.ask("push").await.expect("the turn");

    assert_eq!(
        session.pending_permissions(),
        vec![],
        "the person was never asked"
    );
    let answers: Vec<Answer> = session
        .recorder()
        .iter()
        .filter_map(|entry| match &entry.record {
            Record::PermissionAnswered { answer, .. } => Some(*answer),
            _ => None,
        })
        .collect();
    assert_eq!(answers, vec![Answer::Deny]);
    assert_eq!(events.root().text(), "The push was refused.");
    assert!(
        notices(&session)
            .iter()
            .any(|line| line == "gate no-force-push: denied git push --force origin main"),
        "{:?}",
        notices(&session)
    );
    assert_eq!(
        journaled_decisions(&session).await,
        vec![serde_json::json!({
            "gate": "no-force-push",
            "rule": 0,
            "event": "permission",
            "decision": "deny",
            "subject": "git push --force origin main",
            "detail": "Force-pushing is not allowed here. Push without --force, or tell the person why it is needed.",
        })]
    );
    assert_eq!(session.divergences(), Vec::<String>::new());
}

/// A host `bash` call is rewritten, and the result says so; another is refused with the reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bash_call_is_rewritten_and_another_refused() {
    let session = boot().await;

    let events = session.ask("bash").await.expect("the rewritten call");
    assert_eq!(events.root().text(), "The command ran.");
    let calls: Vec<(String, String, bool)> = session
        .recorder()
        .iter()
        .filter_map(|entry| match &entry.record {
            Record::HostTool {
                name,
                arguments,
                output,
                is_error,
                ..
            } if name == "bash" => Some((arguments.to_string(), output.clone(), *is_error)),
            _ => None,
        })
        .collect();
    let [(arguments, output, is_error)] = calls.as_slice() else {
        panic!("one bash call: {calls:?}");
    };
    assert_eq!(arguments, r#"{"command":"echo pushing origin"}"#);
    assert!(!is_error, "{output}");
    assert!(output.contains("pushing origin"), "{output}");
    assert!(
        output.contains("dry-run"),
        "the command that ran was rewritten: {output}"
    );
    assert!(
        output.contains("Note: gate `echo-dry-run` rewrote this call before it ran (added \"&& echo dry-run\" to command)."),
        "the result says it was rewritten: {output}"
    );

    let events = session.ask("wipe").await.expect("the refused call");
    assert_eq!(events.root().text(), "The command was refused.");
    let refusals: Vec<String> = session
        .recorder()
        .iter()
        .filter_map(|entry| match &entry.record {
            Record::HostTool {
                arguments,
                output,
                is_error: true,
                ..
            } if arguments.to_string().contains("rm -rf") => Some(output.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        refusals,
        vec!["Never run that. Delete only what you created.\n\n(refused by gate `echo-dry-run`; the call did not run)".to_owned()]
    );

    let lines = notices(&session);
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("gate echo-dry-run: rewrote bash (added")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("gate echo-dry-run: denied bash")),
        "{lines:?}"
    );
    let decisions: Vec<(String, i64)> = journaled_decisions(&session)
        .await
        .iter()
        .map(|decision| {
            (
                decision["decision"].as_str().unwrap_or_default().to_owned(),
                decision["rule"].as_i64().unwrap_or(-1),
            )
        })
        .collect();
    assert_eq!(
        decisions,
        vec![("rewrite".to_owned(), 1), ("deny".to_owned(), 0)]
    );
}

/// A turn that changed code and did not say the tests passed is continued once, then ends; the
/// next prompt of the person gets its own continuation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_gate_continues_a_turn_once_per_prompt_and_then_lets_it_end() {
    let session = boot().await;

    let events = session
        .ask("edit")
        .await
        .expect("the turn and its continuation");

    let received = prompts(&session);
    assert_eq!(received.len(), 2, "{received:?}");
    assert_eq!(received[0], "edit");
    assert!(
        received[1].starts_with("<gate_continue gate=\"tests-before-done\">")
            && received[1].contains("did not report a passing test run"),
        "{:?}",
        received[1]
    );
    assert_eq!(
        events.root().text(),
        "I changed the code.I have nothing more to add."
    );
    assert_eq!(
        prompts(&session).len(),
        2,
        "the continuation is not continued again"
    );
    assert!(
        notices(&session)
            .iter()
            .any(|line| line.starts_with("gate tests-before-done: continued the turn")),
        "{:?}",
        notices(&session)
    );

    session
        .ask("edit")
        .await
        .expect("a second prompt of the person");
    assert_eq!(
        prompts(&session).len(),
        4,
        "one more continuation, then it ends"
    );

    let decisions: Vec<String> = journaled_decisions(&session)
        .await
        .iter()
        .map(|decision| format!("{} {}", decision["gate"], decision["decision"]))
        .collect();
    assert_eq!(
        decisions,
        vec![
            "\"tests-before-done\" \"continue\"".to_owned(),
            "\"tests-before-done\" \"continue\"".to_owned()
        ]
    );
}

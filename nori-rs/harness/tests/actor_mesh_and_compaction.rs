//! End-to-end verification for peer-to-peer actor mesh messaging, monotonic
//! `LeaseFence` cancel-during-wake retention, cross-subagent compaction continuity,
//! and checkpoint-seeded fork parity without native `session/fork`.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use nori_config::NoriConfig;
use nori_conversation::ConversationLog;
use nori_conversation::Event;
use nori_harness::runtime::AgentPrepareSpec;
use nori_harness::runtime::LaunchedSession;
use nori_harness::runtime::SessionStart;
use nori_harness::runtime::prepare_and_launch_session;
use nori_protocol::AcpEvent;
use nori_protocol::NoriEvent;
use nori_protocol::Patch;
use nori_protocol::SessionEvent;
use nori_protocol::SessionPhase;
use nori_protocol::SubagentOutcome;
use nori_protocol::SubagentState;
use nori_protocol::SubagentWire;
use nori_protocol::acp::v1 as acp;
use serial_test::serial;

struct Env(Vec<&'static str>);

impl Env {
    fn set(vars: &[(&'static str, String)]) -> Self {
        for (key, value) in vars {
            // SAFETY: tests that mutate mock-agent env variables run under #[serial].
            unsafe { std::env::set_var(key, value) };
        }
        Self(vars.iter().map(|(key, _)| *key).collect())
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for key in &self.0 {
            // SAFETY: tests that mutate mock-agent env variables run under #[serial].
            unsafe { std::env::remove_var(key) };
        }
    }
}

fn launch(home: &Path, wire_log_dir: Option<&Path>) -> LaunchedSession {
    let mut config = NoriConfig {
        active_agent: "mock-model".to_owned(),
        cwd: home.to_path_buf(),
        nori_home: home.to_path_buf(),
        approval_policy: nori_config::AskForApproval::Never,
        ..Default::default()
    };
    if let Some(dir) = wire_log_dir {
        config.acp_proxy = nori_config::AcpProxyConfig {
            enabled: true,
            log_dir: dir.to_path_buf(),
        };
    }
    prepare_and_launch_session(
        AgentPrepareSpec {
            handoff: None,
            config: Arc::new(config),
            cli_version: "actor-mesh-compaction-test".to_owned(),
            session_context: None,
            initial_context: None,
        },
        SessionStart::New,
    )
}

#[expect(clippy::expect_used, reason = "fixture failure should fail loudly")]
async fn collect_until(
    session: &mut LaunchedSession,
    limit: Duration,
    mut done: impl FnMut(&[SessionEvent]) -> bool,
) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    let collected = tokio::time::timeout(limit, async {
        while !done(&events) {
            let event = session.events.recv().await.expect("event stream open");
            events.push(event);
        }
    })
    .await;
    assert!(collected.is_ok(), "timed out; events so far: {events:#?}");
    events
}

#[expect(clippy::expect_used, reason = "bootstrap must succeed")]
async fn wait_started(session: &mut LaunchedSession) -> String {
    let events = collect_until(session, Duration::from_secs(20), |events| {
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::Nori(NoriEvent::SessionStarted(_))))
    })
    .await;
    events
        .into_iter()
        .find_map(|event| match event {
            SessionEvent::Nori(NoriEvent::SessionStarted(started)) => {
                Some(started.acp_session_id.to_string())
            }
            _ => None,
        })
        .expect("session started event")
}

#[expect(clippy::expect_used, reason = "prompt submission must succeed")]
async fn prompt(session: &LaunchedSession, text: &str) -> acp::RequestId {
    session
        .handle
        .prompt(vec![acp::ContentBlock::Text(acp::TextContent::new(text))])
        .await
        .expect("submit prompt")
}

fn prompts(log: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn recorded_wire_prompts(wire_log_dir: &Path) -> Vec<serde_json::Value> {
    let mut logs: Vec<PathBuf> = std::fs::read_dir(wire_log_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    logs.sort();
    let mut out = Vec::new();
    for path in logs {
        for line in std::fs::read_to_string(path).unwrap_or_default().lines() {
            if let Ok(record) = serde_json::from_str::<serde_json::Value>(line)
                && record["direction"] == "client_to_agent"
                && record["message"]["method"] == "session/prompt"
            {
                out.push(record["message"]["params"].clone());
            }
        }
    }
    out
}

fn subagent_states(events: &[SessionEvent]) -> Vec<(String, SubagentState)> {
    events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Update(update))) => {
                match &update.state {
                    Patch::Set(state) => Some((update.session_id.to_string(), state.clone())),
                    Patch::Keep | Patch::Clear => None,
                }
            }
            _ => None,
        })
        .collect()
}

fn is_idle(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::Nori(NoriEvent::SessionPhaseChanged(SessionPhase::Idle))
    )
}

#[tokio::test]
#[serial]
async fn sibling_peer_to_peer_messaging_with_causal_wake_delivery() {
    let home = tempfile::tempdir().expect("temp home");
    let log = home.path().join("prompts.jsonl");
    let _env = Env::set(&[
        ("MOCK_AGENT_MCP_HTTP", "1".to_owned()),
        ("MOCK_AGENT_PROMPT_LOG", log.display().to_string()),
        ("MOCK_AGENT_MCP_TRIGGER", "PING_TRIGGER".to_owned()),
        ("MOCK_AGENT_STREAM_UNTIL_CANCEL_TEXT", "HOLD".to_owned()),
        (
            "MOCK_AGENT_MCP_CALLS_MOCK_MODEL_ALT",
            serde_json::json!([
                {
                    "tool": "task_send",
                    "arguments": {
                        "task_id": "actor://actor_b",
                        "message": "PING_TRIGGER hello actor_b",
                        "mode": "background"
                    }
                }
            ])
            .to_string(),
        ),
        (
            "MOCK_AGENT_MCP_CALLS_MOCK_MODEL",
            serde_json::json!([
                {
                    "tool": "task_send",
                    "arguments": {
                        "task_id": "actor://actor_a",
                        "message": "pong back from actor_b",
                        "mode": "background"
                    }
                },
                {
                    "tool": "task_send",
                    "arguments": {
                        "task_id": "actor://root",
                        "message": "status report for busy root",
                        "mode": "background"
                    }
                }
            ])
            .to_string(),
        ),
        (
            "MOCK_AGENT_RESPONSE_MOCK_MODEL_ALT",
            "actor_a acknowledged".to_owned(),
        ),
    ]);

    let mut session = launch(home.path(), None);
    let root_acp_id = wait_started(&mut session).await;

    let spawn_program = r#"amends "nori:Program"
name = "mesh"
actors {
  ["actor_b"] { agent = "mock-model" }
  ["actor_a"] { agent = "mock-model-alt" }
}
steps {
  new Spawn { actor = "actor_b"; bind = "actor_b_view" }
  new Spawn { actor = "actor_a"; bind = "actor_a_view" }
  new Send { to = "actor_b"; message = "Acknowledge."; await_reply = false }
  new Send { to = "actor_a"; message = "Acknowledge."; await_reply = false }
}"#;
    let spawned = session
        .handle
        .execute_conversation_program(spawn_program.to_owned())
        .await
        .expect("spawn actor_b and actor_a");
    let actor_a_view = spawned.bindings["actor_a_view"].clone();

    // Let both children complete their initial spawn turns and root go idle before the peer mesh run.
    let setup_events = collect_until(&mut session, Duration::from_secs(60), |events| {
        subagent_states(events)
            .iter()
            .filter(|(_, state)| {
                matches!(
                    state,
                    SubagentState::Ended(SubagentOutcome::Completed { .. })
                )
            })
            .count()
            >= 2
            && events.last().is_some_and(is_idle)
    })
    .await;
    assert!(!setup_events.is_empty());

    // Keep root actively mid-turn so actor_b -> actor://root queues until root finishes.
    prompt(&session, "HOLD root busy").await;
    collect_until(&mut session, Duration::from_secs(20), |events| {
        events.iter().any(|event| {
            matches!(
                event,
                SessionEvent::Nori(NoriEvent::SessionPhaseChanged(
                    SessionPhase::Prompting { .. }
                ))
            )
        })
    })
    .await;

    session
        .handle
        .send_subagent_message(
            acp::SessionId::new(actor_a_view.clone()),
            "PING_TRIGGER kick off peer relay".to_owned(),
        )
        .await
        .expect("trigger actor_a");

    // Wait until actor_a -> actor_b -> actor_a completes in the prompt log while root is still busy.
    tokio::time::timeout(Duration::from_secs(40), async {
        loop {
            let entries = prompts(&log);
            let b_woken = entries.iter().any(|entry| {
                entry["agent"] == "mock-model"
                    && entry["text"].as_str().is_some_and(|text| {
                        text.contains("[Message from actor://actor_a]")
                            && text.contains("PING_TRIGGER hello actor_b")
                    })
            });
            let a_woken = entries.iter().any(|entry| {
                entry["agent"] == "mock-model-alt"
                    && entry["text"].as_str().is_some_and(|text| {
                        text.contains("[Message from actor://actor_b]")
                            && text.contains("pong back from actor_b")
                    })
            });
            if b_woken && a_woken {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("sibling-to-sibling ping pong completes");

    // Root's turn is still busy; status report should not have interrupted it yet.
    assert!(
        !prompts(&log).iter().any(|entry| {
            entry["session"] == root_acp_id
                && entry["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("status report for busy root"))
        }),
        "queued mailbox message must wait until root's active turn finishes"
    );

    // Cancel root's HOLD turn; root transitions to Idle and drains its ActorMailbox automatically.
    session.handle.cancel().await.expect("cancel busy root");
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if prompts(&log).iter().any(|entry| {
                entry["session"] == root_acp_id
                    && entry["text"].as_str().is_some_and(|text| {
                        text.contains("[Message from actor://actor_b]")
                            && text.contains("status report for busy root")
                    })
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("root mailbox wake delivered once root became idle");

    let recorder = session
        .handle
        .conversation_recorder()
        .await
        .expect("recorder query")
        .expect("recorder enabled");
    recorder.flush().await.expect("flush journal");
    let journal =
        ConversationLog::open(recorder.transcript_path().with_extension("pkl")).expect("open pkl");
    let mut alias_names: Vec<String> = Vec::new();
    for record in journal.all().map(Result::unwrap) {
        let Event::Command { name, payload } = &record.event else {
            continue;
        };
        let Ok(val) = serde_json::from_str::<serde_json::Value>(payload) else {
            continue;
        };
        // The directory is journaled as `actor-delta` changes (`actor_registered` carries the
        // actor) and `actor-snapshot` trees.
        let actors: Vec<&serde_json::Value> = match name.as_str() {
            "actor-delta" if val["delta"] == "actor_registered" => vec![&val["actor"]],
            "actor-snapshot" => val["actors"].as_array().into_iter().flatten().collect(),
            _ => continue,
        };
        for actor in actors {
            alias_names.extend(
                actor["aliases"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|alias| alias.as_str().map(str::to_owned)),
            );
        }
    }
    assert!(
        alias_names.contains(&"actor_a".to_owned()),
        "{alias_names:?}"
    );
    assert!(
        alias_names.contains(&"actor_b".to_owned()),
        "{alias_names:?}"
    );

    session.handle.shutdown().await.expect("shutdown");
}

#[tokio::test]
#[serial]
async fn simultaneous_parent_cancel_and_child_completion_retains_verdict() {
    let home = tempfile::tempdir().expect("temp home");
    let log = home.path().join("prompts.jsonl");
    let _env = Env::set(&[
        ("MOCK_AGENT_MCP_HTTP", "1".to_owned()),
        ("MOCK_AGENT_PROMPT_LOG", log.display().to_string()),
        ("MOCK_AGENT_STREAM_UNTIL_CANCEL_TEXT", "HOLD".to_owned()),
        (
            "MOCK_AGENT_MCP_CALLS_MOCK_MODEL",
            serde_json::json!([{
                "tool": "delegate_task",
                "arguments": {
                    "task": "Produce final verdict",
                    "agent": "mock-model-alt",
                    "mode": "background"
                }
            }])
            .to_string(),
        ),
        (
            "MOCK_AGENT_RESPONSE_MOCK_MODEL_ALT",
            "Verdict alpha retained.".to_owned(),
        ),
    ]);

    let mut session = launch(home.path(), None);
    let root_acp_id = wait_started(&mut session).await;

    prompt(&session, "DELEGATE verdict work").await;
    // As soon as the delegation launches, enter a busy HOLD turn on parent so child completion
    // occurs before parent goes idle.
    collect_until(&mut session, Duration::from_secs(30), |events| {
        events.last().is_some_and(is_idle)
            || subagent_states(events)
                .iter()
                .any(|(_, state)| matches!(state, SubagentState::Running))
    })
    .await;
    prompt(&session, "HOLD concurrent parent work").await;
    collect_until(&mut session, Duration::from_secs(40), |events| {
        subagent_states(events).iter().any(|(_, state)| {
            matches!(
                state,
                SubagentState::Ended(SubagentOutcome::Completed { .. })
            )
        })
    })
    .await;

    // The person stops the delegations as the parent's turn ends. Cancelling a turn alone ends
    // that turn only (delegated actors are independent and their results arrive at the next
    // boundary, see docs/turn-lifecycle.md); an explicit stop marks the finished task
    // Wake::AfterNextTurn so its verdict is retained instead of fired into the cancelled turn's
    // boundary or lost.
    session
        .handle
        .stop_subagents()
        .await
        .expect("stop delegations");
    session.handle.cancel().await.expect("cancel parent turn");
    collect_until(&mut session, Duration::from_secs(20), |events| {
        events.last().is_some_and(is_idle)
    })
    .await;

    // Brief pause confirms held verdict waits for the user's next turn.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !prompts(&log).iter().any(|entry| {
            entry["session"] == root_acp_id
                && entry["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("Verdict alpha retained."))
        }),
        "verdict held across cancel must wait until next user turn finishes"
    );

    // Next user turn releases held verdicts on completion.
    prompt(&session, "Proceed to next user turn").await;
    tokio::time::timeout(Duration::from_secs(40), async {
        loop {
            if prompts(&log).iter().any(|entry| {
                entry["session"] == root_acp_id
                    && entry["text"].as_str().is_some_and(|text| {
                        text.contains("<delegated_task_results>")
                            && text.contains("Verdict alpha retained.")
                    })
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("retained verdict delivered after next user turn");

    session.handle.shutdown().await.expect("shutdown");
}

#[tokio::test]
#[serial]
async fn cross_subagent_compaction_preserves_live_children_aliases_mailboxes_and_digests() {
    let home = tempfile::tempdir().expect("temp home");
    let wire = home.path().join("wire");
    let log = home.path().join("prompts.jsonl");
    let _env = Env::set(&[
        ("MOCK_AGENT_MCP_HTTP", "1".to_owned()),
        ("MOCK_AGENT_PROMPT_LOG", log.display().to_string()),
        (
            "MOCK_AGENT_SUMMARY_RESPONSE",
            "## Objective\n- Keep subagent mesh and reviewer continuity".to_owned(),
        ),
        (
            "MOCK_AGENT_MCP_CALLS_MOCK_MODEL",
            serde_json::json!([{
                "tool": "delegate_task",
                "arguments": {
                    "task": "Review concurrency race",
                    "agent": "mock-model-alt",
                    "alias": "reviewer",
                    "role": "review",
                    "mode": "wait"
                }
            }])
            .to_string(),
        ),
        (
            "MOCK_AGENT_RESPONSE_MOCK_MODEL_ALT",
            "Found lease race in wake.rs.".to_owned(),
        ),
    ]);

    let mut session = launch(home.path(), Some(&wire));
    let bootstrap_session_id = wait_started(&mut session).await;

    prompt(&session, "DELEGATE setup reviewer").await;
    let setup_events = collect_until(&mut session, Duration::from_secs(60), |events| {
        events.last().is_some_and(is_idle)
            && subagent_states(events).iter().any(|(_, state)| {
                matches!(
                    state,
                    SubagentState::Ended(SubagentOutcome::Completed { .. })
                )
            })
    })
    .await;
    let child_id = subagent_states(&setup_events)
        .into_iter()
        .find_map(|(id, state)| {
            matches!(
                state,
                SubagentState::Ended(SubagentOutcome::Completed { .. })
            )
            .then(|| acp::SessionId::new(id))
        })
        .expect("child view id");

    // 1) Compact the child first; relay records its ActorDigest into the shared mesh.
    let child_handle = session
        .handle
        .controlled_subagent_handle(child_id.clone())
        .await
        .expect("child lookup")
        .expect("live child handle");
    let mut child_events = child_handle
        .subscribe_events()
        .await
        .expect("subscribe child events");
    child_handle.compact().await.expect("compact child");
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if matches!(
                child_events.recv().await,
                Some(SessionEvent::Nori(NoriEvent::ContextCompacted(_)))
            ) {
                break;
            }
        }
    })
    .await
    .expect("child emits ContextCompacted");

    // 2) Compact the parent (summarize-and-swap binds a brand-new ACP session).
    session.handle.compact().await.expect("compact parent");
    collect_until(&mut session, Duration::from_secs(20), |events| {
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::Nori(NoriEvent::ContextCompacted(_))))
    })
    .await;

    // 3) Next parent prompt must embed `<subagent-continuity>` inside its `<conversation-checkpoint>`.
    let next_req = prompt(&session, "Continue coordinating reviewer").await;
    collect_until(&mut session, Duration::from_secs(20), |events| {
        events.iter().any(|event| {
            matches!(
                event,
                SessionEvent::Acp(AcpEvent::Response {
                    request_id,
                    response: Ok(acp::AgentResponse::PromptResponse(_)),
                }) if request_id == &next_req
            )
        })
    })
    .await;

    let wire_prompts = recorded_wire_prompts(&wire);
    let post_compact_parent = wire_prompts
        .iter()
        .rfind(|params| {
            serde_json::to_string(params)
                .unwrap_or_default()
                .contains("Continue coordinating reviewer")
        })
        .expect("post-compact parent prompt on wire");
    assert_ne!(
        post_compact_parent["sessionId"].as_str(),
        Some(bootstrap_session_id.as_str()),
        "parent should swap onto fresh ACP session"
    );
    let checkpoint = post_compact_parent["prompt"][0]["text"]
        .as_str()
        .expect("checkpoint block");
    assert!(
        checkpoint.starts_with("<conversation-checkpoint>"),
        "{checkpoint}"
    );
    assert!(checkpoint.contains("<subagent-continuity>"), "{checkpoint}");
    assert!(
        checkpoint.contains(&format!("actor://reviewer (session {child_id})")),
        "{checkpoint}"
    );
    assert!(
        checkpoint.contains("last: Found lease race in wake.rs."),
        "{checkpoint}"
    );
    assert!(
        checkpoint.contains("Keep subagent mesh and reviewer continuity"),
        "{checkpoint}"
    );

    // 4) Follow-up routing to the live child session stays intact after parent compaction.
    session
        .handle
        .send_subagent_message(child_id, "post-compact follow-up".to_owned())
        .await
        .expect("send follow-up to live child after parent compact");
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if prompts(&log).iter().any(|entry| {
                entry["agent"] == "mock-model-alt"
                    && entry["text"]
                        .as_str()
                        .is_some_and(|text| text.contains("post-compact follow-up"))
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("live child received post-compact follow-up");

    session.handle.shutdown().await.expect("shutdown");
}

#[tokio::test]
#[serial]
async fn checkpoint_fork_parity_without_native_session_fork() {
    let home = tempfile::tempdir().expect("temp home");
    let log = home.path().join("prompts.jsonl");
    // Explicitly omit MOCK_AGENT_SUPPORT_SESSION_FORK to model Codex/OpenCode/Gemini adapters
    // that do not advertise native `session/fork`.
    let _env = Env::set(&[("MOCK_AGENT_PROMPT_LOG", log.display().to_string())]);

    let mut session = launch(home.path(), None);
    wait_started(&mut session).await;

    let first = prompt(&session, "Establish checkpoint fact alpha").await;
    collect_until(&mut session, Duration::from_secs(20), |events| {
        events.iter().any(|event| {
            matches!(
                event,
                SessionEvent::Acp(AcpEvent::Response {
                    request_id,
                    response: Ok(acp::AgentResponse::PromptResponse(_)),
                }) if request_id == &first
            )
        }) && events.last().is_some_and(is_idle)
    })
    .await;

    session
        .handle
        .fork_history("Checkpoint parity fork".to_owned(), None)
        .await
        .expect("checkpoint-seeded fork should succeed without session/fork");

    let fork_events = collect_until(&mut session, Duration::from_secs(40), |events| {
        subagent_states(events).iter().any(|(_, state)| {
            matches!(
                state,
                SubagentState::Idle { .. }
                    | SubagentState::Ended(SubagentOutcome::Completed { .. })
            )
        })
    })
    .await;
    let forked_child_id = subagent_states(&fork_events)
        .into_iter()
        .find_map(|(id, state)| {
            matches!(
                state,
                SubagentState::Idle { .. }
                    | SubagentState::Ended(SubagentOutcome::Completed { .. })
            )
            .then(|| acp::SessionId::new(id))
        })
        .expect("forked child session id");

    // Verify the forked child received seeded context from parent turn and stays responsive.
    session
        .handle
        .send_subagent_message(forked_child_id, "followup on checkpoint fork".to_owned())
        .await
        .expect("follow-up on checkpoint fork");

    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let child_prompts: Vec<_> = prompts(&log)
                .into_iter()
                .filter(|entry| entry["agent"] == "mock-model")
                .collect();
            let seeded = child_prompts.iter().any(|entry| {
                entry["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("Establish checkpoint fact alpha"))
            });
            let followed = child_prompts
                .iter()
                .any(|entry| entry["text"] == "followup on checkpoint fork");
            if seeded && followed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("checkpoint fork preserved parent context and accepted follow-ups");

    session.handle.shutdown().await.expect("shutdown");
}

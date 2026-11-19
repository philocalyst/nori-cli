//! Delegated tasks end to end: a parent on one mock agent delegates to a child on another, through
//! the `nori-client` MCP server the harness gives it.

use std::path::Path;
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
use nori_protocol::SessionEvent;
use nori_protocol::SubagentOutcome;
use nori_protocol::SubagentState;
use nori_protocol::SubagentWire;
use nori_protocol::acp::v1 as acp;
use pretty_assertions::assert_eq;
use serial_test::serial;

/// Every variable a test here sets, cleared when it ends.
struct Env(Vec<&'static str>);

impl Env {
    fn set(vars: &[(&'static str, String)]) -> Self {
        for (key, value) in vars {
            // SAFETY: tests that mutate the mock-agent environment run serially.
            unsafe { std::env::set_var(key, value) };
        }
        Self(vars.iter().map(|(key, _)| *key).collect())
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for key in &self.0 {
            // SAFETY: tests that mutate the mock-agent environment run serially.
            unsafe { std::env::remove_var(key) };
        }
    }
}

fn launch(home: &Path) -> LaunchedSession {
    let config = NoriConfig {
        active_agent: "mock-model".to_string(),
        cwd: home.to_path_buf(),
        nori_home: home.to_path_buf(),
        approval_policy: nori_config::AskForApproval::Never,
        ..Default::default()
    };
    prepare_and_launch_session(
        AgentPrepareSpec {
            handoff: None,
            config: Arc::new(config),
            cli_version: "delegation-test".to_string(),
            session_context: None,
            initial_context: None,
        },
        SessionStart::New,
    )
}

/// Collect events until `done` says so, failing after `limit`.
#[expect(clippy::expect_used, reason = "fixture failures should fail loudly")]
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

fn subagent_states(events: &[SessionEvent]) -> Vec<(String, SubagentState)> {
    events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Update(update))) => {
                match &update.state {
                    nori_protocol::Patch::Set(state) => {
                        Some((update.session_id.to_string(), state.clone()))
                    }
                    nori_protocol::Patch::Keep | nori_protocol::Patch::Clear => None,
                }
            }
            _ => None,
        })
        .collect()
}

fn child_text(events: &[SessionEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Acp(AcpEvent::ChildNotification(notification)) => {
                match &notification.update {
                    acp::SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                        acp::ContentBlock::Text(text) => Some(text.text.clone()),
                        _ => None,
                    },
                    _ => None,
                }
            }
            _ => None,
        })
        .collect()
}

fn tool_results(events: &[SessionEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Acp(AcpEvent::Notification(
                acp::AgentNotification::SessionNotification(notification),
            )) => match &notification.update {
                acp::SessionUpdate::ToolCallUpdate(update) => {
                    update.fields.content.as_ref().map(|content| {
                        content
                            .iter()
                            .filter_map(|content| match content {
                                acp::ToolCallContent::Content(content) => match &content.content {
                                    acp::ContentBlock::Text(text) => Some(text.text.clone()),
                                    _ => None,
                                },
                                _ => None,
                            })
                            .collect::<String>()
                    })
                }
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn prompts(log: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn started_turns(events: &[SessionEvent]) -> usize {
    events
        .iter()
        .filter(|event| {
            matches!(
                event,
                SessionEvent::Nori(NoriEvent::SessionPhaseChanged(
                    nori_protocol::SessionPhase::Prompting { .. }
                ))
            )
        })
        .count()
}

fn is_idle(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::Nori(NoriEvent::SessionPhaseChanged(
            nori_protocol::SessionPhase::Idle
        ))
    )
}

async fn started(session: &mut LaunchedSession) {
    collect_until(session, Duration::from_secs(20), |events| {
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::Nori(NoriEvent::SessionStarted(_))))
    })
    .await;
}

async fn prompt(session: &LaunchedSession, text: &str) {
    let content = vec![acp::ContentBlock::Text(acp::TextContent::new(text))];
    if let Err(error) = session.handle.prompt(content).await {
        panic!("prompt: {error:#}");
    }
}

/// A task delegated in the background to a different agent runs as a subagent of the parent, and
/// its result reaches the parent agent once, in a completion wake, when the parent is idle.
#[tokio::test]
#[serial]
async fn a_background_task_on_another_agent_wakes_the_parent_with_its_result() {
    let home = tempfile::tempdir().unwrap_or_else(|error| panic!("home: {error}"));
    let log = home.path().join("prompts.jsonl");
    let _env = Env::set(&[
        ("MOCK_AGENT_MCP_HTTP", "1".to_string()),
        (
            "MOCK_AGENT_MCP_CALLS_MOCK_MODEL",
            serde_json::json!([{
                "tool": "delegate_task",
                "arguments": {"task": "Read a.txt and report it", "agent": "mock-model-alt", "title": "Read a.txt"}
            }])
            .to_string(),
        ),
        ("MOCK_AGENT_RESPONSE_MOCK_MODEL_ALT", "The file holds alpha.".to_string()),
        ("MOCK_AGENT_PROMPT_LOG", log.display().to_string()),
    ]);
    let mut session = launch(home.path());
    started(&mut session).await;
    prompt(&session, "DELEGATE the reading").await;

    // The parent's own turn ends with the task handle; the child then runs and ends; then the
    // wake turn runs and ends.
    let events = collect_until(&mut session, Duration::from_secs(60), |events| {
        started_turns(events) >= 2 && events.last().is_some_and(is_idle)
    })
    .await;

    let states = subagent_states(&events);
    assert!(
        matches!(states.first(), Some((_, SubagentState::Running))),
        "{states:?}"
    );
    assert!(
        states.iter().any(|(_, state)| matches!(
            state,
            SubagentState::Ended(SubagentOutcome::Completed { .. })
        )),
        "{states:?}"
    );
    assert!(child_text(&events).contains("The file holds alpha."));
    assert!(
        tool_results(&events)
            .iter()
            .any(|result| result.contains("\"task_id\":\"task-1\"")
                && result.contains("delivered to you")),
        "{:?}",
        tool_results(&events)
    );

    let parent_prompts: Vec<String> = prompts(&log)
        .into_iter()
        .filter(|prompt| prompt["agent"] == "mock-model")
        .filter_map(|prompt| prompt["text"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(parent_prompts.len(), 2, "{parent_prompts:?}");
    assert!(parent_prompts[1].contains("<delegated_task_results>"));
    assert!(parent_prompts[1].contains("The file holds alpha."));
    assert!(parent_prompts[1].contains("agent=\"mock-model-alt\""));
    session
        .handle
        .shutdown()
        .await
        .unwrap_or_else(|error| panic!("{error:#}"));
}

/// A task the parent waits on returns its result to that call, so no wake follows; a follow-up
/// reaches the same child session; a model is chosen by name, and one the agent does not list is
/// still given to an agent that takes its model at spawn, which then runs it.
#[tokio::test]
#[serial]
async fn waits_follow_ups_and_models() {
    let home = tempfile::tempdir().unwrap_or_else(|error| panic!("home: {error}"));
    let log = home.path().join("prompts.jsonl");
    let _env = Env::set(&[
        ("MOCK_AGENT_MCP_HTTP", "1".to_string()),
        (
            "MOCK_AGENT_MCP_CALLS_MOCK_MODEL",
            serde_json::json!([
                {"tool": "delegate_task", "arguments": {"task": "first", "agent": "mock-model-alt", "mode": "wait", "model": "Mock Fast Model"}},
                {"tool": "task_send", "arguments": {"task_id": "$task_id", "message": "second", "mode": "wait"}},
                {"tool": "delegate_task", "arguments": {"task": "third", "agent": "mock-model-alt", "mode": "wait", "model": "gpt-9"}}
            ])
            .to_string(),
        ),
        ("MOCK_AGENT_RESPONSE_MOCK_MODEL_ALT", "Child answer.".to_string()),
        ("MOCK_AGENT_PROMPT_LOG", log.display().to_string()),
    ]);
    let mut session = launch(home.path());
    started(&mut session).await;
    prompt(&session, "DELEGATE twice").await;
    let events = collect_until(&mut session, Duration::from_secs(60), |events| {
        started_turns(events) >= 1 && events.last().is_some_and(is_idle)
    })
    .await;
    // Nothing is owed, so no wake starts.
    let quiet = tokio::time::timeout(Duration::from_millis(1500), session.events.recv()).await;
    assert!(
        !matches!(
            quiet,
            Ok(Some(SessionEvent::Nori(NoriEvent::SessionPhaseChanged(_))))
        ),
        "{quiet:?}"
    );

    let announced_models = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Update(update))) => {
                match &update.model {
                    nori_protocol::Patch::Set(model) => Some(model.as_str()),
                    nori_protocol::Patch::Keep | nori_protocol::Patch::Clear => None,
                }
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        announced_models.contains(&"Mock Fast Model"),
        "{announced_models:?}"
    );
    assert!(announced_models.contains(&"gpt-9"), "{announced_models:?}");

    let results = tool_results(&events);
    assert!(
        results[0].contains("\"status\":\"completed\""),
        "{results:?}"
    );
    assert!(
        results[0].contains("\"model\":\"Mock Fast Model\""),
        "{results:?}"
    );
    assert!(
        results[0].contains("\"result\":\"Child answer.\""),
        "{results:?}"
    );
    assert!(results[1].contains("\"task_id\":\"task-2\""), "{results:?}");
    assert!(
        results[1].contains("\"result\":\"Child answer.\""),
        "{results:?}"
    );
    assert!(results[2].contains("\"model\":\"gpt-9\""), "{results:?}");
    assert!(
        results[2].contains("\"status\":\"completed\""),
        "{results:?}"
    );

    let children: Vec<(i64, String)> = prompts(&log)
        .into_iter()
        .filter(|prompt| prompt["agent"] == "mock-model-alt")
        .filter_map(|prompt| {
            Some((
                prompt["pid"].as_i64()?,
                prompt["session"].as_str()?.to_owned(),
            ))
        })
        .collect();
    assert_eq!(children.len(), 3, "{children:?}");
    assert_eq!(
        children[0], children[1],
        "the follow-up reached another child"
    );
    assert_ne!(children[0].0, children[2].0, "a new task reused a child");
    session
        .handle
        .shutdown()
        .await
        .unwrap_or_else(|error| panic!("{error:#}"));
}

/// Connection-local native IDs from sibling child harnesses must never share a public route.
#[tokio::test]
#[serial]
async fn sibling_native_children_with_the_same_provider_id_route_independently() {
    let home = tempfile::tempdir().unwrap_or_else(|error| panic!("home: {error}"));
    let log = home.path().join("prompts.jsonl");
    let replay = home.path().join("native.jsonl");
    std::fs::write(&replay, concat!(
        "{\"t_ms\":0,\"update\":{\"sessionUpdate\":\"subagent_update\",\"sessionId\":\"native-child\",\"state\":{\"state\":\"running\"}}}\n",
        "{\"t_ms\":10,\"session\":\"native-child\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"native answer\"}}}\n",
        "{\"t_ms\":20,\"after_response\":true,\"update\":{\"sessionUpdate\":\"subagent_update\",\"sessionId\":\"native-child\",\"state\":{\"state\":\"turn_idle\"}}}\n"
    )).expect("native child fixture");
    let _env = Env::set(&[
        ("MOCK_AGENT_WIRE_REPLAY", replay.display().to_string()),
        (
            "MOCK_AGENT_REPLAY_EXCEPT_TEXT",
            "unique-native-followup".to_owned(),
        ),
        ("MOCK_AGENT_PROMPT_LOG", log.display().to_string()),
    ]);
    let mut session = launch(home.path());
    started(&mut session).await;
    session
        .handle
        .fork_history("first sibling".to_owned(), None)
        .await
        .expect("first fork");
    session
        .handle
        .fork_history("second sibling".to_owned(), None)
        .await
        .expect("second fork");
    let events = collect_until(&mut session, Duration::from_secs(40), |events| {
        events
            .iter()
            .filter_map(|event| match event {
                SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Update(update)))
                    if update.session_id.to_string().starts_with("nori-relay:")
                        && matches!(
                            &update.state,
                            nori_protocol::Patch::Set(SubagentState::Idle)
                        ) =>
                {
                    Some(update.session_id.clone())
                }
                _ => None,
            })
            .collect::<std::collections::HashSet<_>>()
            .len()
            == 2
    })
    .await;
    let mut children = Vec::new();
    for event in events {
        if let SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Update(update))) = event
            && update.session_id.to_string().starts_with("nori-relay:")
            && matches!(
                &update.state,
                nori_protocol::Patch::Set(SubagentState::Idle)
            )
            && !children.iter().any(|(id, _)| id == &update.session_id)
        {
            children.push((update.session_id, update.parent_session_id));
        }
    }
    assert_eq!(children.len(), 2);
    assert_ne!(children[0].0, children[1].0);
    assert_ne!(children[0].1, children[1].1);
    let first = vec![
        acp::ContentBlock::Text(acp::TextContent::new("unique-native-followup-one")),
        acp::ContentBlock::Image(acp::ImageContent::new("aW1hZ2U=", "image/jpeg")),
    ];
    session
        .handle
        .send_subagent_content(children[0].0.clone(), first.clone())
        .await
        .expect("first native child");
    session
        .handle
        .send_subagent_content(
            children[1].0.clone(),
            vec![acp::ContentBlock::Text(acp::TextContent::new(
                "unique-native-followup-two",
            ))],
        )
        .await
        .expect("second native child");
    let targeted = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let entries: Vec<_> = prompts(&log)
                .into_iter()
                .filter(|entry| {
                    entry["session"] == "native-child"
                        && entry["text"]
                            .as_str()
                            .is_some_and(|text| text.starts_with("unique-native-followup"))
                })
                .collect();
            if entries.len() >= 2 {
                break entries;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("both native child prompts");
    let one = targeted
        .iter()
        .find(|entry| entry["text"] == "unique-native-followup-one")
        .expect("first prompt");
    let two = targeted
        .iter()
        .find(|entry| entry["text"] == "unique-native-followup-two")
        .expect("second prompt");
    assert_ne!(
        one["pid"], two["pid"],
        "requests must reach different owning agents"
    );
    assert_eq!(
        one["content"],
        serde_json::to_value(first).expect("typed content")
    );
    session.handle.shutdown().await.expect("shutdown");
}

/// A user follow-up to a child retains every ACP content block in both the child prompt and the
/// canonical command journal, while the parent stays on its existing turn.
#[tokio::test]
#[serial]
async fn typed_child_follow_up_preserves_text_image_and_embedded_resource() {
    let home = tempfile::tempdir().unwrap_or_else(|error| panic!("home: {error}"));
    let log = home.path().join("prompts.jsonl");
    let _env = Env::set(&[
        ("MOCK_AGENT_MCP_HTTP", "1".to_string()),
        (
            "MOCK_AGENT_MCP_CALLS_MOCK_MODEL",
            serde_json::json!([{
                "tool": "delegate_task",
                "arguments": {
                    "task": "initial child task",
                    "agent": "mock-model-alt",
                    "title": "Initial child task",
                    "mode": "wait"
                }
            }])
            .to_string(),
        ),
        (
            "MOCK_AGENT_RESPONSE_MOCK_MODEL_ALT",
            "Child reply.".to_string(),
        ),
        ("MOCK_AGENT_PROMPT_LOG", log.display().to_string()),
    ]);
    // Finish the initial delegation inside its parent turn. A background
    // result would legitimately wake the parent while this follow-up is tested.
    let mut session = launch(home.path());
    started(&mut session).await;
    prompt(&session, "DELEGATE a child task").await;
    let events = collect_until(&mut session, Duration::from_secs(60), |events| {
        started_turns(events) >= 1
            && events.last().is_some_and(is_idle)
            && subagent_states(events)
                .iter()
                .any(|(_, state)| matches!(state, SubagentState::Running))
    })
    .await;
    let child_id = subagent_states(&events)
        .into_iter()
        .find_map(|(id, state)| matches!(state, SubagentState::Running).then_some(id))
        .expect("child session is announced as running");

    let content = vec![
        acp::ContentBlock::Text(acp::TextContent::new("Review this attachment.")),
        acp::ContentBlock::Image(acp::ImageContent::new("aW1hZ2UtYnl0ZXM=", "image/jpeg")),
        acp::ContentBlock::Resource(acp::EmbeddedResource::new(
            acp::EmbeddedResourceResource::TextResourceContents(
                acp::TextResourceContents::new("embedded resource body", "file:///notes.txt")
                    .mime_type("text/plain"),
            ),
        )),
    ];
    let expected_content = serde_json::to_value(&content).expect("serialize ACP content");
    session
        .handle
        .send_subagent_content(acp::SessionId::new(child_id.clone()), content.clone())
        .await
        .expect("send typed child follow-up");

    let child_prompt = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(prompt) = prompts(&log).into_iter().find(|prompt| {
                prompt["agent"] == "mock-model-alt" && prompt["content"] == expected_content
            }) {
                break prompt;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "child did not receive full ACP content; prompt log: {:?}",
            prompts(&log)
        )
    });
    assert_eq!(child_prompt["text"], "Review this attachment.");

    let recorder = session
        .handle
        .conversation_recorder()
        .await
        .expect("get conversation recorder")
        .expect("conversation journal enabled");
    let journal = ConversationLog::open(recorder.transcript_path().with_extension("pkl"))
        .expect("open canonical conversation journal");
    let recorded = journal
        .records()
        .iter()
        .find_map(|record| match &record.event {
            Event::Command { name, payload } if name == "send-subagent-message" => {
                Some(serde_json::from_str::<serde_json::Value>(payload).expect("command JSON"))
            }
            _ => None,
        });
    let recorded = recorded.expect("typed follow-up command is journaled");
    assert_eq!(recorded["session_id"], child_id);
    assert_eq!(recorded["content"], expected_content);

    let mut parent_events = Vec::new();
    while let Ok(Some(event)) =
        tokio::time::timeout(Duration::from_millis(100), session.events.recv()).await
    {
        parent_events.push(event);
    }
    assert!(
        parent_events.iter().all(|event| !matches!(
            event,
            SessionEvent::Nori(NoriEvent::SessionPhaseChanged(
                nori_protocol::SessionPhase::Prompting { .. }
            ))
        )),
        "a child follow-up must not start another parent turn: {parent_events:#?}"
    );
    assert_eq!(
        prompts(&log)
            .iter()
            .filter(|prompt| prompt["agent"] == "mock-model")
            .count(),
        1
    );
    session
        .handle
        .shutdown()
        .await
        .unwrap_or_else(|error| panic!("{error:#}"));
}

/// Queued and steered user content remains on the same child and keeps its typed blocks.
#[tokio::test]
#[serial]
async fn child_queue_promote_and_steer_preserve_order_and_images() {
    let home = tempfile::tempdir().unwrap_or_else(|error| panic!("home: {error}"));
    let log = home.path().join("prompts.jsonl");
    let cancel_log = home.path().join("cancels.log");
    let _env = Env::set(&[
        ("MOCK_AGENT_MCP_HTTP", "1".to_owned()),
        ("MOCK_AGENT_MCP_CALLS_MOCK_MODEL", serde_json::json!([{
            "tool": "delegate_task",
            "arguments": {"task": "HOLD initial", "agent": "mock-model-alt", "mode": "background"}
        }]).to_string()),
        ("MOCK_AGENT_STREAM_UNTIL_CANCEL_TEXT", "HOLD".to_owned()),
        ("MOCK_AGENT_PROMPT_LOG", log.display().to_string()),
        ("MOCK_AGENT_CANCEL_LOG", cancel_log.display().to_string()),
    ]);
    let mut session = launch(home.path());
    started(&mut session).await;
    prompt(&session, "DELEGATE child").await;
    let events = collect_until(&mut session, Duration::from_secs(60), |events| {
        subagent_states(events)
            .iter()
            .any(|(_, state)| matches!(state, SubagentState::Running))
    })
    .await;
    let child_id = subagent_states(&events)
        .into_iter()
        .find_map(|(id, state)| {
            matches!(state, SubagentState::Running).then_some(acp::SessionId::new(id))
        })
        .expect("child announced");
    let child_handle = session
        .handle
        .controlled_subagent_handle(child_id.clone())
        .await
        .unwrap_or_else(|error| panic!("child handle: {error:#}"))
        .unwrap_or_else(|| panic!("controlled child handle missing"));
    let mut child_events = child_handle
        .subscribe_events()
        .await
        .unwrap_or_else(|error| panic!("child events: {error:#}"));
    let trace = Arc::new(std::sync::Mutex::new(Vec::new()));
    let trace_sink = Arc::clone(&trace);
    tokio::spawn(async move {
        while let Some(event) = child_events.recv().await {
            if matches!(
                &event,
                SessionEvent::Nori(_) | SessionEvent::Acp(AcpEvent::Response { .. })
            ) {
                trace_sink
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(event);
            }
        }
    });
    let wait_for = |count| {
        let log = &log;
        let cancel_log = &cancel_log;
        let trace = Arc::clone(&trace);
        async move {
            tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    let child_prompts: Vec<_> = prompts(log)
                        .into_iter()
                        .filter(|prompt| prompt["agent"] == "mock-model-alt")
                        .collect();
                    if child_prompts.len() >= count {
                        break child_prompts;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap_or_else(|error| panic!("waiting for child prompt {count}: {error}; observed prompts: {:#?}; observed cancels: {:?}; child lifecycle: {:#?}", prompts(log), std::fs::read_to_string(cancel_log), trace.lock().unwrap_or_else(std::sync::PoisonError::into_inner)))
        }
    };
    let first = wait_for(1).await;
    let image = || acp::ContentBlock::Image(acp::ImageContent::new("aW1hZ2U=", "image/jpeg"));
    let queued = vec![
        acp::ContentBlock::Text(acp::TextContent::new("HOLD queued")),
        image(),
    ];
    let later = vec![
        acp::ContentBlock::Text(acp::TextContent::new("later")),
        image(),
    ];
    let steered = vec![
        acp::ContentBlock::Text(acp::TextContent::new("HOLD steer")),
        image(),
    ];
    session
        .handle
        .send_subagent_content(child_id.clone(), queued.clone())
        .await
        .expect("queue first");
    session
        .handle
        .send_subagent_content(child_id.clone(), later.clone())
        .await
        .expect("queue second");
    collect_until(&mut session, Duration::from_secs(20), |events| events.iter().any(|event| matches!(event,
        SessionEvent::Nori(NoriEvent::SubagentQueueChanged { session_id, prompts })
            if session_id == &child_id && prompts.iter().map(String::as_str).eq(["HOLD queued", "later"])
    ))).await;
    session
        .handle
        .promote_subagent_queue(child_id.clone())
        .await
        .expect("promote oldest");
    let promoted = wait_for(2).await;
    assert_eq!(
        promoted[1]["content"],
        serde_json::to_value(&queued).expect("queued content")
    );
    session
        .handle
        .steer_subagent_content(child_id.clone(), steered.clone())
        .await
        .expect("steer");
    let priority = wait_for(3).await;
    assert_eq!(
        priority[2]["content"],
        serde_json::to_value(&steered).expect("steered content")
    );
    collect_until(&mut session, Duration::from_secs(20), |events| {
        events.iter().any(|event| {
            matches!(event,
                SessionEvent::Nori(NoriEvent::SubagentQueueChanged { session_id, prompts })
                    if session_id == &child_id && prompts.iter().map(String::as_str).eq(["later"])
            )
        })
    })
    .await;
    session
        .handle
        .cancel_subagent(child_id)
        .await
        .expect("cancel steer");
    let final_prompts = wait_for(4).await;
    assert_eq!(
        final_prompts[3]["content"],
        serde_json::to_value(&later).expect("later content")
    );
    for child in &final_prompts {
        assert_eq!(
            (&child["pid"], &child["session"]),
            (&first[0]["pid"], &first[0]["session"])
        );
    }
    assert_eq!(
        prompts(&log)
            .iter()
            .filter(|prompt| prompt["agent"] == "mock-model")
            .count(),
        1
    );
    session.handle.shutdown().await.expect("shutdown");
}

#[tokio::test]
#[serial]
async fn exited_child_emits_closed_for_its_announced_view() {
    let home = tempfile::tempdir().unwrap_or_else(|error| panic!("home: {error}"));
    let _env = Env::set(&[
        ("MOCK_AGENT_MCP_HTTP", "1".to_owned()),
        ("MOCK_AGENT_MCP_CALLS_MOCK_MODEL", serde_json::json!([{
            "tool": "delegate_task",
            "arguments": {"task": "exit during prompt", "agent": "mock-model-alt", "mode": "background"}
        }]).to_string()),
        ("MOCK_AGENT_EXIT_DURING_PROMPT_MODEL", "mock-model-alt".to_owned()),
    ]);
    let mut session = launch(home.path());
    started(&mut session).await;
    prompt(&session, "DELEGATE child").await;
    let events = collect_until(&mut session, Duration::from_secs(60), |events| {
        subagent_states(events)
            .iter()
            .any(|(_, state)| matches!(state, SubagentState::Ended(SubagentOutcome::Closed)))
    })
    .await;
    let states = subagent_states(&events);
    let announced = states
        .iter()
        .find_map(|(id, state)| matches!(state, SubagentState::Running).then_some(id))
        .expect("child was announced");
    assert!(states.iter().any(|(id, state)| id == announced
        && matches!(state, SubagentState::Ended(SubagentOutcome::Closed))));
    session.handle.shutdown().await.expect("shutdown");
}

/// A child that needs permission asks in the parent conversation, under its own view, and the
/// answer reaches it; a request whose task is cancelled while it waits is withdrawn.
#[tokio::test]
#[serial]
async fn a_childs_permission_is_asked_through_the_parent() {
    let home = tempfile::tempdir().unwrap_or_else(|error| panic!("home: {error}"));
    let _env = Env::set(&[
        ("MOCK_AGENT_MCP_HTTP", "1".to_string()),
        (
            "MOCK_AGENT_MCP_CALLS_MOCK_MODEL",
            serde_json::json!([
                {"tool": "delegate_task", "arguments": {"task": "mock:request-permission", "agent": "mock-model-alt", "title": "Needs approval"}},
                {"tool": "delegate_task", "arguments": {"task": "mock:request-permission", "agent": "mock-model-alt", "title": "Cancelled while asking"}}
            ])
            .to_string(),
        ),
    ]);
    let config = NoriConfig {
        active_agent: "mock-model".to_string(),
        cwd: home.path().to_path_buf(),
        nori_home: home.path().to_path_buf(),
        approval_policy: nori_config::AskForApproval::OnRequest,
        ..Default::default()
    };
    let mut session = prepare_and_launch_session(
        AgentPrepareSpec {
            handoff: None,
            config: Arc::new(config),
            cli_version: "delegation-test".to_string(),
            session_context: None,
            initial_context: None,
        },
        SessionStart::New,
    );
    started(&mut session).await;
    prompt(&session, "DELEGATE with approvals").await;

    let is_child_request = |event: &SessionEvent| match event {
        SessionEvent::Acp(AcpEvent::Request {
            request: acp::AgentRequest::RequestPermissionRequest(permission),
            ..
        }) => permission.session_id.to_string().starts_with("nori-task-"),
        _ => false,
    };
    let events = collect_until(&mut session, Duration::from_secs(60), |events| {
        events
            .iter()
            .filter(|event| is_child_request(event))
            .count()
            >= 2
    })
    .await;
    let requests: Vec<acp::RequestId> = events
        .iter()
        .filter(|event| is_child_request(event))
        .filter_map(|event| match event {
            SessionEvent::Acp(AcpEvent::Request { request_id, .. }) => Some(request_id.clone()),
            _ => None,
        })
        .collect();

    session
        .handle
        .respond_to_agent(
            requests[0].clone(),
            Ok(acp::ClientResponse::RequestPermissionResponse(
                acp::RequestPermissionResponse::new(acp::RequestPermissionOutcome::Selected(
                    acp::SelectedPermissionOutcome::new("allow"),
                )),
            )),
        )
        .await
        .unwrap_or_else(|error| panic!("answer the child: {error:#}"));
    let answered = collect_until(&mut session, Duration::from_secs(60), |events| {
        child_text(events).contains("Permission granted with option: allow")
            && subagent_states(events).iter().any(|(_, state)| {
                matches!(
                    state,
                    SubagentState::Ended(SubagentOutcome::Completed { .. })
                )
            })
    })
    .await;
    assert!(!answered.is_empty());

    // Stopping the parent cancels the task still waiting for its answer, and withdraws it.
    session
        .handle
        .cancel()
        .await
        .unwrap_or_else(|error| panic!("cancel: {error:#}"));
    let events = collect_until(&mut session, Duration::from_secs(60), |events| {
        events.iter().any(|event| {
            matches!(event, SessionEvent::Nori(NoriEvent::RequestWithdrawn(id)) if *id == requests[1])
        })
    })
    .await;
    let states = subagent_states(&events);
    assert!(
        states
            .iter()
            .any(|(_, state)| matches!(state, SubagentState::Ended(SubagentOutcome::Cancelled))),
        "{states:?}"
    );
    session
        .handle
        .shutdown()
        .await
        .unwrap_or_else(|error| panic!("{error:#}"));
}

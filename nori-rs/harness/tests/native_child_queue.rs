use std::sync::Arc;
use std::time::Duration;

use nori_config::NoriConfig;
use nori_harness::runtime::AgentPrepareSpec;
use nori_harness::runtime::SessionStart;
use nori_harness::runtime::prepare_and_launch_session;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::SubagentWire;
use nori_protocol::acp;
use pretty_assertions::assert_eq;

fn content(text: &str) -> Vec<acp::v1::ContentBlock> {
    vec![acp::v1::ContentBlock::Text(acp::v1::TextContent::new(text))]
}

#[tokio::test]
async fn native_child_controls_are_ordered_and_target_the_child() {
    if std::env::var_os("NORI_NATIVE_CHILD_TEST_INNER").is_none() {
        let dir = tempfile::tempdir().expect("fixture directory");
        let fixture = dir.path().join("replay.jsonl");
        std::fs::write(&fixture, concat!(
            "{\"t_ms\":0,\"update\":{\"sessionUpdate\":\"subagent_update\",\"sessionId\":\"native-child\",\"state\":{\"state\":\"running\"}}}\n",
            "{\"t_ms\":10,\"session\":\"native-child\",\"update\":{\"sessionUpdate\":\"config_option_update\",\"configOptions\":[{\"id\":\"model\",\"name\":\"Model\",\"category\":\"model\",\"type\":\"select\",\"currentValue\":\"mock-model-default\",\"options\":[{\"value\":\"mock-model-default\",\"name\":\"Default\"}]}]}}\n",
            "{\"t_ms\":20,\"session\":\"native-child\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"historical child answer\"}}}\n",
            "{\"t_ms\":1200,\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"done\"}}}\n",
            "{\"t_ms\":1201,\"after_response\":true,\"update\":{\"sessionUpdate\":\"subagent_update\",\"sessionId\":\"native-child\",\"state\":{\"state\":\"idle\",\"stopReason\":\"end_turn\"}}}\n"
        )).expect("fixture");
        let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "native_child_controls_are_ordered_and_target_the_child",
                "--nocapture",
            ])
            .env("NORI_NATIVE_CHILD_TEST_INNER", "1")
            .env("MOCK_AGENT_SUPPORT_LOAD_SESSION", "1")
            .env("MOCK_AGENT_REPLAY_EXCEPT_TEXT", "after rewind")
            .env("MOCK_AGENT_FAIL_TEXT", "native failure")
            .env("MOCK_AGENT_WIRE_REPLAY", fixture)
            .env("MOCK_AGENT_PROMPT_LOG", dir.path().join("prompts"))
            .env("MOCK_AGENT_CANCEL_LOG", dir.path().join("cancels"))
            .status()
            .expect("run isolated fixture");
        assert!(status.success());
        return;
    }
    let home = tempfile::tempdir().expect("session directory");
    let config = NoriConfig {
        active_agent: "mock-model".into(),
        cwd: home.path().into(),
        nori_home: home.path().into(),
        ..Default::default()
    };
    let mut session = prepare_and_launch_session(
        AgentPrepareSpec {
            handoff: None,
            config: Arc::new(config),
            cli_version: "native-child-test".into(),
            session_context: None,
            initial_context: None,
        },
        SessionStart::New,
    );
    let started = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(SessionEvent::Nori(NoriEvent::SessionStarted(started))) =
                session.events.recv().await
            {
                break started;
            }
        }
    })
    .await
    .expect("session starts");
    session
        .handle
        .prompt(content("root"))
        .await
        .expect("root prompt");
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Update(update)))) =
                session.events.recv().await
                && update.session_id.to_string() == "native-child"
            {
                break;
            }
        }
    })
    .await
    .expect("child announced");
    let child = acp::v1::SessionId::new("native-child");
    let mut first = content("first");
    first.push(acp::v1::ContentBlock::Image(acp::v1::ImageContent::new(
        "cmV0YWluZWQ=",
        "image/png",
    )));
    session
        .handle
        .send_subagent_content(child.clone(), first)
        .await
        .expect("queue first");
    let log = std::env::var_os("MOCK_AGENT_PROMPT_LOG").expect("log path");
    async fn wait_for_log(path: &std::ffi::OsStr, count: usize) -> Vec<serde_json::Value> {
        tokio::time::timeout(Duration::from_secs(25), async {
            loop {
                let entries: Vec<serde_json::Value> = std::fs::read_to_string(path)
                    .unwrap_or_default()
                    .lines()
                    .filter_map(|line| serde_json::from_str(line).ok())
                    .collect();
                if entries.len() >= count {
                    break entries;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("prompt log")
    }
    wait_for_log(&log, 2).await;
    // The first turn's replayed answer streams 20 ms after its prompt. Steering cancels the turn,
    // so the answer must be in the child's history first: the response boundary the test rewinds
    // to is the end of an answer that follows the retained image.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let history = session
                .handle
                .child_history(child.clone())
                .await
                .expect("first turn history");
            let mut after_image = false;
            if history.iter().any(|event| {
                let text = serde_json::to_string(event).unwrap();
                after_image |= text.contains("cmV0YWluZWQ=");
                after_image && text.contains("historical child answer")
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("first turn answers after the retained image");
    session
        .handle
        .send_subagent_content(child.clone(), content("queued"))
        .await
        .expect("queue next");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(SessionEvent::Nori(NoriEvent::SubagentQueueChanged {
                session_id,
                prompts,
            })) = session.events.recv().await
                && session_id == child
                && prompts == vec!["queued".to_owned()]
            {
                break;
            }
        }
    })
    .await
    .expect("child-scoped queue update");
    let mut steering = content("steer");
    steering.push(acp::v1::ContentBlock::Image(acp::v1::ImageContent::new(
        "aW1hZ2U=",
        "image/png",
    )));
    session
        .handle
        .steer_subagent_content(child.clone(), steering)
        .await
        .expect("steer child");
    wait_for_log(&log, 3).await;
    session
        .handle
        .promote_subagent_queue(child.clone())
        .await
        .expect("promote child queue");
    let entries = wait_for_log(&log, 4).await;
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry["text"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["root", "first", "steer", "queued"]
    );
    assert_eq!(entries[2]["content"][1]["data"], "aW1hZ2U=");
    session
        .handle
        .cancel_subagent(child.clone())
        .await
        .expect("cancel focused child");
    let cancel_log = std::env::var_os("MOCK_AGENT_CANCEL_LOG").unwrap();
    let cancels = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let text = std::fs::read_to_string(&cancel_log).unwrap_or_default();
            if text.lines().count() >= 3 {
                break text;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("cancel log");
    assert_eq!(
        cancels.lines().collect::<Vec<_>>(),
        vec!["native-child", "native-child", "native-child"]
    );
    let history = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let history = session.handle.child_history(child.clone()).await.expect("native child history");
            if history.iter().any(|event| matches!(event,
                SessionEvent::Acp(nori_protocol::AcpEvent::Response { response: Ok(acp::v1::AgentResponse::PromptResponse(response)), .. })
                    if response.stop_reason == acp::v1::StopReason::Cancelled
            )) { break history; }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }).await.expect("cancelled child terminal receipt");
    session
        .handle
        .send_subagent_content(child.clone(), content("native failure"))
        .await
        .expect("failed child prompt admitted");
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let history = session
                .handle
                .child_history(child.clone())
                .await
                .expect("failed child history");
            if history.iter().any(|event| {
                matches!(
                    event,
                    SessionEvent::Acp(nori_protocol::AcpEvent::Response {
                        response: Err(_),
                        ..
                    })
                )
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("failed child terminal receipt");
    let mut saw_retained_image = false;
    let boundary = history
        .iter()
        .find_map(|event| {
            saw_retained_image |= serde_json::to_string(event)
                .unwrap()
                .contains("cmV0YWluZWQ=");
            match event {
                SessionEvent::Nori(NoriEvent::ConversationResponseBoundary(boundary))
                    if saw_retained_image =>
                {
                    Some((boundary.transcript_id.clone(), boundary.start, boundary.end))
                }
                _ => None,
            }
        })
        .expect("response boundary after retained image");
    let serialized = serde_json::to_string(&history).unwrap();
    assert!(serialized.contains("historical child answer"));
    assert!(
        serialized.contains("aW1hZ2U="),
        "typed child content is journaled"
    );
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            match session
                .handle
                .rewind_child_history(child.clone(), boundary.clone())
                .await
            {
                Ok(()) => break,
                Err(error) if error.to_string().contains("active or queued") => {
                    tokio::time::sleep(Duration::from_millis(50)).await
                }
                Err(error) => panic!("rewind child: {error:#}"),
            }
        }
    })
    .await
    .expect("rewind child");
    let after = serde_json::to_string(
        &session
            .handle
            .child_history(child.clone())
            .await
            .expect("rewound child history"),
    )
    .unwrap();
    assert!(after.contains("historical child answer"));
    assert!(!after.contains("aW1hZ2U="), "discarded future is absent");
    let rewound_history = session
        .handle
        .child_history(child.clone())
        .await
        .expect("rewound history");
    let rewound_boundary = rewound_history
        .iter()
        .find_map(|event| match event {
            SessionEvent::Nori(NoriEvent::ConversationResponseBoundary(boundary)) => {
                Some((boundary.transcript_id.clone(), boundary.start, boundary.end))
            }
            _ => None,
        })
        .expect("rewound child boundary");
    session
        .handle
        .fork_child_history(
            child.clone(),
            "untrusted presentation text".into(),
            rewound_boundary,
        )
        .await
        .expect("fork selected child history");
    let fork_id = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Update(update)))) =
                session.events.recv().await
                && update.parent_session_id == child
                && update.session_id != child
            {
                break update.session_id;
            }
        }
    })
    .await
    .expect("fork appears beneath source child");
    let forked = serde_json::to_string(
        &session
            .handle
            .child_history(fork_id.clone())
            .await
            .expect("fork history"),
    )
    .unwrap();
    assert!(forked.contains("historical child answer"));
    assert!(
        !forked.contains("aW1hZ2U="),
        "fork excludes later child content"
    );
    session
        .handle
        .send_subagent_content(child.clone(), content("after rewind"))
        .await
        .expect("same public child accepts work");
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let history = session
                .handle
                .child_history(child.clone())
                .await
                .expect("child history after rewind");
            let serialized = serde_json::to_string(&history).unwrap();
            if serialized.contains("after rewind")
                && history
                    .iter()
                    .filter(|event| {
                        matches!(
                            event,
                            SessionEvent::Nori(NoriEvent::ConversationResponseBoundary(_))
                        )
                    })
                    .count()
                    >= 2
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .expect("child completes after rewind");
    session.handle.shutdown().await.expect("shutdown");
    let project = nori_harness::transcript::compute_project_id(home.path())
        .await
        .unwrap()
        .id;
    let transcript = nori_harness::transcript::TranscriptLoader::new(home.path().to_path_buf())
        .load_transcript(
            &project,
            started.transcript_id.as_deref().expect("root journal id"),
        )
        .await
        .expect("load root journal");
    let config = NoriConfig {
        active_agent: "mock-model".into(),
        cwd: home.path().into(),
        nori_home: home.path().into(),
        ..Default::default()
    };
    let mut resumed = prepare_and_launch_session(
        AgentPrepareSpec {
            handoff: None,
            config: Arc::new(config),
            cli_version: "native-child-test".into(),
            session_context: None,
            initial_context: None,
        },
        SessionStart::Resume(nori_harness::runtime::SessionResume {
            acp_session_id: Some(started.acp_session_id.to_string()),
            transcript: Some(transcript),
        }),
    );
    let replay = tokio::time::timeout(Duration::from_secs(45), async {
        let mut events = Vec::new();
        let mut saw_child_history = false;
        loop {
            let event = resumed
                .events
                .recv()
                .await
                .unwrap_or_else(|| panic!("resume event stream closed: {events:#?}"));
            saw_child_history |= serde_json::to_string(&event)
                .unwrap()
                .contains("historical child answer");
            let done = matches!(event, SessionEvent::Nori(NoriEvent::ReplayFinished));
            events.push(event);
            if done && saw_child_history {
                break events;
            }
        }
    })
    .await
    .expect("root resumes and replays");
    let replay = serde_json::to_string(&replay).unwrap();
    assert!(
        !replay.contains("aW1hZ2U="),
        "root replay excludes discarded child future"
    );
    let restored = serde_json::to_string(
        &resumed
            .handle
            .child_history(child.clone())
            .await
            .expect("restored child history"),
    )
    .unwrap();
    assert!(restored.contains("historical child answer") && restored.contains("after rewind"));
    assert!(
        !restored.contains("aW1hZ2U="),
        "discarded future remains hidden after root restart"
    );
    let restored_fork = serde_json::to_string(
        &resumed
            .handle
            .child_history(fork_id.clone())
            .await
            .expect("managed fork survives root restart"),
    )
    .unwrap();
    assert!(restored_fork.contains("historical child answer"));
    assert!(!restored_fork.contains("aW1hZ2U="));
    resumed
        .handle
        .send_subagent_content(fork_id, content("fork after restart"))
        .await
        .expect("restored fork stays controllable");
    resumed
        .handle
        .send_subagent_content(child.clone(), content("after restart"))
        .await
        .expect("restored public child remains controllable");
    let restored_prompts = || {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .collect::<Vec<_>>()
    };
    let entries = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let entries = restored_prompts();
            if entries.iter().any(|entry| {
                entry["text"].as_str().is_some_and(|text| {
                    text.ends_with("after restart") && !text.ends_with("fork after restart")
                })
            }) {
                break entries;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|error| {
        let events = std::iter::from_fn(|| resumed.events.try_recv().ok()).collect::<Vec<_>>();
        panic!(
            "restored provider receives prompt: {error}; prompts: {:#?}; events: {events:#?}",
            restored_prompts()
        )
    });
    let provider_prompt = entries
        .iter()
        .find(|entry| {
            entry["text"].as_str().is_some_and(|text| {
                text.ends_with("after restart") && !text.ends_with("fork after restart")
            })
        })
        .unwrap();
    assert_ne!(
        provider_prompt["session"], "native-child",
        "old provider session is not restored"
    );
    let provider_context = provider_prompt["text"].as_str().unwrap();
    assert!(
        provider_context.contains("historical child answer")
            && provider_context.contains("after rewind")
    );
    assert!(
        !provider_context.contains("aW1hZ2U="),
        "provider receives no discarded future"
    );
    assert!(
        provider_prompt["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|part| part["type"] == "image" && part["data"] == "cmV0YWluZWQ="),
        "selected historical image remains an ACP image block"
    );
    resumed
        .handle
        .shutdown()
        .await
        .expect("shutdown resumed root");
}

#[tokio::test]
async fn loaded_native_child_accepts_work_without_a_new_announcement() {
    if std::env::var_os("NORI_NATIVE_CHILD_RESUME_INNER").is_none() {
        let dir = tempfile::tempdir().expect("fixture directory");
        let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "loaded_native_child_accepts_work_without_a_new_announcement",
                "--nocapture",
            ])
            .env("NORI_NATIVE_CHILD_RESUME_INNER", "1")
            .env("MOCK_AGENT_SUPPORT_LOAD_SESSION", "1")
            .env("MOCK_AGENT_LOAD_NATIVE_CHILD", "1")
            .env("MOCK_AGENT_DELAY_MS", "2000")
            .env("MOCK_AGENT_PROMPT_LOG", dir.path().join("prompts"))
            .env("MOCK_AGENT_CANCEL_LOG", dir.path().join("cancels"))
            .status()
            .expect("run isolated fixture");
        assert!(status.success());
        return;
    }
    let home = tempfile::tempdir().expect("session directory");
    let config = NoriConfig {
        active_agent: "mock-model".into(),
        cwd: home.path().into(),
        nori_home: home.path().into(),
        ..Default::default()
    };
    let mut session = prepare_and_launch_session(
        AgentPrepareSpec {
            handoff: None,
            config: Arc::new(config),
            cli_version: "native-child-resume-test".into(),
            session_context: None,
            initial_context: None,
        },
        SessionStart::Resume(nori_harness::runtime::SessionResume {
            acp_session_id: Some("loaded-parent".into()),
            transcript: None,
        }),
    );
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if matches!(
                session.events.recv().await,
                Some(SessionEvent::Nori(NoriEvent::SessionStarted(_)))
            ) {
                break;
            }
        }
    })
    .await
    .expect("session resumes");
    let child = acp::v1::SessionId::new("resumed-child");
    let root_config = session
        .handle
        .get_session_config()
        .await
        .expect("root config");
    let child_config = session
        .handle
        .child_config_options(child.clone())
        .await
        .expect("loaded child config");
    assert!(!child_config.is_empty());
    let updated = session
        .handle
        .set_child_config_option(child.clone(), "model".into(), "mock-model-fast".into())
        .await
        .expect("set child model");
    let model = updated
        .iter()
        .find(|option| option.id.to_string() == "model")
        .expect("child model");
    assert!(
        matches!(&model.kind, acp::v1::SessionConfigKind::Select(select) if select.current_value.to_string() == "mock-model-fast")
    );
    assert_eq!(
        session
            .handle
            .get_session_config()
            .await
            .expect("root config after child update"),
        root_config
    );
    session
        .handle
        .send_subagent_content(child.clone(), content("first"))
        .await
        .expect("start loaded child");
    let log = std::env::var_os("MOCK_AGENT_PROMPT_LOG").unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if std::fs::read_to_string(&log)
                .unwrap_or_default()
                .contains("first")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("loaded child starts");
    session
        .handle
        .send_subagent_content(child.clone(), content("queued"))
        .await
        .expect("queue loaded child");
    session
        .handle
        .cancel_subagent(child)
        .await
        .expect("cancel loaded child");
    let cancel_log = std::env::var_os("MOCK_AGENT_CANCEL_LOG").unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if std::fs::read_to_string(&cancel_log)
                .unwrap_or_default()
                .lines()
                .any(|line| line == "resumed-child")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("cancel reaches loaded child");
    session.handle.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn transcript_child_is_controllable_without_provider_reannouncement() {
    if std::env::var_os("NORI_NATIVE_CHILD_HISTORY_INNER").is_none() {
        let dir = tempfile::tempdir().expect("fixture directory");
        let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "transcript_child_is_controllable_without_provider_reannouncement",
                "--nocapture",
            ])
            .env("NORI_NATIVE_CHILD_HISTORY_INNER", "1")
            .env_remove("MOCK_AGENT_WIRE_REPLAY")
            .env_remove("MOCK_AGENT_LOAD_NATIVE_CHILD")
            .env("MOCK_AGENT_SUPPORT_LOAD_SESSION", "1")
            .env("MOCK_AGENT_DELAY_MS", "2000")
            .env("MOCK_AGENT_PROMPT_LOG", dir.path().join("prompts"))
            .env("MOCK_AGENT_CANCEL_LOG", dir.path().join("cancels"))
            .status()
            .expect("run isolated fixture");
        assert!(status.success());
        return;
    }
    use nori_harness::transcript::TranscriptLoader;
    use nori_harness::transcript::TranscriptRecorder;
    use nori_protocol::Patch;
    use nori_protocol::SubagentState;
    use nori_protocol::SubagentUpdate;
    let home = tempfile::tempdir().expect("session directory");
    let recorder = TranscriptRecorder::new(
        home.path(),
        home.path(),
        Some("mock-model".into()),
        "native-child-test",
        Some("loaded-parent".into()),
    )
    .await
    .expect("recorder");
    recorder.shutdown().await.expect("flush");
    let path = recorder.transcript_path().to_path_buf();
    let child_update =
        SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Update(SubagentUpdate {
            parent_session_id: acp::v1::SessionId::new("loaded-parent"),
            session_id: acp::v1::SessionId::new("historic-child"),
            agent: Patch::Keep,
            model: Patch::Keep,
            name: Patch::Keep,
            title: Patch::Set("Historical child".into()),
            description: Patch::Keep,
            state: Patch::Set(SubagentState::Running),
        })));
    let line = serde_json::json!({"ts":"2026-01-01T00:00:00Z", "v":3, "type":"session_event", "event":child_update});
    let mut log = nori_conversation::ConversationLog::open(path.with_extension("pkl"))
        .expect("open canonical fixture");
    log.append(nori_conversation::Event::Operation {
        name: "transcript-entry".into(),
        args: line.to_string(),
    })
    .expect("record historical child");
    drop(log);
    let transcript = TranscriptLoader::new(home.path().to_path_buf())
        .load_transcript(recorder.project_id(), recorder.session_id())
        .await
        .expect("load fixture");
    let config = NoriConfig {
        active_agent: "mock-model".into(),
        cwd: home.path().into(),
        nori_home: home.path().into(),
        ..Default::default()
    };
    let mut session = prepare_and_launch_session(
        AgentPrepareSpec {
            handoff: None,
            config: Arc::new(config),
            cli_version: "native-child-test".into(),
            session_context: None,
            initial_context: None,
        },
        SessionStart::Resume(nori_harness::runtime::SessionResume {
            acp_session_id: Some("loaded-parent".into()),
            transcript: Some(transcript),
        }),
    );
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if matches!(
                session.events.recv().await,
                Some(SessionEvent::Nori(NoriEvent::SessionStarted(_)))
            ) {
                break;
            }
        }
    })
    .await
    .expect("session resumes");
    let child = acp::v1::SessionId::new("historic-child");
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Update(update)))) =
                session.events.recv().await
                && update.session_id == child
            {
                break;
            }
        }
    })
    .await
    .expect("historical child appears in replay");
    session
        .handle
        .send_subagent_content(child.clone(), content("first"))
        .await
        .expect("start historical child");
    let log = std::env::var_os("MOCK_AGENT_PROMPT_LOG").unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if std::fs::read_to_string(&log)
                .unwrap_or_default()
                .contains("first")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("historical child starts");
    session
        .handle
        .send_subagent_content(child.clone(), content("queued"))
        .await
        .expect("queue historical child");
    session
        .handle
        .cancel_subagent(child)
        .await
        .expect("cancel historical child");
    let cancel_log = std::env::var_os("MOCK_AGENT_CANCEL_LOG").unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if std::fs::read_to_string(&cancel_log)
                .unwrap_or_default()
                .lines()
                .any(|line| line == "historic-child")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("cancel reaches historical child");
    session.handle.shutdown().await.expect("shutdown");
}

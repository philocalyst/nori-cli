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
use nori_harness::transcript::Transcript;
use nori_harness::transcript::TranscriptLoader;
use nori_harness::transcript::TranscriptRecord;
use nori_protocol::NoriEvent;
use nori_protocol::Patch;
use nori_protocol::SessionEvent;
use nori_protocol::SubagentState;
use nori_protocol::SubagentWire;
use nori_protocol::acp;
use pretty_assertions::assert_eq;

async fn launch(home: &Path) -> LaunchedSession {
    let config = NoriConfig {
        active_agent: "mock-model".to_owned(),
        cwd: home.to_path_buf(),
        nori_home: home.to_path_buf(),
        acp_proxy: nori_config::AcpProxyConfig {
            enabled: true,
            log_dir: home.join("acp-wire"),
        },
        ..Default::default()
    };
    prepare_and_launch_session(
        AgentPrepareSpec {
            handoff: None,
            config: Arc::new(config),
            cli_version: "history-fork-test".to_owned(),
            session_context: None,
            initial_context: None,
        },
        SessionStart::New,
    )
}

#[expect(clippy::expect_used, reason = "focused failure for a missing session")]
async fn wait_started(session: &mut LaunchedSession) -> String {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match session.events.recv().await.expect("event stream open") {
                SessionEvent::Nori(NoriEvent::SessionStarted(started)) => {
                    return started.transcript_id.expect("transcript id");
                }
                SessionEvent::Nori(NoriEvent::RequestFailed(failure)) => {
                    panic!("session startup failed: {failure:?}");
                }
                SessionEvent::Acp(_) | SessionEvent::Nori(_) => {}
            }
        }
    })
    .await
    .expect("session should start")
}

#[expect(clippy::expect_used, reason = "focused failure for a stalled prompt")]
async fn prompt(session: &LaunchedSession, text: &str) -> acp::v1::RequestId {
    session
        .handle
        .prompt(vec![acp::v1::ContentBlock::Text(
            acp::v1::TextContent::new(text),
        )])
        .await
        .expect("submit prompt")
}

#[expect(clippy::expect_used, reason = "focused failure for a stalled prompt")]
async fn wait_prompt(session: &mut LaunchedSession, request_id: &acp::v1::RequestId) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match session.events.recv().await.expect("event stream open") {
                SessionEvent::Acp(nori_protocol::AcpEvent::Response {
                    request_id: response_id,
                    response: Ok(acp::v1::AgentResponse::PromptResponse(_)),
                }) if &response_id == request_id => return,
                SessionEvent::Nori(NoriEvent::RequestFailed(failure)) => {
                    panic!("prompt failed: {failure:?}");
                }
                SessionEvent::Acp(_) | SessionEvent::Nori(_) => {}
            }
        }
    })
    .await
    .expect("prompt should finish")
}

#[expect(
    clippy::expect_used,
    reason = "focused failure for a missing child completion"
)]
async fn wait_child_end(session: &mut LaunchedSession) -> acp::v1::SessionId {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match session.events.recv().await.expect("event stream open") {
                SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Update(update)))
                    if matches!(
                        update.state,
                        Patch::Set(SubagentState::Idle { .. } | SubagentState::Ended(_))
                    ) =>
                {
                    return update.session_id;
                }
                SessionEvent::Nori(NoriEvent::RequestFailed(failure)) => {
                    panic!("history fork failed: {failure:?}");
                }
                SessionEvent::Acp(_) | SessionEvent::Nori(_) => {}
            }
        }
    })
    .await
    .expect("history-fork child should complete")
}

fn user_contents(transcript: &Transcript) -> Vec<String> {
    transcript
        .records()
        .filter_map(|record| match record {
            TranscriptRecord::User { content } => Some(content.to_owned()),
            TranscriptRecord::Assistant { .. }
            | TranscriptRecord::Thinking { .. }
            | TranscriptRecord::SessionEvent(_) => None,
        })
        .collect()
}

#[tokio::test]
async fn history_fork_has_a_child_journal_with_source_and_replay_without_duplicate_users() {
    let home = tempfile::tempdir().expect("create session directory");
    let mut parent = launch(home.path()).await;
    let parent_id = wait_started(&mut parent).await;
    let prompt_id = prompt(&parent, "parent-only history").await;
    wait_prompt(&mut parent, &prompt_id).await;
    parent
        .handle
        .flush_transcript()
        .await
        .expect("flush parent");

    let loader = TranscriptLoader::new(home.path().to_path_buf());
    let parent_meta = loader
        .find_session_metadata_by_id(&parent_id)
        .await
        .expect("find parent")
        .expect("parent metadata");
    let parent_path = loader.session_path(&parent_meta.project_id, &parent_id);
    let parent_log =
        ConversationLog::open(parent_path.with_extension("pkl")).expect("open parent journal");
    let source = parent_log.visible_ids();
    let start = *source.first().expect("source has records");
    let end = *source.last().expect("source has records");

    parent
        .handle
        .fork_history(
            "this is replaced by the selected event range".to_owned(),
            Some((parent_id.clone(), start, end)),
        )
        .await
        .expect("start history fork");
    let (child_view, child_updates) = tokio::time::timeout(Duration::from_secs(30), async {
        let mut updates = Vec::new();
        loop {
            match parent.events.recv().await.expect("parent event stream") {
                SessionEvent::Acp(nori_protocol::AcpEvent::ChildNotification(update)) => {
                    updates.push(serde_json::to_string(&update).expect("serialize child update"));
                }
                SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Update(update)))
                    if matches!(
                        update.state,
                        Patch::Set(SubagentState::Idle { .. } | SubagentState::Ended(_))
                    ) =>
                {
                    break (update.session_id, updates);
                }
                SessionEvent::Acp(_) | SessionEvent::Nori(_) => {}
            }
        }
    })
    .await
    .expect("child completes");
    assert!(
        child_updates
            .iter()
            .any(|update| update.contains("parent-only history")),
        "inherited history must be visible in the child view"
    );
    let original_parent_config = parent.handle.get_session_config().await;
    let child = parent
        .handle
        .controlled_subagent_handle(child_view)
        .await
        .expect("resolve child control plane")
        .expect("fork owns a harness");
    let child_recorder = child
        .conversation_recorder()
        .await
        .expect("read child recorder")
        .expect("child has a journal");
    assert_ne!(child_recorder.session_id(), parent_id);
    child
        .set_session_config_option("model".to_owned(), "mock-model-fast".to_owned())
        .await
        .expect("set the child model independently");
    assert_eq!(
        parent.handle.get_session_config().await,
        original_parent_config
    );
    assert!(
        parent
            .handle
            .controlled_subagent_handle("uncontrolled-provider-child".into())
            .await
            .expect("query uncontrolled child")
            .is_none()
    );
    parent
        .handle
        .flush_transcript()
        .await
        .expect("flush parent after fork");
    parent.handle.shutdown().await.expect("shutdown parent");

    let sessions = loader
        .list_sessions(&parent_meta.project_id)
        .await
        .expect("list session transcripts");
    let child_id = sessions
        .iter()
        .filter(|session| session.session_id != parent_id)
        .find_map(|session| {
            let path = loader.session_path(&parent_meta.project_id, &session.session_id);
            let log = ConversationLog::open(path.with_extension("pkl")).ok()?;
            log.visible()
                .map(Result::unwrap)
                .any(|record| {
                    matches!(&record.event, Event::Command { name, .. } if name == "fork-source")
                })
                .then(|| session.session_id.clone())
        })
        .expect("fork child journal with source marker");
    let child_path = loader.session_path(&parent_meta.project_id, &child_id);
    let child_log =
        ConversationLog::open(child_path.with_extension("pkl")).expect("open child journal");
    let source_payload = child_log
        .visible()
        .map(Result::unwrap)
        .find_map(|record| match &record.event {
            Event::Command { name, payload } if name == "fork-source" => Some(payload.clone()),
            _ => None,
        })
        .expect("fork source command");
    let source_payload: serde_json::Value =
        serde_json::from_str(&source_payload).expect("source payload JSON");
    assert_eq!(source_payload["session_id"], parent_id);
    assert_eq!(source_payload["start"], start);
    assert_eq!(source_payload["end"], end);

    let parent_transcript = loader
        .load_transcript(&parent_meta.project_id, &parent_id)
        .await
        .expect("load parent replay");
    let child_transcript = loader
        .load_transcript(&parent_meta.project_id, &child_id)
        .await
        .expect("load child replay");
    assert_eq!(user_contents(&parent_transcript), ["parent-only history"]);
    let child_users = user_contents(&child_transcript);
    assert_eq!(
        child_users,
        [
            "parent-only history",
            "Continue from the conversation above."
        ]
    );

    let replay = nori_harness::backend::transcript_to_replay_session_events(&child_transcript);
    let replay_user_texts: Vec<_> = replay
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
                acp::v1::AgentNotification::SessionNotification(notification),
            )) => match &notification.update {
                acp::v1::SessionUpdate::UserMessageChunk(chunk) => match &chunk.content {
                    acp::v1::ContentBlock::Text(text) => Some(text.text.as_str()),
                    _ => None,
                },
                _ => None,
            },
            SessionEvent::Acp(_) | SessionEvent::Nori(_) => None,
        })
        .collect();
    assert_eq!(
        replay_user_texts,
        [
            "parent-only history",
            "Continue from the conversation above."
        ]
    );
}

#[tokio::test]
async fn session_start_fails_when_its_canonical_journal_cannot_be_created() {
    let cwd = tempfile::tempdir().expect("create cwd");
    let invalid_home = cwd.path().join("nori-home-file");
    std::fs::write(&invalid_home, b"not a directory").expect("create invalid home");
    let config = NoriConfig {
        active_agent: "mock-model".to_owned(),
        cwd: cwd.path().to_path_buf(),
        nori_home: invalid_home,
        ..Default::default()
    };
    let mut session = prepare_and_launch_session(
        AgentPrepareSpec {
            handoff: None,
            config: Arc::new(config),
            cli_version: "history-fork-test".to_owned(),
            session_context: None,
            initial_context: None,
        },
        SessionStart::New,
    );

    let failure = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match session.events.recv().await.expect("event stream open") {
                SessionEvent::Nori(NoriEvent::RequestFailed(failure)) => {
                    return failure.message;
                }
                SessionEvent::Nori(NoriEvent::SessionStarted(_)) => {
                    panic!("session started without a canonical transcript");
                }
                SessionEvent::Acp(_) | SessionEvent::Nori(_) => {}
            }
        }
    })
    .await
    .expect("startup should fail");
    assert!(
        failure.contains("failed to initialize canonical transcript"),
        "{failure}"
    );
}

#[tokio::test]
async fn rewind_waits_for_a_running_child_but_allows_completed_forks() {
    if std::env::var_os("NORI_REWIND_CHILD_TEST_INNER").is_none() {
        let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "rewind_waits_for_a_running_child_but_allows_completed_forks",
                "--nocapture",
            ])
            .env("NORI_REWIND_CHILD_TEST_INNER", "1")
            .env("MOCK_AGENT_DELAY_MS", "1000")
            .status()
            .expect("run isolated child fixture");
        assert!(status.success(), "isolated child test failed");
        return;
    }
    let home = tempfile::tempdir().expect("session directory");
    let mut parent = launch(home.path()).await;
    wait_started(&mut parent).await;
    let first = prompt(&parent, "stable turn").await;
    wait_prompt(&mut parent, &first).await;
    let recorder = parent
        .handle
        .conversation_recorder()
        .await
        .expect("recorder request")
        .expect("journal");
    let replay = recorder
        .replay_session_events()
        .await
        .expect("source replay");
    let boundary = replay
        .iter()
        .rev()
        .find_map(|event| match event {
            SessionEvent::Nori(NoriEvent::ConversationResponseBoundary(boundary)) => {
                Some(boundary.clone())
            }
            _ => None,
        })
        .expect("response boundary");
    parent
        .handle
        .fork_history("context".to_owned(), None)
        .await
        .expect("start child");
    let error = parent
        .handle
        .rewind_and_rebind(boundary.transcript_id.clone(), Some(boundary.end))
        .await
        .expect_err("running child must block rewind");
    assert!(error.to_string().contains("delegated work"), "{error}");
    wait_child_end(&mut parent).await;
    parent
        .handle
        .rewind_and_rebind(boundary.transcript_id, Some(boundary.end))
        .await
        .expect("completed history fork must not block rewind");
    parent.handle.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn child_response_actions_use_its_journal_and_leave_parent_history_alone() {
    if std::env::var_os("NORI_FORK_HISTORY_INNER").is_none() {
        let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "child_response_actions_use_its_journal_and_leave_parent_history_alone",
                "--nocapture",
            ])
            .env("NORI_FORK_HISTORY_INNER", "1")
            .env("MOCK_AGENT_GENERIC_TOOL_CALL", "1")
            .status()
            .expect("run isolated tool fixture");
        assert!(status.success(), "isolated child history test failed");
        return;
    }
    let home = tempfile::tempdir().expect("create session directory");
    let mut parent = launch(home.path()).await;
    let _ = wait_started(&mut parent).await;
    let prompt_id = prompt(&parent, "parent history").await;
    wait_prompt(&mut parent, &prompt_id).await;
    let prompt_id = prompt(&parent, "discarded parent future").await;
    wait_prompt(&mut parent, &prompt_id).await;
    let source_recorder = parent
        .handle
        .conversation_recorder()
        .await
        .expect("parent recorder")
        .expect("parent journal");
    let source_events = source_recorder
        .replay_session_events()
        .await
        .expect("parent source replay");
    let parent_boundary = source_events
        .iter()
        .rev()
        .find_map(|event| match event {
            SessionEvent::Nori(NoriEvent::ConversationResponseBoundary(boundary)) => {
                Some(boundary.clone())
            }
            _ => None,
        })
        .expect("parent response source");
    parent
        .handle
        .fork_history(
            "parent context".to_owned(),
            Some((
                parent_boundary.transcript_id.clone(),
                parent_boundary.start,
                parent_boundary.end,
            )),
        )
        .await
        .expect("start child");
    let child_view = wait_child_end(&mut parent).await;
    let child = parent
        .handle
        .controlled_subagent_handle(child_view.clone())
        .await
        .expect("resolve child")
        .expect("controlled child");
    let recorder = child
        .conversation_recorder()
        .await
        .expect("recorder request")
        .expect("child journal");
    let replay = recorder
        .replay_session_events()
        .await
        .expect("read child replay");
    assert!(
        serde_json::to_string(&replay)
            .expect("serialize replay")
            .contains("parent history")
    );
    let tool_ids: Vec<_> = replay
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
                acp::v1::AgentNotification::SessionNotification(notification),
            )) => match &notification.update {
                acp::v1::SessionUpdate::ToolCall(call) => Some(call.tool_call_id.to_string()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(tool_ids.len(), 3);
    assert_ne!(
        tool_ids[0], tool_ids[1],
        "reused source tool IDs must remain distinct"
    );
    assert_ne!(
        tool_ids[1], tool_ids[2],
        "seed and child tool IDs must remain distinct"
    );
    parent
        .handle
        .rewind_and_rebind(parent_boundary.transcript_id, None)
        .await
        .expect("rewind parent independently");
    assert!(
        recorder
            .visible_context()
            .await
            .expect("child context after parent rewind")
            .contains("parent history")
    );
    let boundary = replay
        .iter()
        .find_map(|event| match event {
            SessionEvent::Nori(NoriEvent::ConversationResponseBoundary(boundary)) => {
                Some(boundary.clone())
            }
            _ => None,
        })
        .expect("child response source");
    assert_eq!(boundary.transcript_id, recorder.session_id());
    child
        .set_session_config_option("model".to_owned(), "mock-model-fast".to_owned())
        .await
        .expect("set child model");
    let child_config = child.get_session_config().await;
    parent
        .handle
        .send_subagent_message(child_view.clone(), "later child turn".to_owned())
        .await
        .expect("send follow-up");
    let child_view = wait_child_end(&mut parent).await;
    let parent_recorder = parent
        .handle
        .conversation_recorder()
        .await
        .expect("parent recorder request")
        .expect("parent journal");
    let before = serde_json::to_value(
        parent_recorder
            .replay_session_events()
            .await
            .expect("parent replay"),
    )
    .expect("serialize parent");
    child
        .rewind_and_rebind(boundary.transcript_id.clone(), Some(boundary.end))
        .await
        .expect("rewind child");
    assert_eq!(child.get_session_config().await, child_config);
    assert!(
        recorder
            .visible_context()
            .await
            .expect("rewound context")
            .contains("parent history")
    );
    assert!(
        !recorder
            .visible_context()
            .await
            .expect("rewound context")
            .contains("discarded parent future")
    );
    let after = serde_json::to_value(
        parent_recorder
            .replay_session_events()
            .await
            .expect("parent replay after rewind"),
    )
    .expect("serialize parent");
    assert_eq!(after, before);
    let child_replay = serde_json::to_string(
        &recorder
            .replay_session_events()
            .await
            .expect("rewound replay"),
    )
    .expect("serialize child");
    assert!(!child_replay.contains("later child turn"));
    parent
        .handle
        .send_subagent_message(child_view.clone(), "after child rewind".to_owned())
        .await
        .expect("continue rewound child");
    let child_view = wait_child_end(&mut parent).await;
    let prompts: Vec<_> = std::fs::read_dir(home.path().join("acp-wire"))
        .expect("wire logs")
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .flat_map(|log| {
            log.lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .collect::<Vec<_>>()
        })
        .filter(|entry| {
            entry["direction"] == "client_to_agent"
                && entry["message"]["method"] == "session/prompt"
        })
        .map(|entry| entry["message"]["params"].to_string())
        .collect();
    let continued = prompts
        .iter()
        .find(|prompt| prompt.contains("after child rewind"))
        .expect("rewound child prompt");
    assert!(continued.contains("parent history"), "{continued}");
    assert!(continued.contains("command output here"), "{continued}");
    assert!(
        !continued.contains("discarded parent future"),
        "{continued}"
    );
    assert_eq!(
        continued.matches("parent history").count(),
        1,
        "{continued}"
    );
    let child_log = ConversationLog::open(recorder.transcript_path().with_extension("pkl"))
        .expect("child journal");
    assert!(child_log.all().map(Result::unwrap).any(|record| matches!(&record.event,
        Event::Command { name, payload } if name == "fork-context" && payload.contains("discarded parent future")
    )), "discarded source events remain archived");
    child
        .fork_history(
            "child context".to_owned(),
            Some((boundary.transcript_id.clone(), boundary.start, boundary.end)),
        )
        .await
        .expect("fork child response");
    let grandchild_view = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match parent.events.recv().await.expect("parent event stream") {
                SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Update(update)))
                    if matches!(
                        update.state,
                        Patch::Set(SubagentState::Idle { .. } | SubagentState::Ended(_))
                    ) =>
                {
                    assert_eq!(update.parent_session_id, child_view);
                    break update.session_id;
                }
                SessionEvent::Acp(_) | SessionEvent::Nori(_) => {}
            }
        }
    })
    .await
    .expect("grandchild should complete");
    assert!(
        parent
            .handle
            .controlled_subagent_handle(grandchild_view.clone())
            .await
            .expect("resolve grandchild")
            .is_some()
    );
    parent
        .handle
        .send_subagent_message(grandchild_view.clone(), "grandchild follow-up".to_owned())
        .await
        .expect("send grandchild follow-up");
    wait_child_end(&mut parent).await;
    parent
        .handle
        .promote_subagent_queue(grandchild_view.clone())
        .await
        .expect("promote grandchild queue");
    let nested_content = vec![
        acp::v1::ContentBlock::Text(acp::v1::TextContent::new("nested steering payload")),
        acp::v1::ContentBlock::Image(acp::v1::ImageContent::new("aW1hZ2U=", "image/jpeg")),
    ];
    parent
        .handle
        .steer_subagent_content(grandchild_view.clone(), nested_content.clone())
        .await
        .expect("steer grandchild");
    let expected = serde_json::to_value(&nested_content).expect("serialize nested content");
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let found = std::fs::read_dir(home.path().join("acp-wire"))
                .expect("wire logs")
                .filter_map(Result::ok)
                .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
                .flat_map(|log| {
                    log.lines()
                        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                        .collect::<Vec<_>>()
                })
                .any(|entry| {
                    entry["direction"] == "client_to_agent"
                        && entry["message"]["method"] == "session/prompt"
                        && entry["message"]["params"]["prompt"] == expected
                });
            if found {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("nested typed steering reached provider");
    parent
        .handle
        .cancel_subagent(grandchild_view)
        .await
        .expect("cancel grandchild");
    let loader = TranscriptLoader::new(home.path().to_path_buf());
    let sessions = loader
        .list_sessions(recorder.project_id())
        .await
        .expect("list journals");
    let source_found = sessions
        .iter()
        .filter(|session| session.session_id != recorder.session_id())
        .any(|session| {
            let path = loader.session_path(recorder.project_id(), &session.session_id);
            ConversationLog::open(path.with_extension("pkl"))
                .ok()
                .is_some_and(|log| {
                    log.visible()
                        .map(Result::unwrap)
                        .any(|record| match &record.event {
                            Event::Command { name, payload } if name == "fork-source" => {
                                serde_json::from_str::<serde_json::Value>(payload)
                                    .ok()
                                    .is_some_and(|value| {
                                        value["session_id"] == boundary.transcript_id
                                            && value["start"] == boundary.start
                                            && value["end"] == boundary.end
                                    })
                            }
                            _ => false,
                        })
                })
        });
    assert!(
        source_found,
        "child fork should retain the exact source range"
    );
    parent.handle.shutdown().await.expect("shutdown parent");
}

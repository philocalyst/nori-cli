//! ACP questions remain interactive under the automatic approval policy, including child relays.
use nori_config::AskForApproval;
use nori_config::NoriConfig;
use nori_harness::runtime::AgentPrepareSpec;
use nori_harness::runtime::SessionLaunchSpec;
use nori_harness::runtime::SessionStart;
use nori_harness::runtime::launch_session;
use nori_harness::runtime::prepare_agent;
use nori_protocol::AcpEvent;
use nori_protocol::NoriEvent;
use nori_protocol::Patch;
use nori_protocol::SessionEvent;
use nori_protocol::SubagentState;
use nori_protocol::SubagentWire;
use nori_protocol::acp::v1 as acp;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn questions_round_trip_for_parent_and_namespaced_child_under_yolo() {
    let home = tempfile::tempdir().unwrap();
    let config = NoriConfig {
        active_agent: "mock-model".into(),
        cwd: home.path().into(),
        nori_home: home.path().into(),
        approval_policy: AskForApproval::Never,
        ..Default::default()
    };
    let agent = prepare_agent(AgentPrepareSpec {
        handoff: None,
        config: Arc::new(config),
        cli_version: "question-test".into(),
        session_context: None,
        initial_context: None,
    })
    .await
    .unwrap();
    let mut session = launch_session(SessionLaunchSpec {
        agent,
        start: SessionStart::New,
    });
    let root = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let SessionEvent::Nori(NoriEvent::SessionStarted(started)) =
                session.events.recv().await.unwrap()
            {
                break started.acp_session_id;
            }
        }
    })
    .await
    .unwrap();
    session
        .handle
        .prompt(vec![acp::ContentBlock::Text(acp::TextContent::new(
            "fixture:user-question",
        ))])
        .await
        .unwrap();
    let (id, params) = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let SessionEvent::Acp(AcpEvent::Request {
                request_id,
                request: acp::AgentRequest::ExtMethodRequest(request),
            }) = session.events.recv().await.unwrap()
            {
                break (
                    request_id,
                    serde_json::from_str::<serde_json::Value>(request.params.get()).unwrap(),
                );
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(params["sessionId"], root.to_string());
    let queued_handle = session.handle.clone();
    let queued = tokio::spawn(async move {
        queued_handle
            .prompt(vec![acp::ContentBlock::Text(acp::TextContent::new(
                "ordinary queued prompt",
            ))])
            .await
    });
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let SessionEvent::Nori(NoriEvent::QueueChanged(queue)) =
                session.events.recv().await.unwrap()
                && queue
                    .prompts
                    .iter()
                    .any(|prompt| prompt.contains("ordinary queued prompt"))
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    let answer = serde_json::json!({"answers":{"choice":{"answers":["yes"]}}});
    session
        .handle
        .respond_to_agent(
            id,
            Ok(acp::ClientResponse::ExtMethodResponse(
                acp::ExtResponse::new(Arc::from(serde_json::value::to_raw_value(&answer).unwrap())),
            )),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let SessionEvent::Acp(AcpEvent::Response {
                response: Ok(acp::AgentResponse::PromptResponse(_)),
                ..
            }) = session.events.recv().await.unwrap()
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    let queued_id = tokio::time::timeout(Duration::from_secs(30), queued)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let SessionEvent::Acp(AcpEvent::Response {
                request_id,
                response: Ok(acp::AgentResponse::PromptResponse(_)),
            }) = session.events.recv().await.unwrap()
                && request_id == queued_id
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    session
        .handle
        .prompt(vec![acp::ContentBlock::Text(acp::TextContent::new(
            "fixture:withdraw-user-question",
        ))])
        .await
        .unwrap();
    let mut withdrawn = None;
    let mut retained = None;
    let mut withdrawal = None;
    tokio::time::timeout(Duration::from_secs(30), async {
        while withdrawn.is_none() || retained.is_none() || withdrawal.is_none() {
            match session.events.recv().await.unwrap() {
                SessionEvent::Acp(AcpEvent::Request {
                    request_id,
                    request: acp::AgentRequest::ExtMethodRequest(request),
                }) => {
                    let params: serde_json::Value =
                        serde_json::from_str(request.params.get()).unwrap();
                    match params["callId"].as_str().unwrap() {
                        "withdrawn" => withdrawn = Some(request_id),
                        "retained" => retained = Some(request_id),
                        other => panic!("unexpected question: {other}"),
                    }
                }
                SessionEvent::Nori(NoriEvent::RequestWithdrawn(request_id)) => {
                    withdrawal = Some(request_id)
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(withdrawn, withdrawal);
    let response = || {
        Ok(acp::ClientResponse::ExtMethodResponse(
            acp::ExtResponse::new(Arc::from(
                serde_json::value::to_raw_value(
                    &serde_json::json!({"answers":{"choice":{"answers":["yes"]}}}),
                )
                .unwrap(),
            )),
        ))
    };
    assert!(
        session
            .handle
            .respond_to_agent(withdrawn.unwrap(), response())
            .await
            .is_err()
    );
    session
        .handle
        .respond_to_agent(retained.unwrap(), response())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let SessionEvent::Acp(AcpEvent::Response {
                response: Ok(acp::AgentResponse::PromptResponse(_)),
                ..
            }) = session.events.recv().await.unwrap()
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    session
        .handle
        .prompt(vec![acp::ContentBlock::Text(acp::TextContent::new(
            "fixture:early-withdraw-user-question",
        ))])
        .await
        .unwrap();
    let retained = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let SessionEvent::Acp(AcpEvent::Request {
                request_id,
                request: acp::AgentRequest::ExtMethodRequest(request),
            }) = session.events.recv().await.unwrap()
            {
                let params: serde_json::Value = serde_json::from_str(request.params.get()).unwrap();
                assert_eq!(
                    params["callId"], "retained",
                    "early withdrawal must not show its question"
                );
                break request_id;
            }
        }
    })
    .await
    .unwrap();
    session
        .handle
        .respond_to_agent(retained, response())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match session.events.recv().await.unwrap() {
                SessionEvent::Acp(AcpEvent::Request {
                    request: acp::AgentRequest::ExtMethodRequest(request),
                    ..
                }) => {
                    let params: serde_json::Value =
                        serde_json::from_str(request.params.get()).unwrap();
                    assert_ne!(
                        params["callId"], "withdrawn",
                        "early withdrawal must not show its question"
                    );
                }
                SessionEvent::Acp(AcpEvent::Response {
                    response: Ok(acp::AgentResponse::PromptResponse(_)),
                    ..
                }) => break,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    session
        .handle
        .fork_history("question child".into(), None)
        .await
        .unwrap();
    let child = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Update(update))) =
                session.events.recv().await.unwrap()
                && matches!(
                    update.state,
                    Patch::Set(SubagentState::Idle { .. } | SubagentState::Ended(_))
                )
            {
                break update.session_id;
            }
        }
    })
    .await
    .unwrap();
    session
        .handle
        .fork_history("second question child".into(), None)
        .await
        .unwrap();
    let sibling = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Update(update))) =
                session.events.recv().await.unwrap()
                && update.session_id != child
                && matches!(
                    update.state,
                    Patch::Set(SubagentState::Idle { .. } | SubagentState::Ended(_))
                )
            {
                break update.session_id;
            }
        }
    })
    .await
    .unwrap();
    session
        .handle
        .send_subagent_content(
            child.clone(),
            vec![acp::ContentBlock::Text(acp::TextContent::new(
                "fixture:child-user-question",
            ))],
        )
        .await
        .unwrap();
    session
        .handle
        .send_subagent_content(
            sibling.clone(),
            vec![acp::ContentBlock::Text(acp::TextContent::new(
                "fixture:child-user-question",
            ))],
        )
        .await
        .unwrap();
    let mut seen = Vec::new();
    for _ in 0..2 {
        let (id, params) = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let SessionEvent::Acp(AcpEvent::Request {
                    request_id,
                    request: acp::AgentRequest::ExtMethodRequest(request),
                }) = session.events.recv().await.unwrap()
                {
                    break (
                        request_id,
                        serde_json::from_str::<serde_json::Value>(request.params.get()).unwrap(),
                    );
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(params["callId"], "fixture-call");
        seen.push((id, params["sessionId"].as_str().unwrap().to_owned()));
    }
    let expected = |view: &acp::SessionId| {
        format!("nori-relay:{}:{}nested-child", view.to_string().len(), view)
    };
    assert_ne!(seen[0].0, seen[1].0);
    let mut views = seen
        .iter()
        .map(|(_, view)| view.clone())
        .collect::<Vec<_>>();
    views.sort();
    let mut expected_views = vec![expected(&child), expected(&sibling)];
    expected_views.sort();
    assert_eq!(views, expected_views);
    for (index, (id, _)) in seen.into_iter().enumerate() {
        let answer = if index == 0 {
            serde_json::json!({"cancelled":true})
        } else {
            serde_json::json!({"answers":{"choice":{"answers":["yes"]}}})
        };
        session
            .handle
            .respond_to_agent(
                id,
                Ok(acp::ClientResponse::ExtMethodResponse(
                    acp::ExtResponse::new(Arc::from(
                        serde_json::value::to_raw_value(&answer).unwrap(),
                    )),
                )),
            )
            .await
            .unwrap();
    }
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut count = 0;
        while count < 2 {
            if let SessionEvent::Acp(AcpEvent::ChildNotification(notification)) =
                session.events.recv().await.unwrap()
                && let acp::SessionUpdate::AgentMessageChunk(chunk) = notification.update
                && let acp::ContentBlock::Text(text) = chunk.content
                && text.text.contains("question-response:")
            {
                count += 1;
            }
        }
    })
    .await
    .unwrap();
    session.handle.shutdown().await.unwrap();
}

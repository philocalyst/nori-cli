//! Permission requests, elicitations, user questions and shutdown with questions pending.

use crate::thread::Thread;
use crate::thread::ThreadMessage;
use crate::thread::actor::ThreadActor;
use crate::thread::client::SessionClient;
use crate::thread::mcp_approval::MCP_TOOL_APPROVAL_ALLOW_ALWAYS_OPTION_ID;
use crate::thread::mcp_approval::MCP_TOOL_APPROVAL_ALLOW_OPTION_ID;
use crate::thread::mcp_approval::MCP_TOOL_APPROVAL_ALLOW_SESSION_OPTION_ID;
use crate::thread::mcp_approval::MCP_TOOL_APPROVAL_CANCEL_OPTION_ID;
use crate::thread::mcp_approval::MCP_TOOL_APPROVAL_PERSIST_SESSION;
use crate::thread::mcp_approval::MCP_TOOL_APPROVAL_REQUEST_ID_PREFIX;
use crate::thread::prompt::PromptState;
use crate::thread::tests::harness::Recorded;
use crate::thread::tests::harness::StubAuth;
use crate::thread::tests::harness::StubClient;
use crate::thread::tests::harness::StubCodexThread;
use crate::thread::tests::harness::StubModelsManager;
use crate::thread::tests::harness::setup;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;
use codex_protocol::approvals::ElicitationRequest;
use codex_protocol::approvals::ElicitationRequestEvent;
use codex_protocol::parse_command::ParsedCommand;
use codex_protocol::protocol::AgentMessageEvent;
use codex_protocol::protocol::ElicitationAction;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecApprovalRequestEvent;
use codex_protocol::protocol::ReviewDecision;
use codex_utils_path_uri::LegacyAppPathString;
use nori_acp_kit::schema::ContentBlock;
use nori_acp_kit::schema::ContentChunk;
use nori_acp_kit::schema::PromptRequest;
use nori_acp_kit::schema::RequestPermissionOutcome;
use nori_acp_kit::schema::RequestPermissionResponse;
use nori_acp_kit::schema::SelectedPermissionOutcome;
use nori_acp_kit::schema::SessionId;
use nori_acp_kit::schema::SessionUpdate;
use nori_acp_kit::schema::StopReason;
use nori_acp_kit::schema::TextContent;
use nori_acp_kit::schema::ToolCallStatus;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

#[tokio::test]
async fn test_parallel_exec_commands() -> anyhow::Result<()> {
    let (session_id, client, _, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["parallel-exec".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();

    // Collect all ToolCall (begin) notifications keyed by their tool_call_id prefix.
    let tool_calls: Vec<_> = notifications
        .iter()
        .filter_map(|n| match &n.update {
            SessionUpdate::ToolCall(tc) => Some(tc.clone()),
            _ => None,
        })
        .collect();

    // Collect all ToolCallUpdate notifications that carry a terminal status.
    let completed_updates: Vec<_> = notifications
        .iter()
        .filter_map(|n| match &n.update {
            SessionUpdate::ToolCallUpdate(update) => {
                if update.fields.status == Some(ToolCallStatus::Completed) {
                    Some(update.clone())
                } else {
                    None
                }
            }
            _ => None,
        })
        .collect();

    // Both commands A and B should have produced a ToolCall (begin).
    assert_eq!(
        tool_calls.len(),
        2,
        "expected 2 ToolCall begin notifications, got {tool_calls:?}"
    );

    // Both commands A and B should have produced a completed ToolCallUpdate.
    assert_eq!(
        completed_updates.len(),
        2,
        "expected 2 completed ToolCallUpdate notifications, got {completed_updates:?}"
    );

    // The completed updates should reference the same tool_call_ids as the begins.
    let begin_ids: std::collections::HashSet<_> = tool_calls
        .iter()
        .map(|tc| tc.tool_call_id.clone())
        .collect();
    let end_ids: std::collections::HashSet<_> = completed_updates
        .iter()
        .map(|u| u.tool_call_id.clone())
        .collect();
    assert_eq!(
        begin_ids, end_ids,
        "completed update tool_call_ids should match begin tool_call_ids"
    );

    Ok(())
}

#[tokio::test]
async fn user_questions_reach_the_requesting_session_and_resume_the_turn() {
    use codex_protocol::request_user_input::RequestUserInputEvent;
    use codex_protocol::request_user_input::RequestUserInputQuestion;
    for (session, answer) in [
        ("root", json!({"answers":{"choice":{"answers":["yes"]}}})),
        ("child", json!({"cancelled":true})),
    ] {
        let client = Arc::new(StubClient::new());
        *client.question_response.lock().unwrap() = Some(answer.clone());
        let session_client =
            SessionClient::with_client(SessionId::new(session), client.clone(), Arc::default());
        let thread = Arc::new(StubCodexThread::new());
        let (resolution_tx, _resolution_rx) = mpsc::unbounded_channel();
        let (response_tx, _response_rx) = oneshot::channel();
        let mut state = PromptState::new(
            "submission".into(),
            thread.clone(),
            resolution_tx,
            response_tx,
        );
        state
            .handle_event(
                &session_client,
                EventMsg::RequestUserInput(RequestUserInputEvent {
                    call_id: "call".into(),
                    turn_id: "actual-turn".into(),
                    is_blocking: true,
                    auto_resolution_ms: None,
                    questions: vec![RequestUserInputQuestion {
                        id: "choice".into(),
                        header: "Choice".into(),
                        question: "Proceed?".into(),
                        is_other: true,
                        is_secret: false,
                        options: None,
                    }],
                }),
            )
            .await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if thread
                    .ops
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|op| matches!(op, Recorded::UserInputAnswer { .. } | Recorded::Interrupt))
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("question response submitted");
        let requests = client.question_requests.lock().unwrap();
        assert_eq!(requests[0]["sessionId"], session);
        assert_eq!(requests[0]["questions"][0]["question"], "Proceed?");
        let ops = thread.ops.lock().unwrap();
        if session == "child" {
            assert_eq!(ops[0], Recorded::Interrupt);
        } else {
            let Recorded::UserInputAnswer { id, response } = &ops[0] else {
                panic!("expected answer")
            };
            assert_eq!(id, "actual-turn");
            assert_eq!(serde_json::to_value(response).unwrap(), answer);
        }
    }
}

#[tokio::test]
async fn test_exec_approval_uses_available_decisions() -> anyhow::Result<()> {
    let session_id = SessionId::new("test");
    let client = Arc::new(StubClient::with_permission_responses(vec![
        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new("denied"),
        )),
    ]));
    let session_client = SessionClient::with_client(session_id, client.clone(), Arc::default());
    let thread = Arc::new(StubCodexThread::new());
    let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
    let (message_tx, mut message_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut prompt_state = PromptState::new(
        "submission-id".to_string(),
        thread.clone(),
        message_tx,
        response_tx,
    );

    prompt_state.exec_approval(
        &session_client,
        ExecApprovalRequestEvent {
            call_id: "call-id".to_string(),
            approval_id: Some("approval-id".to_string()),
            kind: Default::default(),
            model_context: None,
            plugin_id: None,
            script_path: None,
            environment_id: None,
            turn_id: "turn-id".to_string(),
            started_at_ms: 0,
            command: vec!["echo".to_string(), "hi".to_string()],
            cwd: LegacyAppPathString::from_path(&std::env::current_dir()?),
            reason: None,
            network_approval_context: None,
            proposed_execpolicy_amendment: None,
            proposed_network_policy_amendments: None,
            additional_permissions: None,
            available_decisions: Some(vec![
                ReviewDecision::Approved,
                ReviewDecision::Denied {
                    rejection: String::new(),
                },
            ]),
            parsed_cmd: vec![ParsedCommand::Unknown {
                cmd: "echo hi".to_string(),
            }],
        },
    )?;

    let ThreadMessage::PermissionRequestResolved {
        submission_id,
        interaction_id,
        request_key,
        response,
    } = message_rx.recv().await.unwrap()
    else {
        panic!("expected permission resolution message");
    };
    assert_eq!(submission_id, "submission-id");
    prompt_state
        .handle_permission_request_resolved(&session_client, interaction_id, request_key, response)
        .await?;

    let requests = client.permission_requests.lock().unwrap();
    let request = requests.last().unwrap();
    let option_ids = request
        .options
        .iter()
        .map(|option| option.option_id.0.to_string())
        .collect::<Vec<_>>();
    assert_eq!(option_ids, vec!["approved", "denied"]);

    let ops = thread.ops.lock().unwrap();
    assert_eq!(
        ops.last(),
        Some(&Recorded::ExecApproval {
            id: "approval-id".to_string(),
            turn_id: Some("turn-id".to_string()),
            decision: ReviewDecision::Denied {
                rejection: "The user declined to run it".to_string(),
            },
        })
    );

    Ok(())
}

#[tokio::test]
async fn test_mcp_tool_approval_elicitation_routes_to_permission_request() -> anyhow::Result<()> {
    let session_id = SessionId::new("test");
    let client = Arc::new(StubClient::with_permission_responses(vec![
        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new(MCP_TOOL_APPROVAL_ALLOW_SESSION_OPTION_ID),
        )),
    ]));
    let session_client = SessionClient::with_client(session_id, client.clone(), Arc::default());
    let thread = Arc::new(StubCodexThread::new());
    let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
    let (message_tx, mut message_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut prompt_state = PromptState::new(
        "submission-id".to_string(),
        thread.clone(),
        message_tx,
        response_tx,
    );

    let request_id = format!("{MCP_TOOL_APPROVAL_REQUEST_ID_PREFIX}call-123");
    prompt_state
        .mcp_elicitation(
            &session_client,
            ElicitationRequestEvent {
                turn_id: Some("turn-id".to_string()),
                server_name: "test-server".to_string(),
                id: codex_protocol::mcp::RequestId::String(request_id.clone()),
                request: ElicitationRequest::Form {
                    meta: Some(serde_json::json!({
                        "codex_approval_kind": "mcp_tool_call",
                        "persist": ["session", "always"],
                        "connector_name": "Docs",
                        "tool_title": "search_docs",
                        "tool_description": "Search project documentation",
                        "tool_params_display": [
                            {
                                "display_name": "Query",
                                "name": "query",
                                "value": "approval flow"
                            }
                        ]
                    })),
                    message: "Allow Docs to run tool \"search_docs\"?".to_string(),
                    requested_schema: serde_json::json!({
                        "type": "object",
                        "properties": {}
                    }),
                },
            },
        )
        .await?;

    let ThreadMessage::PermissionRequestResolved {
        submission_id,
        interaction_id,
        request_key,
        response,
    } = message_rx.recv().await.unwrap()
    else {
        panic!("expected permission resolution message");
    };
    assert_eq!(submission_id, "submission-id");

    {
        let requests = client.permission_requests.lock().unwrap();
        let request = requests.last().unwrap();
        assert_eq!(request.tool_call.tool_call_id.0.as_ref(), "call-123");
        assert_eq!(
            request
                .options
                .iter()
                .map(|option| option.option_id.0.to_string())
                .collect::<Vec<_>>(),
            vec![
                MCP_TOOL_APPROVAL_ALLOW_OPTION_ID.to_string(),
                MCP_TOOL_APPROVAL_ALLOW_SESSION_OPTION_ID.to_string(),
                MCP_TOOL_APPROVAL_ALLOW_ALWAYS_OPTION_ID.to_string(),
                MCP_TOOL_APPROVAL_CANCEL_OPTION_ID.to_string(),
            ]
        );
    }

    prompt_state
        .handle_permission_request_resolved(&session_client, interaction_id, request_key, response)
        .await?;

    let op = thread.ops.lock().unwrap().last().cloned().unwrap();
    match op {
        Recorded::ResolveElicitation {
            server_name,
            request_id: codex_protocol::mcp::RequestId::String(id),
            decision,
            content,
            meta,
        } => {
            assert_eq!(server_name, "test-server");
            assert_eq!(id, request_id);
            assert_eq!(decision, ElicitationAction::Accept);
            assert!(content.is_none());
            assert_eq!(
                meta.as_ref()
                    .and_then(|value| value.get("persist"))
                    .and_then(serde_json::Value::as_str),
                Some(MCP_TOOL_APPROVAL_PERSIST_SESSION)
            );
        }
        other => panic!("unexpected op: {other:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn test_mcp_elicitation_declines_unsupported_form_requests() -> anyhow::Result<()> {
    let session_id = SessionId::new("test");
    let client = Arc::new(StubClient::with_permission_responses(vec![
        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new("decline"),
        )),
    ]));
    let session_client = SessionClient::with_client(session_id, client.clone(), Arc::default());
    let thread = Arc::new(StubCodexThread::new());
    let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
    let (message_tx, _message_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut prompt_state = PromptState::new(
        "submission-id".to_string(),
        thread.clone(),
        message_tx,
        response_tx,
    );

    prompt_state
        .mcp_elicitation(
            &session_client,
            ElicitationRequestEvent {
                turn_id: Some("turn-id".to_string()),
                server_name: "test-server".to_string(),
                id: codex_protocol::mcp::RequestId::String("request-id".to_string()),
                request: ElicitationRequest::Form {
                    meta: None,
                    message: "Need some structured input".to_string(),
                    requested_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" }
                        }
                    }),
                },
            },
        )
        .await?;

    let requests = client.permission_requests.lock().unwrap();
    assert!(
        requests.is_empty(),
        "unsupported MCP elicitations should be auto-declined"
    );

    let ops = thread.ops.lock().unwrap();
    assert!(matches!(
        ops.last(),
        Some(Recorded::ResolveElicitation {
            server_name,
            request_id: codex_protocol::mcp::RequestId::String(request_id),
            decision: ElicitationAction::Decline,
            content: None,
            meta: None,
        }) if server_name == "test-server" && request_id == "request-id"
    ));

    Ok(())
}

#[tokio::test]
async fn test_blocked_approval_does_not_block_followup_events() -> anyhow::Result<()> {
    let session_id = SessionId::new("test");
    let notify = Arc::new(Notify::new());
    let client = Arc::new(StubClient::with_blocked_permission_requests(
        vec![],
        notify.clone(),
    ));
    let session_client = SessionClient::with_client(session_id, client.clone(), Arc::default());
    let thread = Arc::new(StubCodexThread::new());
    let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
    let (message_tx, _message_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut prompt_state =
        PromptState::new("submission-id".to_string(), thread, message_tx, response_tx);

    prompt_state
        .handle_event(
            &session_client,
            EventMsg::ExecApprovalRequest(ExecApprovalRequestEvent {
                call_id: "call-id".to_string(),
                approval_id: Some("approval-id".to_string()),
                kind: Default::default(),
                model_context: None,
                plugin_id: None,
                script_path: None,
                environment_id: None,
                turn_id: "turn-id".to_string(),
                started_at_ms: 0,
                command: vec!["echo".to_string(), "hi".to_string()],
                cwd: LegacyAppPathString::from_path(&std::env::current_dir()?),
                reason: None,
                network_approval_context: None,
                proposed_execpolicy_amendment: None,
                proposed_network_policy_amendments: None,
                additional_permissions: None,
                available_decisions: Some(vec![ReviewDecision::Approved, ReviewDecision::Abort]),
                parsed_cmd: vec![ParsedCommand::Unknown {
                    cmd: "echo hi".to_string(),
                }],
            }),
        )
        .await;

    prompt_state
        .handle_event(
            &session_client,
            EventMsg::AgentMessage(AgentMessageEvent {
                message: "still flowing".to_string(),
                phase: None,
                memory_citation: None,
                moderation: None,
                delivery: None,
                questions: None,
            }),
        )
        .await;

    let notifications = client.notifications.lock().unwrap();
    assert!(notifications.iter().any(|notification| {
        matches!(
            &notification.update,
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(TextContent { text, .. }),
                ..
            }) if text == "still flowing"
        )
    }));

    drop(notifications);
    prompt_state.detach_pending_interactions();
    notify.notify_one();

    Ok(())
}

#[tokio::test]
async fn test_detached_permission_request_drains_late_response() -> anyhow::Result<()> {
    let notify = Arc::new(Notify::new());
    let session_id = SessionId::new("test");
    let client = Arc::new(StubClient::with_blocked_permission_requests(
        vec![RequestPermissionResponse::new(
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("approved")),
        )],
        notify.clone(),
    ));
    let session_client = SessionClient::with_client(session_id, client.clone(), Arc::default());
    let thread = Arc::new(StubCodexThread::new());
    let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
    let (message_tx, mut message_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut prompt_state = PromptState::new(
        "submission-id".to_string(),
        thread.clone(),
        message_tx,
        response_tx,
    );

    prompt_state
        .handle_event(
            &session_client,
            EventMsg::ExecApprovalRequest(ExecApprovalRequestEvent {
                call_id: "call-id".to_string(),
                approval_id: Some("approval-id".to_string()),
                kind: Default::default(),
                model_context: None,
                plugin_id: None,
                script_path: None,
                environment_id: None,
                turn_id: "turn-id".to_string(),
                started_at_ms: 0,
                command: vec!["echo".to_string(), "hi".to_string()],
                cwd: LegacyAppPathString::from_path(&std::env::current_dir()?),
                reason: None,
                network_approval_context: None,
                proposed_execpolicy_amendment: None,
                proposed_network_policy_amendments: None,
                additional_permissions: None,
                available_decisions: Some(vec![ReviewDecision::Approved, ReviewDecision::Abort]),
                parsed_cmd: vec![ParsedCommand::Unknown {
                    cmd: "echo hi".to_string(),
                }],
            }),
        )
        .await;

    tokio::time::timeout(Duration::from_millis(100), async {
        loop {
            if !client.permission_requests.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;

    prompt_state.detach_pending_interactions();
    notify.notify_one();

    let ThreadMessage::PermissionRequestResolved {
        submission_id,
        interaction_id,
        request_key,
        response,
    } = tokio::time::timeout(Duration::from_millis(100), message_rx.recv())
        .await?
        .expect("permission response should be drained")
    else {
        panic!("expected permission resolution message");
    };
    assert_eq!(submission_id, "submission-id");

    prompt_state
        .handle_permission_request_resolved(&session_client, interaction_id, request_key, response)
        .await?;

    let ops = thread.ops.lock().unwrap();
    assert!(
        ops.is_empty(),
        "late permission response should not submit an approval: {ops:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_thread_shutdown_bypasses_blocked_permission_request() -> anyhow::Result<()> {
    let session_id = SessionId::new("test");
    let notify = Arc::new(Notify::new());
    let client = Arc::new(StubClient::with_blocked_permission_requests(
        vec![RequestPermissionResponse::new(
            RequestPermissionOutcome::Cancelled,
        )],
        notify.clone(),
    ));
    let session_client =
        SessionClient::with_client(session_id.clone(), client.clone(), Arc::default());
    let conversation = Arc::new(StubCodexThread::new());
    let models_manager = Arc::new(StubModelsManager);
    let config =
        Config::load_with_cli_overrides_and_harness_overrides(vec![], ConfigOverrides::default())
            .await?;
    let (message_tx, message_rx) = tokio::sync::mpsc::unbounded_channel();
    let (resolution_tx, resolution_rx) = tokio::sync::mpsc::unbounded_channel();
    let actor = ThreadActor::new(
        StubAuth,
        session_client,
        conversation.clone(),
        models_manager,
        config,
        message_rx,
        resolution_tx,
        resolution_rx,
    );

    let handle = tokio::spawn(actor.spawn());
    let thread = Thread {
        thread: conversation.clone(),
        gate: nori_acp_kit::TurnGate::new(nori_acp_kit::TurnPolicy::Steerable),
        message_tx,
        _handle: handle,
    };

    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();
    thread.message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id, vec!["approval-block".into()]),
        response_tx: prompt_response_tx,
    })?;
    let stop_reason_rx = prompt_response_rx.await??;

    tokio::time::timeout(Duration::from_millis(100), async {
        loop {
            if !client.permission_requests.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;

    tokio::time::timeout(Duration::from_millis(100), thread.shutdown()).await??;
    let stop_reason = tokio::time::timeout(Duration::from_millis(100), stop_reason_rx).await??;
    assert_eq!(stop_reason?, StopReason::Cancelled);
    notify.notify_one();

    let ops = conversation.ops.lock().unwrap();
    assert!(matches!(ops.last(), Some(Recorded::Shutdown)));

    Ok(())
}

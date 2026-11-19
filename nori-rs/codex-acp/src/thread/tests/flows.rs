//! Whole-turn flows through a stub Codex thread: prompts, slash commands, deltas.

use crate::thread::ThreadMessage;
use crate::thread::client::SessionClient;
use crate::thread::guardian::guardian_action_summary;
use crate::thread::modes::current_session_mode_id;
use crate::thread::modes::mode_trusts_project;
use crate::thread::prompt::PromptState;
use crate::thread::submit::INIT_COMMAND_PROMPT;
use crate::thread::tests::harness::Recorded;
use crate::thread::tests::harness::StubClient;
use crate::thread::tests::harness::StubCodexThread;
use crate::thread::tests::harness::setup;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;
use codex_core::review_prompts::user_facing_hint;
use codex_protocol::approvals::GuardianAssessmentAction;
use codex_protocol::approvals::GuardianCommandSource;
use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::DynamicToolCallResponseEvent;
use codex_protocol::protocol::ReviewRequest;
use codex_protocol::protocol::ReviewTarget;
use codex_protocol::user_input::UserInput;
use nori_acp_kit::schema::Content;
use nori_acp_kit::schema::ContentBlock;
use nori_acp_kit::schema::ContentChunk;
use nori_acp_kit::schema::ImageContent;
use nori_acp_kit::schema::PromptRequest;
use nori_acp_kit::schema::SessionId;
use nori_acp_kit::schema::SessionUpdate;
use nori_acp_kit::schema::StopReason;
use nori_acp_kit::schema::TextContent;
use nori_acp_kit::schema::ToolCallContent;
use nori_acp_kit::schema::ToolCallStatus;
use pretty_assertions::assert_eq;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn test_prompt() -> anyhow::Result<()> {
    let (session_id, client, _, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["Hi".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    assert_eq!(notifications.len(), 1);
    assert!(matches!(
        &notifications[0].update,
        SessionUpdate::AgentMessageChunk(ContentChunk {
            content: ContentBlock::Text(TextContent { text, .. }),
            message_id: Some(_),
            ..
        }) if text == "Hi"
    ));

    Ok(())
}

#[tokio::test]
async fn test_thread_goal_updated_is_sent_as_agent_message() -> anyhow::Result<()> {
    let (session_id, client, _, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["thread-goal-update".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    assert!(notifications.iter().any(|notification| {
        matches!(
            &notification.update,
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(TextContent { text, .. }),
                ..
            }) if text == "Goal updated (active): Ship the goal update"
        )
    }));

    Ok(())
}

#[tokio::test]
async fn test_image_generation_emits_image_content() -> anyhow::Result<()> {
    let (session_id, client, _, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();
    let expected_uri = image_generation_test_saved_path()
        .to_string_lossy()
        .into_owned();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["image-generation".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    let tool_call = notifications
        .iter()
        .find_map(|notification| match &notification.update {
            SessionUpdate::ToolCall(tool_call) if tool_call.tool_call_id.0.as_ref() == "ig-1" => {
                Some(tool_call)
            }
            _ => None,
        })
        .expect("image generation tool call should be sent");
    assert_eq!(tool_call.title, "Image generation");
    assert_eq!(tool_call.status, ToolCallStatus::InProgress);

    let update = notifications
        .iter()
        .find_map(|notification| match &notification.update {
            SessionUpdate::ToolCallUpdate(update) if update.tool_call_id.0.as_ref() == "ig-1" => {
                Some(update)
            }
            _ => None,
        })
        .expect("image generation tool call update should be sent");
    assert_eq!(update.fields.status, Some(ToolCallStatus::Completed));
    let content = update
        .fields
        .content
        .as_ref()
        .expect("image generation update should include content");
    assert_eq!(content.len(), 2);
    assert!(matches!(
        &content[0],
        ToolCallContent::Content(Content {
            content: ContentBlock::Text(TextContent { text, .. }),
            ..
        }) if text == "Revised prompt: A tiny blue square"
    ));
    assert!(matches!(
        &content[1],
        ToolCallContent::Content(Content {
            content: ContentBlock::Image(ImageContent {
                data,
                mime_type,
                uri,
                ..
            }),
            ..
        }) if data == "Zm9v" && mime_type == "image/png" && uri.as_deref() == Some(expected_uri.as_str())
    ));

    Ok(())
}

pub(super) fn image_generation_test_saved_path() -> PathBuf {
    std::env::temp_dir().join("ig-1.png")
}

#[tokio::test]
async fn test_compact() -> anyhow::Result<()> {
    let (session_id, client, thread, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["/compact".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    assert_eq!(notifications.len(), 1);
    assert!(matches!(
        &notifications[0].update,
        SessionUpdate::AgentMessageChunk(ContentChunk {
            content: ContentBlock::Text(TextContent { text, .. }),
            ..
        }) if text == "Compact task completed"
    ));
    let ops = thread.ops.lock().unwrap();
    assert_eq!(ops.as_slice(), &[Recorded::Compact]);

    Ok(())
}

#[test]
fn test_guardian_execve_summary_uses_argv_without_duplication() -> anyhow::Result<()> {
    let action = GuardianAssessmentAction::Execve {
        source: GuardianCommandSource::UnifiedExec,
        program: "/bin/ls".to_string(),
        argv: vec!["/bin/ls".to_string(), "-l".to_string()],
        cwd: std::env::current_dir()?.try_into()?,
    };

    assert_eq!(
        guardian_action_summary(&action),
        Some("exec /bin/ls -l".to_string())
    );

    Ok(())
}

#[tokio::test]
async fn nori_full_access_overrides_an_accounts_read_only_mode() -> anyhow::Result<()> {
    let home = std::env::temp_dir().join(format!("nori-codex-mode-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home)?;
    std::fs::write(
        home.join("config.toml"),
        "approval_policy = \"on-request\"\nsandbox_mode = \"read-only\"\nmodel = \"gpt-account-setting\"\n",
    )?;
    let overrides = codex_utils_cli::CliConfigOverrides {
        raw_overrides: vec![
            "approval_policy=\"never\"".into(),
            "sandbox_mode=\"danger-full-access\"".into(),
        ],
    }
    .parse_overrides()
    .map_err(anyhow::Error::msg)?;
    for persisted in [None, Some(PermissionProfile::read_only())] {
        let config = codex_core::config::ConfigBuilder::default()
            .codex_home(home.clone())
            .cli_overrides(overrides.clone())
            .harness_overrides(ConfigOverrides {
                approval_policy: Some(codex_protocol::protocol::AskForApproval::Never),
                sandbox_mode: Some(codex_protocol::config_types::SandboxMode::DangerFullAccess),
                persisted_permission_profile: persisted,
                ..ConfigOverrides::default()
            })
            .build()
            .await?;
        assert_eq!(
            current_session_mode_id(&config)
                .expect("recognized mode")
                .0
                .as_ref(),
            "full-access"
        );
        assert_eq!(config.model.as_deref(), Some("gpt-account-setting"));
    }
    std::fs::remove_dir_all(home)?;
    Ok(())
}

#[tokio::test]
async fn modes_match_augmented_workspace_permission_profile() -> anyhow::Result<()> {
    let mut config =
        Config::load_with_cli_overrides_and_harness_overrides(vec![], ConfigOverrides::default())
            .await?;
    config
        .permissions
        .approval_policy
        .set(codex_protocol::protocol::AskForApproval::OnRequest)?;

    let workspace_profile = PermissionProfile::workspace_write();
    let extra_roots = vec![config.codex_home.as_path().join("memories").try_into()?];
    let file_system_policy = workspace_profile
        .file_system_sandbox_policy()
        .with_additional_writable_roots(config.cwd.as_path(), &extra_roots);
    let augmented_profile = PermissionProfile::from_runtime_permissions(
        &file_system_policy,
        workspace_profile.network_sandbox_policy(),
    );
    assert_ne!(augmented_profile, workspace_profile);

    config
        .permissions
        .set_permission_profile(augmented_profile)?;

    let mode_id = current_session_mode_id(&config).expect("mode should be recognized");
    assert_eq!(mode_id.0.as_ref(), "auto");

    Ok(())
}

#[tokio::test]
async fn modes_match_legacy_augmented_workspace_permission_profile() -> anyhow::Result<()> {
    let mut config =
        Config::load_with_cli_overrides_and_harness_overrides(vec![], ConfigOverrides::default())
            .await?;
    config
        .permissions
        .approval_policy
        .set(codex_protocol::protocol::AskForApproval::OnRequest)?;

    let workspace_profile = PermissionProfile::workspace_write();
    let extra_roots = vec![config.codex_home.as_path().join("memories").try_into()?];
    let file_system_policy = workspace_profile
        .file_system_sandbox_policy()
        .with_additional_writable_roots(config.cwd.as_path(), &extra_roots);
    let augmented_profile = PermissionProfile::from_runtime_permissions(
        &file_system_policy,
        workspace_profile.network_sandbox_policy(),
    );
    assert_ne!(augmented_profile, workspace_profile);

    config
        .permissions
        .set_permission_profile(augmented_profile)?;
    assert!(config.permissions.active_permission_profile().is_none());

    let mode_id = current_session_mode_id(&config).expect("mode should be recognized");
    assert_eq!(mode_id.0.as_ref(), "auto");

    Ok(())
}

#[test]
fn read_only_mode_does_not_trust_project() {
    assert!(!mode_trusts_project("read-only"));
    assert!(mode_trusts_project("auto"));
    assert!(mode_trusts_project("full-access"));
}

#[tokio::test]
async fn test_init() -> anyhow::Result<()> {
    let (session_id, client, thread, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["/init".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    assert_eq!(notifications.len(), 1);
    assert!(
        matches!(
            &notifications[0].update,
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(TextContent { text, .. }), ..
            }) if text == INIT_COMMAND_PROMPT // we echo the prompt
        ),
        "notifications don't match {notifications:?}"
    );
    let ops = thread.ops.lock().unwrap();
    assert_eq!(
        ops.as_slice(),
        &[Recorded::UserInput(vec![UserInput::Text {
            text: INIT_COMMAND_PROMPT.to_string(),
            text_elements: vec![]
        }])],
        "ops don't match {ops:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_review() -> anyhow::Result<()> {
    let (session_id, client, thread, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["/review".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    assert_eq!(notifications.len(), 1);
    assert!(
        matches!(
            &notifications[0].update,
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(TextContent { text, .. }),
                ..
            }) if text == "current changes" // we echo the prompt
        ),
        "notifications don't match {notifications:?}"
    );

    let ops = thread.ops.lock().unwrap();
    assert_eq!(
        ops.as_slice(),
        &[Recorded::Review(ReviewRequest {
            user_facing_hint: Some(user_facing_hint(&ReviewTarget::UncommittedChanges)),
            target: ReviewTarget::UncommittedChanges,
        })],
        "ops don't match {ops:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_custom_review() -> anyhow::Result<()> {
    let (session_id, client, thread, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();
    let instructions = "Review what we did in agents.md";

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(
            session_id.clone(),
            vec![format!("/review {instructions}").into()],
        ),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    assert_eq!(notifications.len(), 1);
    assert!(
        matches!(
            &notifications[0].update,
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(TextContent { text, .. }),
                ..
            }) if text == "Review what we did in agents.md" // we echo the prompt
        ),
        "notifications don't match {notifications:?}"
    );

    let ops = thread.ops.lock().unwrap();
    assert_eq!(
        ops.as_slice(),
        &[Recorded::Review(ReviewRequest {
            user_facing_hint: Some(user_facing_hint(&ReviewTarget::Custom {
                instructions: instructions.to_owned()
            })),
            target: ReviewTarget::Custom {
                instructions: instructions.to_owned()
            },
        })],
        "ops don't match {ops:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_commit_review() -> anyhow::Result<()> {
    let (session_id, client, thread, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["/review-commit 123456".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    assert_eq!(notifications.len(), 1);
    assert!(
        matches!(
            &notifications[0].update,
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(TextContent { text, .. }),
                ..
            }) if text == "commit 123456" // we echo the prompt
        ),
        "notifications don't match {notifications:?}"
    );

    let ops = thread.ops.lock().unwrap();
    assert_eq!(
        ops.as_slice(),
        &[Recorded::Review(ReviewRequest {
            user_facing_hint: Some(user_facing_hint(&ReviewTarget::Commit {
                sha: "123456".to_owned(),
                title: None
            })),
            target: ReviewTarget::Commit {
                sha: "123456".to_owned(),
                title: None
            },
        })],
        "ops don't match {ops:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_branch_review() -> anyhow::Result<()> {
    let (session_id, client, thread, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["/review-branch feature".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    let notifications = client.notifications.lock().unwrap();
    assert_eq!(notifications.len(), 1);
    assert!(
        matches!(
            &notifications[0].update,
            SessionUpdate::AgentMessageChunk(ContentChunk {
                content: ContentBlock::Text(TextContent { text, .. }),
                ..
            }) if text == "changes against 'feature'" // we echo the prompt
        ),
        "notifications don't match {notifications:?}"
    );

    let ops = thread.ops.lock().unwrap();
    assert_eq!(
        ops.as_slice(),
        &[Recorded::Review(ReviewRequest {
            user_facing_hint: Some(user_facing_hint(&ReviewTarget::BaseBranch {
                branch: "feature".to_owned()
            })),
            target: ReviewTarget::BaseBranch {
                branch: "feature".to_owned()
            },
        })],
        "ops don't match {ops:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_delta_deduplication() -> anyhow::Result<()> {
    let (session_id, client, _, message_tx, _handle) = setup().await?;
    let (prompt_response_tx, prompt_response_rx) = tokio::sync::oneshot::channel();

    message_tx.send(ThreadMessage::Prompt {
        request: PromptRequest::new(session_id.clone(), vec!["test delta".into()]),
        response_tx: prompt_response_tx,
    })?;

    let stop_reason = prompt_response_rx.await??.await??;
    assert_eq!(stop_reason, StopReason::EndTurn);
    drop(message_tx);

    // We should only get ONE notification, not duplicates from both delta and non-delta
    let notifications = client.notifications.lock().unwrap();
    assert_eq!(
        notifications.len(),
        1,
        "Should only receive delta event, not duplicate non-delta. Got: {notifications:?}"
    );
    assert!(matches!(
        &notifications[0].update,
        SessionUpdate::AgentMessageChunk(ContentChunk {
            content: ContentBlock::Text(TextContent { text, .. }),
            ..
        }) if text == "test delta"
    ));

    Ok(())
}

#[test]
fn model_only_goal_context_is_recorded_without_changing_visible_tool_output() {
    let client = Arc::new(StubClient::new());
    let session_client =
        SessionClient::with_client(SessionId::new("root"), client.clone(), Arc::default());
    let thread = Arc::new(StubCodexThread::new());
    let (resolution_tx, _) = tokio::sync::mpsc::unbounded_channel();
    let (response_tx, _) = tokio::sync::oneshot::channel();
    let state = PromptState::new("submission".into(), thread, resolution_tx, response_tx);
    let context = "<goal_context>revision 2</goal_context>";
    state
        .host_goal_contexts
        .lock()
        .unwrap()
        .insert("call".into(), context.into());
    let item = DynamicToolCallOutputContentItem::InputText {
        text: context.into(),
    };
    state.end_dynamic_tool_call(
        &session_client,
        DynamicToolCallResponseEvent {
            call_id: "call".into(),
            turn_id: "turn".into(),
            completed_at_ms: 0,
            namespace: None,
            tool: "task_list".into(),
            arguments: serde_json::json!({}),
            content_items: vec![item.clone(), item],
            success: true,
            error: None,
            duration: Duration::ZERO,
        },
    );
    let notifications = client.notifications.lock().unwrap();
    let SessionUpdate::ToolCallUpdate(update) = &notifications[0].update else {
        panic!("expected tool update")
    };
    assert_eq!(
        update.fields.raw_output.as_ref().unwrap()["content_items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(update.fields.content.as_ref().unwrap().len(), 1);
    assert_eq!(update.meta.as_ref().unwrap()["nori/modelContext"], context);
}

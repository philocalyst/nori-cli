use super::*;
use serde_json::json;

#[test]
fn test_converter_new() {
    let converter = NotificationConverter::new();
    assert!(converter.tool_use_cache.is_empty());
}

#[test]
fn test_cache_tool_use() {
    let converter = NotificationConverter::new();
    let tool_use = ToolUseBlock {
        id: "tool_123".to_string(),
        name: "Read".to_string(),
        input: json!({"file_path": "/test.txt"}),
    };

    converter.cache_tool_use(&tool_use);

    let cached = converter.get_tool_use("tool_123");
    assert!(cached.is_some());
    assert_eq!(cached.unwrap().name, "Read");
}

#[test]
fn test_make_agent_message() {
    let converter = NotificationConverter::new();
    let session_id = SessionId::new("session-1");
    let notification = converter.make_agent_message(&session_id, "Hello!");

    assert_eq!(notification.session_id.0.as_ref(), "session-1");
    // Check that it's an AgentMessageChunk update
    assert!(matches!(
        notification.update,
        SessionUpdate::AgentMessageChunk(_)
    ));
}

#[test]
fn test_make_agent_message_chunk() {
    let converter = NotificationConverter::new();
    let session_id = SessionId::new("session-1");
    let notification = converter.make_agent_message_chunk(&session_id, "chunk");

    // Check that it's an AgentMessageChunk update
    assert!(matches!(
        notification.update,
        SessionUpdate::AgentMessageChunk(_)
    ));
}

#[test]
fn test_make_agent_thought() {
    let converter = NotificationConverter::new();
    let session_id = SessionId::new("session-1");
    let notification = converter.make_agent_thought(&session_id, "thinking...");

    // Check that it's an AgentThoughtChunk update
    assert!(matches!(
        notification.update,
        SessionUpdate::AgentThoughtChunk(_)
    ));
}

#[test]
fn test_make_tool_call() {
    let converter = NotificationConverter::new();
    let session_id = SessionId::new("session-1");
    let tool_use = ToolUseBlock {
        id: "tool_456".to_string(),
        name: "Bash".to_string(),
        input: json!({"command": "ls", "description": "List files"}),
    };

    let notification = converter.make_tool_call(&session_id, &tool_use);

    // Check that it's a ToolCall update
    assert!(matches!(notification.update, SessionUpdate::ToolCall(_)));
    if let SessionUpdate::ToolCall(tool_call) = &notification.update {
        assert_eq!(tool_call.tool_call_id.0.as_ref(), "tool_456");
    }
}

#[test]
fn a_tool_result_in_a_user_message_finishes_the_call() {
    let converter = NotificationConverter::new();
    let session_id = SessionId::new("s");
    let tool_use = ToolUseBlock {
        id: "toolu_1".to_string(),
        name: "mcp__nori__git_status".to_string(),
        input: json!({}),
    };
    converter.cache_tool_use(&tool_use);
    // As the CLI writes it in stream-json.
    let message: Message = serde_json::from_value(json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_1",
                "content": "{\"branch\":\"main\",\"changes\":[]}",
                "is_error": false
            }]
        },
        "parent_tool_use_id": null,
        "session_id": "abc"
    }))
    .expect("a user message");
    let notifications = converter.convert_message(&message, "s");
    let [notification] = notifications.as_slice() else {
        panic!("one update: {notifications:?}");
    };
    let SessionUpdate::ToolCallUpdate(update) = &notification.update else {
        panic!("a tool call update");
    };
    assert_eq!(update.tool_call_id.0.as_ref(), "toolu_1");
    assert_eq!(update.fields.status, Some(ToolCallStatus::Completed));

    // An MCP result's text blocks are its output, not a JSON array of blocks.
    converter.cache_tool_use(&ToolUseBlock {
        id: "toolu_3".to_string(),
        name: "mcp__nori__git_log".to_string(),
        input: json!({}),
    });
    let blocks: Message = serde_json::from_value(json!({
        "type": "user",
        "message": {"role": "user", "content": [{
            "type": "tool_result",
            "tool_use_id": "toolu_3",
            "content": [{"type": "text", "text": "{\"commits\":[]}"}]
        }]},
        "session_id": "abc"
    }))
    .expect("a user message");
    let [notification] = converter
        .convert_message(&blocks, "s")
        .try_into()
        .expect("one");
    let SessionUpdate::ToolCallUpdate(update) = notification.update else {
        panic!("a tool call update");
    };
    assert_eq!(
        update.fields.raw_output,
        Some(json!({"content": "{\"commits\":[]}", "is_error": false}))
    );

    // The ACP server finishes its own tools; their results are not reported twice.
    converter.cache_tool_use(&ToolUseBlock {
        id: "toolu_2".to_string(),
        name: "mcp__acp__Edit".to_string(),
        input: json!({}),
    });
    let edit_result: Message = serde_json::from_value(json!({
        "type": "user",
        "message": {"role": "user", "content": [{
            "type": "tool_result", "tool_use_id": "toolu_2", "content": "ok"
        }]},
        "session_id": "abc"
    }))
    .expect("a user message");
    assert_eq!(converter.convert_message(&edit_result, "s").len(), 0);

    // A plain echo of the user's text finishes nothing.
    let echo: Message = serde_json::from_value(json!({
        "type": "user",
        "message": {"role": "user", "content": "hello"},
        "session_id": "abc"
    }))
    .expect("an echo");
    assert_eq!(converter.convert_message(&echo, &session_id.0).len(), 0);
}

#[test]
fn a_host_tool_call_carries_its_name_in_meta() {
    let converter = NotificationConverter::new();
    let tool_use = ToolUseBlock {
        id: "tool_1".to_string(),
        name: "mcp__nori__git_status".to_string(),
        input: json!({}),
    };
    let notification = converter.make_tool_call(&SessionId::new("s"), &tool_use);
    let SessionUpdate::ToolCall(tool_call) = notification.update else {
        panic!("expected a tool call");
    };
    assert_eq!(tool_call.title, "git_status");
    assert_eq!(
        nori_host_tools::host_tool_of(tool_call.meta.as_ref()),
        Some("git_status")
    );
}

#[test]
fn test_map_tool_kind() {
    assert!(matches!(
        NotificationConverter::map_tool_kind(ToolKind::Read),
        AcpToolKind::Read
    ));
    assert!(matches!(
        NotificationConverter::map_tool_kind(ToolKind::Edit),
        AcpToolKind::Edit
    ));
    assert!(matches!(
        NotificationConverter::map_tool_kind(ToolKind::Execute),
        AcpToolKind::Execute
    ));
}

#[test]
fn test_make_plan_from_todo_write() {
    let converter = NotificationConverter::new();
    let session_id = SessionId::new("session-1");

    // Create a TodoWrite tool use
    let tool_use = ToolUseBlock {
        id: "todo_123".to_string(),
        name: "TodoWrite".to_string(),
        input: json!({
            "todos": [
                {
                    "content": "Implement feature",
                    "status": "in_progress",
                    "activeForm": "Implementing feature"
                },
                {
                    "content": "Write tests",
                    "status": "pending",
                    "activeForm": "Writing tests"
                },
                {
                    "content": "Setup project",
                    "status": "completed",
                    "activeForm": "Setting up project"
                }
            ]
        }),
    };

    let notification = converter.make_plan_from_todo_write(&session_id, &tool_use);

    assert!(notification.is_some());
    let notification = notification.unwrap();

    if let SessionUpdate::Plan(plan) = &notification.update {
        assert_eq!(plan.entries.len(), 3);
        assert_eq!(plan.entries[0].content, "Implement feature");
        assert_eq!(plan.entries[0].status, PlanEntryStatus::InProgress);
        assert_eq!(plan.entries[1].content, "Write tests");
        assert_eq!(plan.entries[1].status, PlanEntryStatus::Pending);
        assert_eq!(plan.entries[2].content, "Setup project");
        assert_eq!(plan.entries[2].status, PlanEntryStatus::Completed);
    } else {
        panic!("Expected Plan update");
    }
}

#[test]
fn test_make_tool_result_todowrite_no_duplicate_plan() {
    // Since Plan is now sent at tool_use time, tool_result should NOT include Plan
    let converter = NotificationConverter::new();
    let session_id = SessionId::new("session-1");

    // Cache a TodoWrite tool use
    let tool_use = ToolUseBlock {
        id: "todo_456".to_string(),
        name: "TodoWrite".to_string(),
        input: json!({
            "todos": [
                {
                    "content": "Task 1",
                    "status": "pending",
                    "activeForm": "Doing task 1"
                }
            ]
        }),
    };
    converter.cache_tool_use(&tool_use);

    // Create a tool result
    let tool_result = ToolResultBlock {
        tool_use_id: "todo_456".to_string(),
        content: Some(ToolResultContent::Text("Todos updated".to_string())),
        is_error: Some(false),
    };

    let notifications = converter.make_tool_result(&session_id, &tool_result);

    // Should have only 1 notification: ToolCallUpdate (no Plan, since Plan is sent at tool_use time)
    assert_eq!(notifications.len(), 1);
    assert!(matches!(
        notifications[0].update,
        SessionUpdate::ToolCallUpdate(_)
    ));
}

#[test]
fn test_request_id_propagation() {
    let mut converter = NotificationConverter::new();
    let session_id = SessionId::new("session-1");

    // Without request_id, notification should not have meta
    let notification = converter.make_agent_message_chunk(&session_id, "test");
    assert!(notification.meta.is_none());

    // Set request_id
    converter.set_request_id("req-123".to_string());

    // Now notifications should have request_id in meta
    let notification = converter.make_agent_message_chunk(&session_id, "test");
    assert!(notification.meta.is_some());
    if let Some(meta) = &notification.meta {
        assert_eq!(meta.get("request_id"), Some(&serde_json::json!("req-123")));
    }
}

#[test]
fn test_request_id_clear() {
    let mut converter = NotificationConverter::new();
    let session_id = SessionId::new("session-1");

    // Set request_id
    converter.set_request_id("req-456".to_string());
    let notification = converter.make_agent_message_chunk(&session_id, "test");
    assert!(notification.meta.is_some());

    // Clear request_id
    converter.clear_request_id();
    let notification = converter.make_agent_message_chunk(&session_id, "test");
    assert!(notification.meta.is_none());
}

#[test]
fn test_request_id_propagation_all_notification_types() {
    let mut converter = NotificationConverter::new();
    let session_id = SessionId::new("session-1");

    // Set request_id
    let test_request_id = "test-req-789";
    converter.set_request_id(test_request_id.to_string());

    // Verify all notification types include request_id
    let notifications = vec![
        converter.make_agent_message_chunk(&session_id, "test"),
        converter.make_agent_thought_chunk(&session_id, "thinking"),
        converter.make_tool_call(
            &session_id,
            &ToolUseBlock {
                id: "tool-1".to_string(),
                name: "TestTool".to_string(),
                input: serde_json::json!({}),
            },
        ),
    ];

    for notification in notifications {
        assert!(
            notification.meta.is_some(),
            "Notification should have meta when request_id is set"
        );
        if let Some(meta) = &notification.meta {
            assert_eq!(
                meta.get("request_id"),
                Some(&serde_json::json!(test_request_id)),
                "Meta should contain the correct request_id"
            );
        }
    }
}

#[test]
fn test_request_id_with_converter_with_cwd() {
    let mut converter = NotificationConverter::with_cwd(std::path::PathBuf::from("/test"));
    let session_id = SessionId::new("session-1");

    // Initially no request_id
    let notification = converter.make_agent_message_chunk(&session_id, "test");
    assert!(notification.meta.is_none());

    // Set request_id
    converter.set_request_id("req-cwd-test".to_string());

    // Now should have request_id
    let notification = converter.make_agent_message_chunk(&session_id, "test");
    assert!(notification.meta.is_some());
    if let Some(meta) = &notification.meta {
        assert_eq!(
            meta.get("request_id"),
            Some(&serde_json::json!("req-cwd-test"))
        );
    }
}

#[test]
fn test_request_id_default() {
    let converter = NotificationConverter::new();
    // Default request_id should be None
    assert!(converter.request_id.is_none());
}

#[test]
fn test_request_id_with_cwd_default() {
    let converter = NotificationConverter::with_cwd(std::path::PathBuf::from("/test"));
    // Default request_id should be None even with cwd
    assert!(converter.request_id.is_none());
}

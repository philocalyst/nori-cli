use super::*;
use claude_code_agent_sdk::ACP_TOOL_PREFIX;

#[test]
fn test_acp_mcp_server_creation() {
    let server = AcpMcpServer::new("test", "1.0.0");
    assert_eq!(server.name, "test");
    assert_eq!(server.version, "1.0.0");
}

#[test]
fn test_tool_definitions_use_original_names() {
    let server = AcpMcpServer::new("acp", "1.0.0");
    // Tools should use original names WITHOUT prefix
    // MCP protocol automatically adds mcp__acp__ prefix when exposing to Claude
    assert!(server.tools.contains_key("Bash"));
    assert!(server.tools.contains_key("Read"));
    assert!(server.tools.contains_key("Write"));
    // Prefixed names should NOT be in the map
    assert!(!server.tools.contains_key("mcp__acp__Bash"));
    assert!(!server.tools.contains_key("mcp__acp__Read"));
}

#[test]
fn test_acp_tool_prefix_constant() {
    // Verify the SDK constant is correct
    assert_eq!(ACP_TOOL_PREFIX, "mcp__acp__");
}

// ============================================================
// MCP handle_message tests
// ============================================================

#[tokio::test]
async fn test_handle_message_initialize() {
    let server = AcpMcpServer::new("test-server", "1.0.0");

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "test-client",
                "version": "1.0.0"
            }
        }
    });

    let result = server.handle_message(request).await;
    assert!(result.is_ok(), "initialize should succeed");

    let response = result.unwrap();
    assert_eq!(response["protocolVersion"], "2024-11-05");
    assert!(response["capabilities"]["tools"].is_object());
    assert_eq!(response["serverInfo"]["name"], "test-server");
    assert_eq!(response["serverInfo"]["version"], "1.0.0");
}

#[tokio::test]
async fn test_handle_message_tools_list() {
    let server = AcpMcpServer::new("test-server", "1.0.0");

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });

    let result = server.handle_message(request).await;
    assert!(result.is_ok(), "tools/list should succeed");

    let response = result.unwrap();
    let tools = response["tools"].as_array().unwrap();
    assert!(!tools.is_empty(), "Should have tools");

    // Check that Bash tool is present
    let bash_tool = tools.iter().find(|t| t["name"] == "Bash");
    assert!(bash_tool.is_some(), "Bash tool should be present");

    // Check tool structure
    let bash = bash_tool.unwrap();
    assert!(bash["description"].is_string());
    assert!(bash["inputSchema"].is_object());
}

#[tokio::test]
async fn test_handle_message_tools_call_bash_fallback() {
    // Test Bash tool execution WITHOUT terminal client (fallback to direct execution)
    let server = AcpMcpServer::new("test-server", "1.0.0");

    // Set a valid cwd for the test
    server.set_cwd(std::env::temp_dir());
    server.set_session_id("test-session");

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "Bash",
            "arguments": {
                "command": "echo hello"
            }
        }
    });

    let result = server.handle_message(request).await;
    assert!(
        result.is_ok(),
        "tools/call should succeed: {:?}",
        result.err()
    );

    let response = result.unwrap();

    // Check response structure matches MCP tool result format
    assert!(
        response["content"].is_array(),
        "Response should have content array"
    );
    let content = response["content"].as_array().unwrap();
    assert!(!content.is_empty(), "Content should not be empty");

    // First content block should be text type
    assert_eq!(content[0]["type"], "text");
    assert!(content[0]["text"].is_string());

    // Should contain "hello" in output (direct execution of echo)
    let text = content[0]["text"].as_str().unwrap();
    assert!(
        text.contains("hello"),
        "Output should contain 'hello', got: {text}"
    );
}

#[tokio::test]
async fn test_handle_message_tools_call_with_tool_use_id() {
    // Test that tool_use_id is extracted from _meta
    let server = AcpMcpServer::new("test-server", "1.0.0");
    server.set_cwd(std::env::temp_dir());
    server.set_session_id("test-session");

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "Bash",
            "arguments": {
                "command": "echo test"
            },
            "_meta": {
                "claudecode/toolUseId": "toolu_123456"
            }
        }
    });

    let result = server.handle_message(request).await;
    assert!(result.is_ok(), "tools/call with tool_use_id should succeed");
}

#[tokio::test]
async fn test_handle_message_notifications_initialized() {
    let server = AcpMcpServer::new("test-server", "1.0.0");

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {}
    });

    let result = server.handle_message(request).await;
    assert!(result.is_ok(), "notifications/initialized should succeed");

    let response = result.unwrap();
    assert!(response.is_object(), "Should return empty object");
}

#[tokio::test]
async fn test_handle_message_unknown_method() {
    let server = AcpMcpServer::new("test-server", "1.0.0");

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "unknown/method",
        "params": {}
    });

    let result = server.handle_message(request).await;
    assert!(result.is_err(), "Unknown method should return error");
}

#[tokio::test]
async fn test_handle_message_read_tool() {
    let server = AcpMcpServer::new("test-server", "1.0.0");
    server.set_cwd(std::env::temp_dir());
    server.set_session_id("test-session");

    // Create a test file
    let test_file = std::env::temp_dir().join("test_read_tool.txt");
    std::fs::write(&test_file, "test content").unwrap();

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "tools/call",
        "params": {
            "name": "Read",
            "arguments": {
                "file_path": test_file.to_string_lossy()
            }
        }
    });

    let result = server.handle_message(request).await;
    assert!(result.is_ok(), "Read tool should succeed");

    let response = result.unwrap();
    let content = response["content"].as_array().unwrap();
    let text = content[0]["text"].as_str().unwrap();
    assert!(text.contains("test content"), "Should contain file content");

    // Clean up
    std::fs::remove_file(test_file).ok();
}

#[tokio::test]
async fn read_image_returns_mcp_image_content() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("image.png"), [137, 80, 78, 71]).unwrap();
    let server = AcpMcpServer::new("test", "1.0.0");
    server.set_cwd(dir.path().to_path_buf());
    server.set_session_id("test-session");
    let response = server
        .handle_message(serde_json::json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": {"name": "Read", "arguments": {"file_path": "image.png"}}
        }))
        .await
        .unwrap();
    pretty_assertions::assert_eq!(
        response["content"][1],
        serde_json::json!({
            "type": "image", "mimeType": "image/png", "data": "iVBORw=="
        })
    );
}

#[tokio::test]
async fn test_handle_message_missing_method() {
    let server = AcpMcpServer::new("test-server", "1.0.0");

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 7,
        "params": {}
    });

    let result = server.handle_message(request).await;
    assert!(result.is_err(), "Missing method should return error");
}

#[tokio::test]
async fn test_handle_message_tools_call_missing_tool_name() {
    let server = AcpMcpServer::new("test-server", "1.0.0");

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 8,
        "method": "tools/call",
        "params": {
            "arguments": {
                "command": "echo test"
            }
        }
    });

    let result = server.handle_message(request).await;
    assert!(result.is_err(), "Missing tool name should return error");
}

// ============================================================
// Tool execution tests (without terminal client - fallback mode)
// ============================================================

#[tokio::test]
async fn test_execute_bash_without_terminal_client() {
    let server = AcpMcpServer::new("test-server", "1.0.0");
    server.set_cwd(std::env::temp_dir());
    server.set_session_id("test-session");

    // Execute without terminal client (terminal_client is None by default)
    let result = server
        .execute_tool(
            "Bash",
            serde_json::json!({
                "command": "echo fallback_test"
            }),
            None,
        )
        .await;

    assert!(result.is_ok(), "Execute should succeed");
    let tool_result = result.unwrap();
    assert!(!tool_result.is_error, "Should not be an error");
    assert!(
        tool_result.content.contains("fallback_test"),
        "Output should contain 'fallback_test', got: {}",
        tool_result.content
    );
}

#[tokio::test]
async fn search_tools_remain_claude_cli_builtins() {
    let server = AcpMcpServer::new("test-server", "1.0.0");
    for name in ["Glob", "Grep"] {
        let result = server
            .execute_tool(name, serde_json::json!({"pattern": "example"}), None)
            .await
            .expect("tool lookup returns a result");
        assert!(result.is_error, "{name} must remain a CLI builtin");
        assert!(
            result.content.contains("Tool not found"),
            "{}",
            result.content
        );
    }
}

// ============================================================
// Error message propagation tests
// ============================================================

#[tokio::test]
async fn test_read_tool_error_propagation() {
    // Test that Read tool properly reports errors when file doesn't exist
    let server = AcpMcpServer::new("test-server", "1.0.0");
    server.set_cwd(std::env::temp_dir());
    server.set_session_id("test-session");

    // Try to read a non-existent file
    let result = server
        .execute_tool(
            "Read",
            serde_json::json!({
                "file_path": "/nonexistent/path/file.txt"
            }),
            None,
        )
        .await;

    assert!(result.is_ok(), "Read should return a result");
    let tool_result = result.unwrap();
    assert!(
        tool_result.is_error,
        "Reading non-existent file should be marked as error"
    );
    assert!(
        !tool_result.content.is_empty(),
        "Error should have a message"
    );
}

#[tokio::test]
async fn test_error_content_preparation_logic() {
    // Test the logic that prepares content for error notifications
    use super::ToolResult;

    // Simulate successful result
    let success_result = ToolResult::success("Operation completed".to_string());
    let content_success: Option<Vec<ToolCallContent>> = if success_result.is_error {
        Some(vec![success_result.content.into()])
    } else {
        None
    };
    assert!(
        content_success.is_none(),
        "Successful results should not include content in notification"
    );

    // Simulate error result
    let error_message = "File not found: /path/to/file.txt";
    let error_result = ToolResult::error(error_message.to_string());
    let content_error: Option<Vec<ToolCallContent>> = if error_result.is_error {
        Some(vec![error_result.content.into()])
    } else {
        None
    };
    assert!(
        content_error.is_some(),
        "Error results should include content in notification"
    );
    let content_vec = content_error.unwrap();
    assert_eq!(content_vec.len(), 1, "Should have one content item");

    // Verify the content contains the error message
    // ToolCallContent::Content(Content { content: ContentBlock::Text(TextContent) })
    match &content_vec[0] {
        ToolCallContent::Content(content) => match &content.content {
            nori_acp_kit::schema::ContentBlock::Text(text) => {
                assert_eq!(text.text, error_message, "Error message should match");
            }
            _ => panic!("Content block should be Text type"),
        },
        _ => panic!("Content should be Content type"),
    }
}

#[tokio::test]
async fn test_glob_tool_error_on_invalid_pattern() {
    // Test that Glob tool handles invalid patterns correctly
    let server = AcpMcpServer::new("test-server", "1.0.0");
    let cwd = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    server.set_cwd(&cwd);
    server.set_session_id("test-session");

    // Use an invalid glob pattern (unclosed bracket)
    let result = server
        .execute_tool(
            "Glob",
            serde_json::json!({
                "pattern": "src/**/[unclosed"
            }),
            None,
        )
        .await;

    // Result should be OK but may contain an error
    assert!(result.is_ok(), "Glob should return a result");
    let _tool_result = result.unwrap();
    // The glob implementation may not fail on invalid patterns, so we just
    // verify it returns without panicking
}

#[tokio::test]
async fn test_bash_error_with_nonexistent_command() {
    // Test that Bash tool properly reports errors for invalid commands
    let server = AcpMcpServer::new("test-server", "1.0.0");
    server.set_cwd(std::env::temp_dir());
    server.set_session_id("test-session");

    let result = server
        .execute_tool(
            "Bash",
            serde_json::json!({
                "command": "this_command_definitely_does_not_exist_12345"
            }),
            None,
        )
        .await;

    assert!(result.is_ok(), "Bash should return a result");
    let tool_result = result.unwrap();
    assert!(
        tool_result.is_error,
        "Non-existent command should be marked as error"
    );
}

// ============================================================
// Concurrency tests - verify deadlock fixes
// ============================================================

// Note: test_concurrent_set_session_id_and_execute was removed because
// OnceLock does not allow multiple sets. The test_lock_order_consistency test
// below verifies that lock ordering is consistent.

#[tokio::test]
async fn test_lock_order_consistency() {
    // Test that lock acquisition order is consistent across different code paths
    let server = std::sync::Arc::new(AcpMcpServer::new("test-server", "1.0.0"));
    let cwd = std::env::temp_dir();
    server.set_cwd(&cwd);
    server.set_session_id("test-session");

    // Create a barrier to synchronize tasks
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(3));
    let mut handles = vec![];

    // Task 1: Execute Read tool (uses create_tool_context)
    let server1 = server.clone();
    let barrier1 = barrier.clone();
    let handle1 = tokio::spawn(async move {
        barrier1.wait().await;
        drop(
            server1
                .execute_tool(
                    "Read",
                    serde_json::json!({"file_path": "/tmp/test.txt"}),
                    Some("tool-1"),
                )
                .await,
        );
    });

    // Task 2: Execute Bash tool (uses execute_bash_tool)
    let server2 = server.clone();
    let barrier2 = barrier.clone();
    let handle2 = tokio::spawn(async move {
        barrier2.wait().await;
        drop(
            server2
                .execute_tool(
                    "Bash",
                    serde_json::json!({"command": "echo test"}),
                    Some("tool-2"),
                )
                .await,
        );
    });

    // Task 3: Another Read tool
    let server3 = server.clone();
    let barrier3 = barrier.clone();
    let handle3 = tokio::spawn(async move {
        barrier3.wait().await;
        drop(
            server3
                .execute_tool(
                    "Read",
                    serde_json::json!({"file_path": "/tmp/test.txt"}),
                    Some("tool-3"),
                )
                .await,
        );
    });

    handles.push(handle1);
    handles.push(handle2);
    handles.push(handle3);

    // Wait for all tasks with a timeout
    let timeout_duration = std::time::Duration::from_secs(10);
    let start = std::time::Instant::now();

    for handle in handles {
        tokio::select! {
            () = tokio::time::sleep(timeout_duration) => {
                panic!("Test timed out - likely deadlock detected!");
            }
            result = handle => {
                result.unwrap();
            }
        }
    }

    let elapsed = start.elapsed();
    println!("All concurrent tasks completed in {elapsed:?}");

    // Should complete well within timeout
    assert!(elapsed < timeout_duration, "Possible deadlock detected");
}

#[tokio::test]
async fn test_cancel_callback_try_lock() {
    // Test that cancel_callback uses try_lock and doesn't cause deadlock
    let server = AcpMcpServer::new("test-server", "1.0.0");

    // Set a cancel callback
    let callback_called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let callback_flag = callback_called.clone();

    server
        .set_cancel_callback(move || {
            callback_flag.store(true, std::sync::atomic::Ordering::SeqCst);
        })
        .await;

    // Simulate MCP cancellation notification
    let cancellation_message = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": {
            "requestId": "test-request-123"
        }
    });

    // This should not deadlock even if callback is locked elsewhere
    let result = server.handle_message(cancellation_message).await;
    assert!(result.is_ok(), "Cancellation handling should succeed");
    assert!(callback_called.load(std::sync::atomic::Ordering::SeqCst));
}
